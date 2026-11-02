use super::*;
pub(super) fn spawn_poll_loop(
    shared: Shared,
    poll_ms: u64,
    poll_mode: crate::types::PollMode,
    run_id: u64,
) {
    let remote_ids = shared
        .lock()
        .unwrap()
        .nodes
        .iter()
        .filter_map(|(id, node)| node.remote.as_ref().map(|_| *id))
        .collect::<Vec<_>>();
    for remote_id in remote_ids {
        spawn_remote_poll_loop(shared.clone(), remote_id, run_id);
    }
    thread::spawn(move || {
        let mut next_node = 0;
        loop {
            let started = Instant::now();
            let (targets, auth_key, order_accounts, mixed_cluster) = {
                let mut state = shared.lock().unwrap();
                if !state.running || state.run_id != run_id {
                    break;
                }
                let mut exited = Vec::new();
                for (id, node) in state.nodes.iter_mut() {
                    if let Ok(Some(exit)) = node.child.try_wait() {
                        if node.state != "dead" {
                            exited.push(format!("Node {id} exited: {exit}"));
                        }
                        node.state = "dead".into();
                    }
                }
                for message in exited {
                    state.log(message);
                }
                let targets = state
                    .nodes
                    .iter()
                    .filter_map(|(id, node)| {
                        (node.state != "dead" && node.remote.is_none())
                            .then_some((*id, node.address?))
                    })
                    .collect::<Vec<_>>();
                let mut order_accounts = state
                    .agents
                    .values()
                    .filter_map(|agent| state.wallet.get(&agent.config.account_id).copied())
                    .collect::<Vec<_>>();
                order_accounts.extend_from_slice(&state.watched_order_accounts);
                if let Some(mm) = &state.market_maker {
                    if let Some(address) = state.wallet.get(&mm.config.account_id) {
                        order_accounts.push(*address);
                    }
                }
                order_accounts.sort_unstable();
                order_accounts.dedup();
                (
                    select_poll_targets(targets, poll_mode, &mut next_node),
                    state.auth_key,
                    order_accounts,
                    state.nodes.values().any(|node| node.remote.is_some()),
                )
            };
            let (tx, rx) = mpsc::channel();
            let count = targets.len();
            for (id, address) in targets {
                let order_accounts = order_accounts.clone();
                let tx = tx.clone();
                thread::spawn(move || {
                    let started = Instant::now();
                    let timeout = if mixed_cluster {
                        Duration::from_secs(1)
                    } else {
                        Duration::from_millis(400)
                    };
                    let _ = tx.send((
                        id,
                        started,
                        request_scoped_status(address, &auth_key, timeout, order_accounts),
                    ));
                });
            }
            drop(tx);
            for _ in 0..count {
                let Ok((id, started, result)) = rx.recv_timeout(if mixed_cluster {
                    Duration::from_millis(1200)
                } else {
                    Duration::from_millis(700)
                }) else {
                    break;
                };
                update_status(&shared, run_id, id, started, result);
            }
            let delay = Duration::from_millis(poll_ms.max(50));
            if started.elapsed() < delay {
                thread::sleep(delay - started.elapsed());
            }
        }
    });
}

fn spawn_remote_poll_loop(shared: Shared, remote_id: NodeId, run_id: u64) {
    thread::spawn(move || {
        loop {
            let (address, auth_key) = {
                let state = shared.lock().unwrap();
                if !state.running || state.run_id != run_id {
                    break;
                }
                let Some(node) = state.nodes.get(&remote_id) else {
                    break;
                };
                if node.state == "dead" {
                    break;
                }
                (node.address, state.auth_key)
            };
            let started = Instant::now();
            if let Some(address) = address {
                update_status(
                    &shared,
                    run_id,
                    remote_id,
                    started,
                    request_health(address, &auth_key, Duration::from_secs(5)),
                );
            }
            let period = Duration::from_secs(5);
            if started.elapsed() < period {
                thread::sleep(period - started.elapsed());
            }
        }
    });
}

pub(super) fn select_poll_targets(
    targets: Vec<(NodeId, SocketAddr)>,
    poll_mode: crate::types::PollMode,
    next_node: &mut usize,
) -> Vec<(NodeId, SocketAddr)> {
    match poll_mode {
        crate::types::PollMode::All => targets,
        crate::types::PollMode::Pinned => targets.into_iter().next().into_iter().collect(),
        crate::types::PollMode::RoundRobin => {
            if targets.is_empty() {
                return targets;
            }
            let target = targets[*next_node % targets.len()];
            *next_node = next_node.wrapping_add(1);
            vec![target]
        }
    }
}

pub(super) fn update_status(
    shared: &Shared,
    run_id: u64,
    node_id: NodeId,
    started: Instant,
    result: io::Result<NodeStatus>,
) {
    let now = Instant::now();
    let mut state = shared.lock().unwrap();
    if state.run_id != run_id {
        return;
    }
    let mixed_cluster = state.nodes.values().any(|node| node.remote.is_some());
    let Some(node) = state.nodes.get_mut(&node_id) else {
        return;
    };
    if let Ok(Some(exit)) = node.child.try_wait() {
        node.state = "dead".into();
        state.log(format!("Node {node_id} exited: {exit}"));
        return;
    }
    if node
        .last_poll_started
        .is_some_and(|previous| started < previous)
    {
        return;
    }
    node.last_poll_started = Some(started);
    match result {
        Ok(status) => {
            reconcile_load(&mut state, &status);
            if state
                .last_book_height
                .is_none_or(|height| status.chain_height > height)
            {
                if let Some(book) = status.order_books.iter().find(|book| book.market_id == 0) {
                    if let Some((bid, ask)) = book.bids.first().zip(book.asks.first()) {
                        if bid.price < ask.price {
                            let mid = (bid.price as f64 + ask.price as f64) / 2.0;
                            if state
                                .local_mid_history
                                .back()
                                .is_none_or(|previous| *previous != mid)
                            {
                                state.local_mid_history.push_back(mid);
                                while state.local_mid_history.len() > 101 {
                                    state.local_mid_history.pop_front();
                                }
                            }
                        }
                    }
                }
                state.last_book_height = Some(status.chain_height);
            }
            let Some(node) = state.nodes.get_mut(&node_id) else {
                return;
            };
            if let Some((then, txs, blocks)) = node.last_sample {
                if status.blocks_committed > blocks {
                    node.last_commit_seen = now;
                }
                let seconds = now.duration_since(then).as_secs_f64().max(0.000_001);
                node.tx_rate = status.transactions_committed.saturating_sub(txs) as f64 / seconds;
                node.block_rate = status.blocks_committed.saturating_sub(blocks) as f64 / seconds;
            }
            node.last_sample = Some((now, status.transactions_committed, status.blocks_committed));
            node.status = Some(status);
            node.status_failures = 0;
            node.status_rtt_ms = Some(
                now.duration_since(started)
                    .as_millis()
                    .min(u64::MAX as u128) as u64,
            );
            node.state = "ok".into();
            node.last_status_at_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64);
        }
        Err(_) => {
            let Some(node) = state.nodes.get_mut(&node_id) else {
                return;
            };
            let exit = node.child.try_wait().ok().flatten();
            node.status_failures = node.status_failures.saturating_add(1);
            node.state = if exit.is_some() {
                "dead"
            } else if node.paused_until.is_some_and(|until| now < until) {
                "paused"
            } else if mixed_cluster
                && node.status_failures < 3
                && node.last_sample.is_some_and(|(sample, _, _)| {
                    now.duration_since(sample)
                        < Duration::from_secs(if node.remote.is_some() { 15 } else { 5 })
                })
            {
                "ok"
            } else {
                "unresponsive"
            }
            .into();
            if let Some(exit) = exit {
                state.log(format!("Node {node_id} exited: {exit}"));
            }
        }
    }
}

pub(super) fn snapshot(shared: &Shared) -> ControlSnapshot {
    let mut state = shared.lock().unwrap();
    let now = Instant::now();
    let mixed_cluster = state.nodes.values().any(|node| node.remote.is_some());
    let mut exited = Vec::new();
    for (id, node) in state.nodes.iter_mut() {
        if let Ok(Some(exit)) = node.child.try_wait() {
            if node.state != "dead" {
                exited.push(format!("Node {id} exited: {exit}"));
            }
            node.state = "dead".into();
        }
    }
    for message in exited {
        state.log(message);
    }
    while state
        .submit_times
        .front()
        .is_some_and(|time| now.duration_since(*time) > Duration::from_secs(5))
    {
        state.submit_times.pop_front();
    }
    let mut tx_rates = Vec::new();
    let mut block_rates = Vec::new();
    let mut latencies = Vec::new();
    let mut heights = Vec::new();
    let mut views = BTreeSet::new();
    let mut primaries = BTreeSet::new();
    let mut tips = HashSet::new();
    let mut responsive = 0;
    for node in state.nodes.values() {
        if node.state != "ok"
            || node.last_sample.is_none_or(|(sample, _, _)| {
                now.duration_since(sample)
                    > Duration::from_secs(if node.remote.is_some() {
                        15
                    } else if mixed_cluster {
                        5
                    } else {
                        2
                    })
            })
        {
            continue;
        }
        let Some(status) = &node.status else { continue };
        responsive += 1;
        tx_rates.push(node.tx_rate);
        block_rates.push(node.block_rate);
        if status.average_commit_latency_ms > 0.0 {
            latencies.push(status.average_commit_latency_ms);
        }
        heights.push(status.chain_height);
        views.insert(status.current_view);
        primaries.insert(status.current_primary);
        tips.insert((
            status.chain_height,
            status
                .recent_blocks
                .first()
                .map(|b| b.digest.clone())
                .unwrap_or_default(),
        ));
    }
    let alive = state
        .nodes
        .values()
        .filter(|node| node.state != "dead")
        .count();
    let nodes = state
        .nodes
        .iter()
        .map(|(id, node)| node_snapshot(*id, node))
        .collect();
    let status = best_status(&state);
    let mut order_counts = HashMap::new();
    let mut nonces = HashMap::new();
    let mut balances = HashMap::new();
    if let Some(status) = status {
        for order in &status.open_orders {
            *order_counts
                .entry((order.account_id, order.market_id))
                .or_insert(0usize) += 1;
        }
        nonces.extend(
            status
                .account_nonces
                .iter()
                .map(|entry| (entry.account_id, entry.nonce)),
        );
        balances.extend(
            status
                .agent_balances
                .iter()
                .map(|entry| (entry.account_id, entry)),
        );
    }
    let market_maker = match state.market_maker.as_ref() {
        Some(mm) => MarketMakerSnapshot {
            running: mm.active,
            stopping: mm.stopping,
            account_id: Some(mm.config.account_id),
            refresh_ms: Some(mm.config.refresh_ms),
            last_update_id: mm.latest_depth.as_ref().map(|depth| depth.last_update_id),
            feed_age_ms: mm
                .feed_at
                .map(|at| now.duration_since(at).as_millis().min(u64::MAX as u128) as u64),
            desired_bids: mm.desired.bids.clone(),
            desired_asks: mm.desired.asks.clone(),
            own_open_orders: slot_address(&state, mm.config.account_id)
                .and_then(|address| order_counts.get(&(address, 0)).copied())
                .unwrap_or(0),
            pending: !mm.pending.is_empty(),
            last_error: mm.last_error.clone(),
        },
        None => MarketMakerSnapshot {
            running: false,
            stopping: false,
            account_id: None,
            refresh_ms: None,
            last_update_id: None,
            feed_age_ms: None,
            desired_bids: vec![],
            desired_asks: vec![],
            own_open_orders: 0,
            pending: false,
            last_error: None,
        },
    };
    ControlSnapshot {
        running: state.running,
        launching: state.launching,
        stopping: state.stopping,
        nodes,
        metrics: MetricsSnapshot {
            alive_nodes: alive,
            responsive_nodes: responsive,
            submitted: state.submitted,
            submit_tps: state.submit_times.len() as f64 / 5.0,
            commit_tps: median(tx_rates),
            blocks_per_second: median(block_rates),
            commit_latency_ms: median(latencies),
            height_min: heights.iter().min().copied(),
            height_max: heights.iter().max().copied(),
            views: views.into_iter().collect(),
            primaries: primaries.into_iter().collect(),
            chain_consistency: if responsive == 0 || responsive < alive {
                "unknown"
            } else if tips.len() == 1 {
                "consistent"
            } else {
                "split"
            }
            .into(),
        },
        load: LoadSnapshot {
            running: state.load.is_some(),
            kind: state.load.as_ref().map(|load| {
                match load.config.preset {
                    LoadPreset::RandomTransfers { .. } => "random transfers",
                    LoadPreset::RoundRobinTransfers { .. } => "rotating transfers",
                    LoadPreset::BookTaker { .. } => "book taker",
                }
                .to_string()
            }),
            tps: state.load.as_ref().map(|load| load.config.tps),
            pending_accounts: state.load.as_ref().map_or(0, |load| load.pending.len()),
            failed_accounts: state.load.as_ref().map_or_else(Vec::new, |load| {
                let mut accounts = load.failed.keys().copied().collect::<Vec<_>>();
                accounts.sort_unstable();
                accounts
            }),
        },
        market_maker,
        agents: state
            .agents
            .values()
            .map(|agent| AgentSnapshot {
                agent_id: agent.config.agent_id,
                account_id: agent.config.account_id,
                market_id: agent.config.market_id,
                strategy: agent.config.strategy.clone(),
                target: agent.config.target.clone(),
                wait_for_confirmation: agent.config.wait_for_confirmation,
                state: if agent.stopping {
                    "stopping"
                } else if agent.active {
                    "active"
                } else {
                    "stopped"
                }
                .into(),
                pending: agent.pending.len(),
                pending_action: agent.pending.front().and_then(|pending| {
                    match &pending.transaction {
                        Transaction::UpdateQuotes {
                            cancel,
                            reduce,
                            place,
                            ..
                        } => Some(format!(
                            "Quotes: {} cancel, {} reduce, {} place",
                            cancel.len(),
                            reduce.len(),
                            place.len()
                        )),
                        Transaction::ReduceOrder {
                            order_id,
                            remaining_quantity,
                            ..
                        } => Some(format!("Reduce {order_id} to {remaining_quantity}")),
                        _ => pending.action.map(|action| format!("{action:?}")),
                    }
                }),
                own_open_orders: slot_address(&state, agent.config.account_id)
                    .and_then(|address| {
                        order_counts
                            .get(&(address, agent.config.market_id))
                            .copied()
                    })
                    .unwrap_or(0),
                last_error: agent.last_error.clone(),
                confirmed_nonce: slot_address(&state, agent.config.account_id)
                    .and_then(|address| nonces.get(&address).copied()),
                balance: slot_address(&state, agent.config.account_id)
                    .and_then(|address| balances.get(&address).map(|balance| (*balance).clone())),
                feed_age_ms: agent
                    .config
                    .strategy
                    .needs_binance()
                    .then(|| {
                        state.feed_at.map(|at| {
                            now.duration_since(at).as_millis().min(u64::MAX as u128) as u64
                        })
                    })
                    .flatten(),
                group_id: agent.group_id,
            })
            .collect(),
        wallet: state
            .wallet
            .iter()
            .map(|(&label, &address)| WalletAccount { label, address })
            .collect(),
        generated_wallets: state
            .wallet
            .iter()
            .filter(|(_, address)| {
                state
                    .signing_manifest
                    .as_ref()
                    .is_some_and(|manifest| !manifest.accounts.contains(address))
            })
            .map(|(&label, &address)| WalletAccount { label, address })
            .collect(),
        claim_progress: state.claim_progress.clone(),
        logs: state.logs.iter().cloned().collect(),
        active_data_dir: state.active_data_dir.clone(),
    }
}

pub(super) fn node_snapshot(id: NodeId, node: &RuntimeNode) -> NodeRuntimeSnapshot {
    NodeRuntimeSnapshot {
        node_id: id,
        pid: node
            .status
            .as_ref()
            .map(|status| status.pid)
            .unwrap_or_else(|| node.child.id()),
        host_label: node.remote.as_ref().map_or_else(
            || "Local".to_string(),
            |remote| remote.profile.label.clone(),
        ),
        status_rtt_ms: node.status_rtt_ms,
        state: node.state.clone(),
        address: node.address.map(|address| address.to_string()),
        read_api_address: node.read_api_address.map(|address| address.to_string()),
        tx_rate: node.tx_rate,
        block_rate: node.block_rate,
        status: node.status.clone(),
        last_status_at_ms: node.last_status_at_ms,
    }
}

pub(super) fn median(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}
