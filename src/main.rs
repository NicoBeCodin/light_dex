use std::{env, net::SocketAddr, path::PathBuf};
use std::{fs::OpenOptions, sync::Arc};
use light_dex::crypto::{SessionManifest, read_secret};
use light_dex::{
    blockchain::BlockchainState,
    consensus::{Consensus, hotstuff::HotStuff, pbft::Pbft},
    network::Network,
    node::{Node, NodeId},
};

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let node_id: NodeId = arg_value(&args, "--node")
        .expect("missing --node")
        .parse()
        .expect("invalid --node");

    let parent_addr: SocketAddr = arg_value(&args, "--parent")
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
    let manifest_path = PathBuf::from(arg_value(&args, "--chain-config").expect("missing --chain-config"));
    let manifest = Arc::new(SessionManifest::load(&manifest_path).expect("invalid chain manifest"));
    let signing_key = read_secret(&PathBuf::from(arg_value(&args, "--validator-key")
        .expect("missing --validator-key"))).expect("invalid validator signing key");
    assert_eq!(signing_key.verifying_key(), manifest.verifier(true, node_id).expect("validator not in manifest"));
    let data_dir = PathBuf::from(arg_value(&args, "--data-dir").expect("missing --data-dir"));

    let auth_key = light_dex::message::hex_decode::<32>(&env::var("LIGHT_DEX_AUTH_KEY")
        .expect("missing cluster authentication key")).expect("invalid cluster authentication key");
    let mut network = Network::new(node_id, parent_addr, auth_key).expect("failed creating network");
    network.attach_signing(manifest.clone(), signing_key.clone()).expect("failed configuring peer authentication");

    let state = BlockchainState::load_signed(data_dir.join(format!("node-{node_id}.json")), manifest.clone())
        .expect("failed loading signed chain");

    let algorithm = arg_value(&args, "--consensus").unwrap_or_else(|| "pbft".into());
    assert_eq!(algorithm, manifest.consensus, "consensus algorithm differs from session manifest");
    let mut started = OpenOptions::new();
    started.write(true).create_new(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; started.mode(0o600); }
    started.open(data_dir.join(format!("validator-{node_id}.started")))
        .and_then(|file| file.sync_all())
        .and_then(|_| std::fs::File::open(&data_dir)?.sync_all())
        .expect("validator already started in this session; unsafe recovery refused");
    let mut consensus: Box<dyn Consensus> = match algorithm.as_str() {
        "pbft" => Box::new(Pbft::new(node_id, 1)),
        "hotstuff" => Box::new(HotStuff::new(node_id, 1)),
        _ => panic!("invalid --consensus: {algorithm}; expected pbft or hotstuff"),
    };
    consensus.set_timeout(timeout_ms);
    println!("Node {node_id} using {algorithm} consensus");

    let mut node = Node::new(
        node_id,
        pid,
        network,
        state,
        consensus,
        block_time_ms,
    )
    .expect("failed creating node");
    node.attach_signing(manifest, signing_key).expect("failed configuring validator signing");

    println!("Initialized node {node_id} pid={pid}");

    node.register_with_parent()
        .expect("failed registering with controller");

    node.discover_peers()
        .expect("failed discovering peers");

    node.wait_for_start()
        .expect("failed waiting for START");

    node.run().expect("node failed");
}
