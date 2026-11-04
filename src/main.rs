use light_dex::crypto::{SessionManifest, read_secret};
use light_dex::exchange::market::{BTC, USD};
use light_dex::oracle::{OraclePair, Venue};
use light_dex::{
    blockchain::BlockchainState,
    consensus::{
        Consensus,
        chained::ChainedHotStuff,
        fast::{FastConsensus, FastMode},
        hotstuff::HotStuff,
        pbft::Pbft,
    },
    network::Network,
    node::{Node, NodeId},
};
use std::collections::HashMap;
use std::{env, net::SocketAddr, path::PathBuf};
use std::{fs::OpenOptions, sync::Arc};

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.iter().any(|arg| arg == "--audit-journal") {
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let node_id: u64 = arg_value(&args, "--node")
                .ok_or("missing audit node ID")?
                .parse()?;
            let manifest = Arc::new(SessionManifest::load(&PathBuf::from(
                arg_value(&args, "--chain-config").ok_or("missing audit manifest")?,
            ))?);
            let data_dir = PathBuf::from(
                arg_value(&args, "--data-dir").ok_or("missing audit data directory")?,
            );
            let state = BlockchainState::load_signed(
                data_dir.join(format!("node-{node_id}.json")),
                manifest,
            )?;
            let audit = serde_json::json!({
                "node_id": node_id,
                "height":state.height(),
                "retained_from":state.base_height,
                "block_digests": state.blocks.iter().map(|block| light_dex::message::hex_encode(&block.to_digest())).collect::<Vec<_>>(),
                "state_root": light_dex::message::hex_encode(&state.exchange_state.state_digest()),
            });
            println!("JOURNAL_AUDIT {audit}");
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("journal audit failed: {error}");
            std::process::exit(1);
        }
        return;
    }

    let node_id: NodeId = arg_value(&args, "--node")
        .expect("missing --node")
        .parse()
        .expect("invalid --node");

    let peers_file = arg_value(&args, "--peers-file");
    let parent_addr: SocketAddr = arg_value(&args, "--parent")
        .or_else(|| peers_file.as_ref().map(|_| "127.0.0.1:0".into()))
        .expect("missing --parent")
        .parse()
        .expect("invalid --parent");

    let block_time_ms: u64 = arg_value(&args, "--block-time-ms")
        .unwrap_or_else(|| "500".to_string())
        .parse()
        .expect("invalid --block-time-ms");

    let timeout_ms: u64 = arg_value(&args, "--timeout-ms")
        .unwrap_or_else(|| "1500".to_string())
        .parse()
        .expect("invalid --timeout-ms");

    let pid = std::process::id();
    let manifest_path =
        PathBuf::from(arg_value(&args, "--chain-config").expect("missing --chain-config"));
    let manifest = Arc::new(SessionManifest::load(&manifest_path).expect("invalid chain manifest"));
    let static_peers = peers_file.map(|path| {
        let peers: std::collections::BTreeMap<NodeId, SocketAddr> = serde_json::from_slice(
            &std::fs::read(path).expect("cannot read static peer configuration"),
        )
        .expect("invalid static peer configuration");
        assert_eq!(
            peers.keys().copied().collect::<Vec<_>>(),
            manifest.validators.keys().copied().collect::<Vec<_>>(),
            "static peers must exactly match validator membership"
        );
        assert!(
            peers
                .values()
                .all(|address| address.port() != 0 && !address.ip().is_unspecified()),
            "static peers require concrete addresses and ports"
        );
        assert_eq!(
            peers
                .values()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            peers.len(),
            "static peer addresses must be distinct"
        );
        peers
    });
    let signing_key = read_secret(&PathBuf::from(
        arg_value(&args, "--validator-key").expect("missing --validator-key"),
    ))
    .expect("invalid validator signing key");
    assert_eq!(
        signing_key.verifying_key(),
        manifest
            .verifier(true, node_id)
            .expect("validator not in manifest")
    );
    let data_dir = PathBuf::from(arg_value(&args, "--data-dir").expect("missing --data-dir"));
    let recover = args.iter().any(|arg| arg == "--recover-hotstuff2");
    let _process_lease = if recover {
        assert_eq!(
            manifest.consensus, "hotstuff2",
            "persistent recovery currently supports HotStuff2 only"
        );
        assert!(
            static_peers.is_some(),
            "recovery requires a fixed peer configuration"
        );
        std::fs::create_dir_all(&data_dir).expect("cannot create validator data directory");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lease = options
            .open(data_dir.join(format!("validator-{node_id}.process")))
            .expect("cannot open process lease");
        lease.try_lock().expect("validator identity already in use");
        Some(lease)
    } else {
        None
    };

    let auth_key = light_dex::message::hex_decode::<32>(
        &env::var("LIGHT_DEX_AUTH_KEY").expect("missing cluster authentication key"),
    )
    .expect("invalid cluster authentication key");
    let listen_addr: SocketAddr = arg_value(&args, "--listen")
        .unwrap_or_else(|| "127.0.0.1:0".into())
        .parse()
        .expect("invalid --listen");
    let advertise_addr: Option<SocketAddr> =
        arg_value(&args, "--advertise").map(|value| value.parse().expect("invalid --advertise"));
    let mut network =
        Network::new_with_addresses(node_id, parent_addr, auth_key, listen_addr, advertise_addr)
            .expect("failed creating network");
    network
        .attach_signing(manifest.clone(), signing_key.clone())
        .expect("failed configuring peer authentication");

    let mut state = BlockchainState::load_signed(
        data_dir.join(format!("node-{node_id}.json")),
        manifest.clone(),
    )
    .expect("failed loading signed chain");

    let algorithm = arg_value(&args, "--consensus").unwrap_or_else(|| manifest.consensus.clone());
    assert_eq!(
        algorithm, manifest.consensus,
        "consensus algorithm differs from session manifest"
    );
    let mut started = OpenOptions::new();
    started.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        started.mode(0o600);
    }
    let marker = data_dir.join(format!("validator-{node_id}.started"));
    let safety_path = data_dir.join(format!("validator-{node_id}.safety"));
    if recover && marker.exists() {
        assert!(
            safety_path.exists(),
            "existing validator has no recoverable consensus safety state"
        );
    } else if !recover {
        started
            .open(&marker)
            .and_then(|file| file.sync_all())
            .and_then(|_| std::fs::File::open(&data_dir)?.sync_all())
            .expect("validator already started in this session; unsafe recovery refused");
    }
    let mut consensus: Box<dyn Consensus> = match algorithm.as_str() {
        "pbft" => Box::new(Pbft::new(node_id, 1)),
        "hotstuff" if manifest.chained_hotstuff => Box::new(ChainedHotStuff::new(node_id, 1)),
        "hotstuff" => Box::new(HotStuff::new(node_id, 1)),
        "hotstuff2" if recover => Box::new(
            FastConsensus::recover(&safety_path, node_id, &manifest, &mut state)
                .expect("failed recovering consensus safety state"),
        ),
        "hotstuff2" => Box::new(FastConsensus::new(FastMode::Hotstuff2, node_id, 1)),
        "votor" => Box::new(FastConsensus::new(FastMode::Votor, node_id, 1)),
        _ => {
            panic!("invalid --consensus: {algorithm}; expected pbft, hotstuff, hotstuff2, or votor")
        }
    };
    consensus.set_timeout(timeout_ms);
    if let Some(peers) = &static_peers {
        consensus.set_node_count(manifest.validators.len() as u64);
        for (&id, &address) in peers {
            if id != node_id {
                network.peers_address.insert(id, address);
            }
        }
    }
    println!(
        "Node {node_id} using {} consensus",
        if manifest.chained_hotstuff {
            "chained hotstuff"
        } else {
            algorithm.as_str()
        }
    );

    if recover {
        network.startup_quorum =
            Some(light_dex::consensus::quorum_size(manifest.validators.len() as u64) as usize);
        consensus
            .persist_before_actions(&[], state.height() as u64)
            .expect("cannot persist initial consensus state");
        if !marker.exists() {
            started
                .open(&marker)
                .and_then(|file| file.sync_all())
                .and_then(|_| std::fs::File::open(&data_dir)?.sync_all())
                .expect("cannot mark persistent validator started");
        }
    }

    let mut node = Node::new(node_id, pid, network, state, consensus, block_time_ms)
        .expect("failed creating node");
    if let Some(window) = arg_value(&args, "--tx-batch-window-ms") {
        node.set_batch_window_ms(window.parse().expect("invalid --tx-batch-window-ms"));
    }
    if let Some(timeout) = arg_value(&args, "--sync-timeout-ms") {
        node.set_sync_timeout(timeout.parse().expect("invalid --sync-timeout-ms"));
    }
    if let Some(timeout) = arg_value(&args, "--primary-probe-timeout-ms") {
        node.set_primary_probe_timeout(
            timeout.parse().expect("invalid --primary-probe-timeout-ms"),
        );
    }
    let btc_usd = OraclePair::new(BTC, USD);
    let mut feed_urls: HashMap<(OraclePair, Venue), String> = [
        (Venue::Binance, "--binance-feed-url"),
        (Venue::Okx, "--okx-feed-url"),
        (Venue::Coinbase, "--coinbase-feed-url"),
    ]
    .into_iter()
    .filter_map(|(venue, flag)| arg_value(&args, flag).map(|url| ((btc_usd, venue), url)))
    .collect();
    // Additional pairs can use direct venue WebSockets without changing node code.
    // Format: --oracle-feed BASE:QUOTE:VENUE:URL
    for values in args
        .windows(2)
        .filter(|values| values[0] == "--oracle-feed")
    {
        let parts = values[1].splitn(4, ':').collect::<Vec<_>>();
        assert_eq!(
            parts.len(),
            4,
            "expected BASE:QUOTE:VENUE:URL for --oracle-feed"
        );
        let pair = OraclePair::new(
            parts[0].parse().expect("invalid oracle base asset"),
            parts[1].parse().expect("invalid oracle quote asset"),
        );
        let venue = match parts[2] {
            "binance" => Venue::Binance,
            "okx" => Venue::Okx,
            "coinbase" => Venue::Coinbase,
            _ => panic!("invalid oracle venue"),
        };
        feed_urls.insert((pair, venue), parts[3].to_string());
    }
    node.configure_market_data(feed_urls);
    node.attach_signing(manifest, signing_key)
        .expect("failed configuring validator signing");

    println!("Initialized node {node_id} pid={pid}");

    if static_peers.is_none() {
        node.register_with_parent()
            .expect("failed registering with controller");
        node.discover_peers().expect("failed discovering peers");
        node.wait_for_start().expect("failed waiting for START");
    }

    node.run().expect("node failed");
}
