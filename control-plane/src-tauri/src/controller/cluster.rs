use super::*;
pub(super) fn launch_cluster(shared: Shared, config: LaunchConfig, run_id: u64) -> io::Result<()> {
    let auth_key = shared.lock().unwrap().auth_key;
    let project = PathBuf::from(&config.project_dir).canonicalize()?;
    let placements = placement_plan(&project, &config)?;
    for (profile, _) in placements.hosts.values() {
        log(
            &shared,
            format!("Remote preflight: {}", preflight(profile)?),
        );
    }
    let requested_data = PathBuf::from(&config.data_dir);
    let base_data = if requested_data.is_absolute() {
        requested_data
    } else {
        project.join(requested_data)
    };
    let data_dir = if config.recover_existing {
        return Err(io::Error::other(
            "live recovery is unavailable until durable vote and lock state are persisted; use offline replay to inspect old sessions",
        ));
    } else {
        let suffix = format!(
            "run-{}-{:08x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_millis(),
            rand::random::<u32>()
        );
        let path = base_data.join(suffix);
        std::fs::create_dir_all(&path)?;
        std::fs::write(path.join("consensus.txt"), config.consensus.as_str())?;
        path
    };
    let data_dir = data_dir.canonicalize()?;
    let manifest = Arc::new(create_session_with_mode(
        &data_dir,
        config.consensus.as_str(),
        config.node_count,
        FaucetTerms {
            max_claims: config.claim_limit,
            usd_per_claim: config
                .claim_usd
                .checked_mul(light_dex::exchange::market::USD_ATOMS)
                .ok_or_else(|| io::Error::other("USD faucet amount overflows atomic units"))?,
            btc_per_claim: config
                .claim_btc
                .checked_mul(light_dex::exchange::market::BTC_ATOMS)
                .ok_or_else(|| io::Error::other("BTC faucet amount overflows atomic units"))?,
        },
        config.auto_view_change_blocks,
        config.consensus.as_str() == "hotstuff" && config.chained_hotstuff,
    )?);
    let wallet: BTreeMap<u64, Address> =
        serde_json::from_slice(&std::fs::read(data_dir.join("wallet.json"))?)?;
    let mut account_keys = BTreeMap::new();
    for (&id, &address) in &wallet {
        let key = read_secret(&data_dir.join("keys").join(format!("account-{id}.key")))?;
        if Address::from_public_key(&key.verifying_key()) != address {
            return Err(io::Error::other("wallet key/address mismatch"));
        }
        account_keys.insert(id, key);
    }
    {
        let mut state = shared.lock().unwrap();
        state.active_data_dir = Some(data_dir.display().to_string());
        state.signing_manifest = Some(manifest);
        state.account_keys = account_keys;
        state.wallet_slots = wallet
            .iter()
            .map(|(slot, address)| (*address, *slot))
            .collect();
        state.wallet = wallet;
        state.log(format!("Node data directory: {}", data_dir.display()));
    }
    let requested_binary = PathBuf::from(&config.binary_path);
    let binary = if requested_binary.is_absolute() {
        requested_binary
    } else {
        project.join(requested_binary)
    };
    if config.build_before_launch {
        let release = binary == project.join("target/release/light_dex");
        log(
            &shared,
            if release {
                "Running cargo build --release --bin light_dex..."
            } else {
                "Running cargo build --bin light_dex..."
            },
        );
        let mut command = Command::new("cargo");
        command.args(["build", "--bin", "light_dex"]);
        if release {
            command.arg("--release");
        }
        let output = command
            .current_dir(&project)
            .env("LIGHT_DEX_AUTH_KEY", hex_encode(&auth_key))
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }
    }
    if !binary.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("node binary not found: {}", binary.display()),
        ));
    }

    let mut specs = Vec::new();
    for base in [
        light_dex::exchange::market::BTC,
        light_dex::exchange::market::ETH,
        light_dex::exchange::market::BNB,
        light_dex::exchange::market::XRP,
        light_dex::exchange::market::SOL,
    ] {
        let pair = light_dex::oracle::OraclePair::new(base, light_dex::exchange::market::USD);
        for venue in light_dex::oracle::Venue::ALL {
            let Some(symbol) = light_dex::oracle::config::venue_symbol(pair, venue) else {
                continue;
            };
            let fallback = match venue {
                light_dex::oracle::Venue::Binance => format!(
                    "wss://data-stream.binance.vision/ws/{}@bookTicker",
                    symbol.to_ascii_lowercase()
                ),
                light_dex::oracle::Venue::Okx => "wss://wseea.okx.com/ws/v5/public".into(),
                light_dex::oracle::Venue::Coinbase => "wss://ws-feed.exchange.coinbase.com".into(),
            };
            let override_key = if base == light_dex::exchange::market::BTC {
                format!("LIGHT_DEX_{}_WS_URL", venue.as_str().to_ascii_uppercase())
            } else {
                format!(
                    "LIGHT_DEX_{}_{}_WS_URL",
                    symbol.replace('-', "_").to_ascii_uppercase(),
                    venue.as_str().to_ascii_uppercase()
                )
            };
            specs.push(light_dex_market_data_hub::FeedSpec {
                base_asset: base,
                venue: venue.as_str().into(),
                symbol: symbol.into(),
                upstream_url: std::env::var(override_key).unwrap_or(fallback),
            });
        }
    }
    let feed_specs = specs.clone();
    let hub = HubHandle::start_many("127.0.0.1".parse().unwrap(), specs)?;
    let binance_url = hub.url("binance").expect("Binance relay URL");
    let okx_url = hub.url("okx").expect("OKX relay URL");
    let coinbase_url = hub.url("coinbase").expect("Coinbase relay URL");
    let mut extra_feeds = Vec::new();
    for base in [
        light_dex::exchange::market::ETH,
        light_dex::exchange::market::BNB,
        light_dex::exchange::market::XRP,
        light_dex::exchange::market::SOL,
    ] {
        for venue in light_dex::oracle::Venue::ALL {
            if let Some(url) = hub.url_for(base, venue.as_str()) {
                extra_feeds.push(format!(
                    "{base}:{}:{}:{url}",
                    light_dex::exchange::market::USD,
                    venue.as_str()
                ));
            }
        }
    }
    let local_feeds = feed_specs
        .iter()
        .map(|feed| {
            let url = hub.url_for(feed.base_asset, &feed.venue).expect("feed URL");
            (feed.base_asset, feed.venue.clone(), url)
        })
        .collect::<Vec<_>>();
    {
        let mut state = shared.lock().unwrap();
        if state.run_id != run_id {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "launch cancelled",
            ));
        }
        state.market_data_hub = Some(hub);
        state.log("Market data relay: BTC, ETH, BNB, XRP, SOL feeds started (one upstream per pair/venue)");
    }

    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let parent_address = listener.local_addr()?;
    log(&shared, format!("Bootstrap listening at {parent_address}"));

    let local_tailnet_address = placements
        .hosts
        .values()
        .next()
        .map(|(profile, _)| local_tailnet_ip(profile))
        .transpose()?
        .flatten();
    let mut local_peer_ports = Vec::new();
    if let Some((profile, _)) = placements.hosts.values().next() {
        let mut attempts = 0;
        while local_peer_ports.len() < placements.local_count as usize {
            let bind_ip = local_tailnet_address
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "127.0.0.1".into());
            let port = TcpListener::bind((bind_ip.as_str(), 0))?
                .local_addr()?
                .port();
            if port != profile.peer_port
                && port != profile.bootstrap_port
                && !(profile.feed_port_start as u32..profile.feed_port_start as u32 + 15)
                    .contains(&(port as u32))
                && !local_peer_ports.contains(&port)
            {
                local_peer_ports.push(port);
            }
            attempts += 1;
            if attempts > 100 {
                return Err(io::Error::other(
                    "could not reserve distinct local peer ports",
                ));
            }
        }
    }

    for (id, (profile, node_ids)) in &placements.hosts {
        if shared.lock().unwrap().run_id != run_id {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "launch cancelled",
            ));
        }
        let host = stage_host(
            profile.clone(),
            &binary,
            &data_dir,
            &auth_key,
            node_ids.clone(),
            parent_address,
            &local_feeds,
            &local_peer_ports,
        )?;
        shared.lock().unwrap().remote_hosts.insert(id.clone(), host);
        log(
            &shared,
            format!("Staged {} validators on {}", node_ids.len(), profile.label),
        );
    }

    for node_id in 0..config.node_count {
        if shared.lock().unwrap().run_id != run_id {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "launch cancelled",
            ));
        }
        let remote_location = placements.nodes.get(&node_id);
        let mut child_command = Command::new(&binary);
        child_command
            .args([
                "--node",
                &node_id.to_string(),
                "--parent",
                &parent_address.to_string(),
                "--block-time-ms",
                &config.block_time_ms.to_string(),
                "--tx-batch-window-ms",
                &config.tx_batch_window_ms.to_string(),
                "--timeout-ms",
                &config.timeout_ms.to_string(),
                "--binance-feed-url",
                &binance_url,
                "--okx-feed-url",
                &okx_url,
                "--coinbase-feed-url",
                &coinbase_url,
                "--consensus",
                config.consensus.as_str(),
                "--data-dir",
                &data_dir.display().to_string(),
                "--chain-config",
                &data_dir.join("manifest.json").display().to_string(),
                "--validator-key",
                &data_dir
                    .join("keys")
                    .join(format!("validator-{node_id}.key"))
                    .display()
                    .to_string(),
            ])
            .current_dir(&project)
            .env("LIGHT_DEX_AUTH_KEY", hex_encode(&auth_key));
        if remote_location.is_none() {
            if let Some(port) = local_peer_ports.get(node_id as usize) {
                let bind_ip = local_tailnet_address
                    .map(|ip| ip.to_string())
                    .unwrap_or_else(|| "127.0.0.1".into());
                child_command.args(["--listen", &format!("{bind_ip}:{port}")]);
                child_command.args(["--sync-timeout-ms", "10000"]);
                child_command.args(["--primary-probe-timeout-ms", "1500"]);
            }
        }
        for feed in &extra_feeds {
            child_command.args(["--oracle-feed", feed]);
        }
        let (remote, mut child) = if let Some((host_id, peer_port)) = remote_location {
            let host = shared
                .lock()
                .unwrap()
                .remote_hosts
                .get(host_id)
                .unwrap()
                .info
                .clone();
            let (remote, child, _address) =
                stage_node(&host, &data_dir, node_id, *peer_port, &local_feeds, &config)?;
            log(
                &shared,
                format!(
                    "Node {node_id} launched on {} with {} peer transport",
                    remote.profile.label,
                    if remote.profile.tailnet_ip.is_some() {
                        "Tailscale"
                    } else {
                        "SSH"
                    }
                ),
            );
            (Some(remote), child)
        } else {
            (
                None,
                child_command
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?,
            )
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let mut state = shared.lock().unwrap();
        if state.run_id != run_id {
            drop(state);
            if let Some(remote) = &remote {
                let _ = terminate(remote);
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "launch cancelled",
            ));
        }
        state.nodes.insert(
            node_id,
            RuntimeNode {
                child,
                remote,
                address: None,
                read_api_address: None,
                status: None,
                state: "starting".into(),
                last_sample: None,
                last_poll_started: None,
                last_status_at_ms: None,
                status_rtt_ms: None,
                status_failures: 0,
                last_commit_seen: Instant::now(),
                tx_rate: 0.0,
                block_rate: 0.0,
                paused_until: None,
            },
        );
        drop(state);
        if let Some(reader) = stdout {
            spawn_output_reader(
                node_id,
                reader,
                shared.clone(),
                data_dir.join(format!("node-{node_id}.stdout.log")),
            );
        }
        if let Some(reader) = stderr {
            spawn_output_reader(
                node_id,
                reader,
                shared.clone(),
                data_dir.join(format!("node-{node_id}.stderr.log")),
            );
        }
    }

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut addresses = BTreeMap::new();
    while addresses.len() < config.node_count as usize {
        let (stream, _) = accept_until(&listener, deadline)?;
        let parts: Vec<_> = read_line(&stream)?
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        if parts.len() != 3 || parts[0] != "REGISTER" {
            continue;
        }
        let node_id = parts[1]
            .parse::<NodeId>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid node ID"))?;
        let address = parts[2]
            .parse::<SocketAddr>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid node address"))?;
        addresses.insert(node_id, address);
        let mut state = shared.lock().unwrap();
        if let Some(node) = state.nodes.get_mut(&node_id) {
            node.address = Some(address);
        }
        state.log(format!("Registered node {node_id} at {address}"));
    }
    for (node_id, address) in &addresses {
        log(
            &shared,
            format!("Sending peer list to node {node_id} at {address}"),
        );
        let mut stream = connect_bootstrap_peer(address, Instant::now() + Duration::from_secs(30))
            .map_err(|error| {
                io::Error::other(format!(
                    "peer bootstrap connection to node {node_id} at {address}: {error}"
                ))
            })?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        for (peer_id, peer_address) in &addresses {
            if node_id != peer_id {
                writeln!(stream, "REGISTER {peer_id} {peer_address}")?;
            }
        }
        writeln!(stream, "DONE")?;
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ready = HashSet::new();
    let mut ready_streams = Vec::new();
    while ready.len() < config.node_count as usize {
        let (stream, _) = accept_until(&listener, deadline)?;
        let parts: Vec<_> = read_line(&stream)?
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        if parts.len() == 2 && parts[0] == "READY" {
            let node_id = parts[1]
                .parse::<NodeId>()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid READY node"))?;
            if ready.insert(node_id) {
                ready_streams.push(stream);
                if let Ok(mut state) = shared.lock() {
                    if let Some(node) = state.nodes.get_mut(&node_id) {
                        node.state = "ready".into();
                    }
                    state.log(format!("Node {node_id} ready"));
                }
            }
        }
    }
    for mut stream in ready_streams {
        stream.write_all(b"START\n")?;
    }
    {
        let mut state = shared.lock().unwrap();
        if state.run_id != run_id {
            return Ok(());
        }
        state.running = true;
        state.launching = false;
        state.log(format!(
            "Simulation started with {} nodes",
            config.node_count
        ));
    }
    spawn_poll_loop(shared.clone(), config.poll_ms, config.poll_mode, run_id);
    let remote_hosts = shared
        .lock()
        .unwrap()
        .remote_hosts
        .values()
        .map(|host| host.info.clone())
        .collect::<Vec<_>>();
    for host in remote_hosts {
        spawn_resource_guard(shared.clone(), host, run_id);
    }
    spawn_agent_loop(shared, run_id);
    Ok(())
}

fn connect_bootstrap_peer(address: &SocketAddr, deadline: Instant) -> io::Result<TcpStream> {
    loop {
        match TcpStream::connect_timeout(address, Duration::from_secs(3)) {
            Ok(stream) => return Ok(stream),
            Err(_) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(200));
            }
            Err(error) => return Err(error),
        }
    }
}

pub(super) fn accept_until(
    listener: &TcpListener,
    deadline: Instant,
) -> io::Result<(TcpStream, SocketAddr)> {
    loop {
        match listener.accept() {
            Ok(stream) => return Ok(stream),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "bootstrap timed out",
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

pub(super) fn stop_cluster(shared: Shared, force: bool) {
    let (addresses, auth_key, stopped_run_id) = {
        let mut state = shared.lock().unwrap();
        if force && state.stopping {
            return;
        }
        if !state.stopping {
            state.stopping = true;
            state.run_id += 1;
        }
        state.running = false;
        state.launching = false;
        state.load = None;
        state.market_maker = None;
        state.agents.clear();
        let addresses = state
            .nodes
            .values()
            .filter_map(|node| node.address)
            .collect::<Vec<_>>();
        (addresses, state.auth_key, state.run_id)
    };
    if !force {
        for address in addresses {
            let _ = send_message(
                address,
                &Message::Control(ControlMessage::Shutdown),
                Duration::from_millis(200),
                &auth_key,
            );
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let all_exited = {
                let mut state = shared.lock().unwrap();
                state
                    .nodes
                    .values_mut()
                    .all(|node| matches!(node.child.try_wait(), Ok(Some(_))))
            };
            if all_exited {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    let remotes = {
        let state = shared.lock().unwrap();
        state
            .nodes
            .values()
            .filter_map(|node| node.remote.clone())
            .collect::<Vec<_>>()
    };
    let mut retained_hosts = HashSet::new();
    for remote in &remotes {
        if let Err(error) = terminate(remote) {
            retained_hosts.insert(remote.profile.id.clone());
            log(
                &shared,
                format!(
                    "Remote validator termination failed on {}: {error}",
                    remote.profile.label
                ),
            );
        }
    }
    let mut state = shared.lock().unwrap();
    for node in state.nodes.values_mut() {
        if !matches!(node.child.try_wait(), Ok(Some(_))) {
            let _ = node.child.kill();
            let _ = node.child.wait();
        }
        node.state = "dead".into();
    }
    drop(state);
    let (hosts, local_dir) = {
        let mut state = shared.lock().unwrap();
        (
            std::mem::take(&mut state.remote_hosts),
            state.active_data_dir.clone(),
        )
    };
    let mut host_infos = Vec::new();
    for (id, mut host) in hosts {
        let _ = host.bootstrap_tunnel.kill();
        let _ = host.bootstrap_tunnel.wait();
        let _ = host.feed_tunnel.kill();
        let _ = host.feed_tunnel.wait();
        host_infos.push((id, host.info));
    }
    let mut state = shared.lock().unwrap();
    state.account_keys.clear();
    state.market_data_hub = None;
    state.pending_airdrops.clear();
    state.signing_manifest = None;
    state.auth_key = [0; 32];
    state.stopping = false;
    let summary_only = std::env::var("LIGHT_DEX_REMOTE_AUDIT_SUMMARY").as_deref() == Ok("1");
    state.log(if summary_only {
        "Simulation stopped; remote journals are being replayed for audit in the background"
    } else {
        "Simulation stopped; remote journals are being archived in the background"
    });
    drop(state);

    // Journal transfer may take minutes over a remote link. The validators
    // and their fixed-port tunnels are already gone, so a new signed session
    // can launch while archival of this run's unique directories continues.
    if !remotes.is_empty() || !host_infos.is_empty() {
        thread::spawn(move || {
            // Collect independent validator journals concurrently. A Tokyo
            // transfer must not consume the timeout budget for IdeaPad or
            // the second Tokyo validator before either is even started.
            let transfers = remotes
                .iter()
                .filter_map(|remote| {
                    local_dir.as_ref().map(|dir| {
                        let remote = remote.clone();
                        let dir = PathBuf::from(dir);
                        let worker_remote = remote.clone();
                        (
                            remote,
                            thread::spawn(move || {
                                if summary_only {
                                    retrieve_journal_audit(&worker_remote, &dir)
                                } else {
                                    retrieve_journal(&worker_remote, &dir)
                                }
                            }),
                        )
                    })
                })
                .collect::<Vec<_>>();
            for (remote, transfer) in transfers {
                match transfer.join() {
                    Ok(Ok(())) => log_stopped_run(
                        &shared,
                        stopped_run_id,
                        local_dir.as_deref(),
                        format!(
                            "{} signed journal for node {} from {}",
                            if summary_only { "Audited" } else { "Retrieved" },
                            remote.node_id,
                            remote.profile.label
                        ),
                    ),
                    Ok(Err(error)) => {
                        retained_hosts.insert(remote.profile.id.clone());
                        log_stopped_run(
                            &shared,
                            stopped_run_id,
                            local_dir.as_deref(),
                            format!(
                                "Remote journal unavailable for node {}; session retained on {}: {error}",
                                remote.node_id, remote.profile.label
                            ),
                        );
                    }
                    Err(_) => {
                        retained_hosts.insert(remote.profile.id.clone());
                        log_stopped_run(
                            &shared,
                            stopped_run_id,
                            local_dir.as_deref(),
                            format!(
                                "Remote journal worker failed for node {}; session retained on {}",
                                remote.node_id, remote.profile.label
                            ),
                        );
                    }
                }
            }
            for (id, host) in host_infos {
                if retained_hosts.contains(&id) {
                    log_stopped_run(
                        &shared,
                        stopped_run_id,
                        local_dir.as_deref(),
                        format!("Remote session retained on {}", host.profile.label),
                    );
                } else if let Err(error) = cleanup_host(&host) {
                    log_stopped_run(
                        &shared,
                        stopped_run_id,
                        local_dir.as_deref(),
                        format!("Remote cleanup failed on {}: {error}", host.profile.label),
                    );
                }
            }
            log_stopped_run(
                &shared,
                stopped_run_id,
                local_dir.as_deref(),
                "Remote archival complete".into(),
            );
        });
    }
}

fn log_stopped_run(shared: &Shared, run_id: u64, local_dir: Option<&str>, message: String) {
    if let Some(dir) = local_dir {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(PathBuf::from(dir).join("remote-archive.log"))
        {
            let _ = writeln!(file, "{message}");
        }
    }
    let mut state = shared.lock().unwrap();
    if state.run_id == run_id && !state.running && !state.launching {
        state.log(message);
    }
}
