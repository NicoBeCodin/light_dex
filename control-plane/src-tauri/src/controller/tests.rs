use super::market_maker::market_maker_step;
use super::*;
use light_dex::node::{AccountNonce, TransactionOutcome};

#[test]
fn node_oracle_adapter_reads_three_raw_relay_feeds() {
    use light_dex::oracle::{Venue, run_feed};
    use std::sync::atomic::{AtomicBool, Ordering};

    let payloads = [
        r#"{"s":"BTCUSDT","b":"100","a":"101"}"#,
        r#"{"data":[{"instId":"BTC-USDT","bidPx":"110","askPx":"111"}]}"#,
        r#"{"type":"ticker","product_id":"BTC-USD","best_bid":"120","best_ask":"121"}"#,
    ];
    let mut upstream = Vec::new();
    for (index, payload) in payloads.into_iter().enumerate() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        upstream.push(format!("ws://{}", listener.local_addr().unwrap()));
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            if index != 0 {
                let _ = socket.read().unwrap();
            }
            socket
                .send(tungstenite::Message::Text(payload.into()))
                .unwrap();
            thread::sleep(Duration::from_millis(500));
        });
    }
    let hub = HubHandle::start("127.0.0.1".parse().unwrap(), upstream.try_into().unwrap()).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let mut readers = Vec::new();
    for venue in Venue::ALL {
        let url = hub.url(venue.as_str()).unwrap();
        let stop = stop.clone();
        let tx = tx.clone();
        readers.push(thread::spawn(move || {
            let _ = run_feed(venue, &url, &stop, |quote| {
                let _ = tx.send(quote);
            });
        }));
    }
    let mut seen = HashSet::new();
    for _ in 0..3 {
        seen.insert(rx.recv_timeout(Duration::from_secs(5)).unwrap().venue);
    }
    assert_eq!(seen, Venue::ALL.into_iter().collect());
    stop.store(true, Ordering::Relaxed);
    for reader in readers {
        reader.join().unwrap();
    }
}

#[test]
fn participant_nonce_pipeline_fills_gaps_without_reusing_a_nonce() {
    let pending_at = |nonce| {
        let transaction = Transaction::ClaimAirdrop {
            account_id: 0.into(),
            nonce,
        };
        PendingAgentTransaction {
            digest: hex_encode(&transaction.to_digest()),
            transaction,
            action: None,
            last_sent: Instant::now(),
        }
    };
    let mut pending = VecDeque::from([pending_at(5), pending_at(6), pending_at(7)]);
    assert_eq!(next_agent_nonce(5, &pending), Some(8));
    pending.remove(1);
    assert_eq!(next_agent_nonce(5, &pending), Some(6));
    pending.insert(1, pending_at(6));
    assert_eq!(next_agent_nonce(6, &pending), Some(8));
    pending.retain(|tx| tx.transaction.nonce() >= 7);
    assert_eq!(next_agent_nonce(7, &pending), Some(8));
}

#[test]
fn polling_mode_selects_all_or_rotates_one_node() {
    let targets = (0..3)
        .map(|id| (id, SocketAddr::from(([127, 0, 0, 1], 9000 + id as u16))))
        .collect::<Vec<_>>();
    let mut next = 0;
    assert_eq!(
        select_poll_targets(targets.clone(), crate::types::PollMode::All, &mut next),
        targets
    );
    assert_eq!(next, 0);
    for _ in 0..4 {
        let selected =
            select_poll_targets(targets.clone(), crate::types::PollMode::Pinned, &mut next);
        assert_eq!(selected[0].0, 0);
    }
    let after_failure = select_poll_targets(
        targets[1..].to_vec(),
        crate::types::PollMode::Pinned,
        &mut next,
    );
    assert_eq!(after_failure[0].0, 1);
    for expected in [0, 1, 2, 0] {
        let selected = select_poll_targets(
            targets.clone(),
            crate::types::PollMode::RoundRobin,
            &mut next,
        );
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].0, expected);
    }
    assert!(
        select_poll_targets(Vec::new(), crate::types::PollMode::RoundRobin, &mut next).is_empty()
    );
}

#[test]
fn benchmark_future_nonce_window_does_not_change_ui_rule() {
    assert!(nonce_in_window(5, 5, 0));
    assert!(!nonce_in_window(5, 6, 0));
    assert!(nonce_in_window(5, 6, 32));
    assert!(nonce_in_window(5, 37, 32));
    assert!(!nonce_in_window(5, 38, 32));
    assert!(!nonce_in_window(5, 4, 32));
    let account = light_dex::node::AccountNonce {
        account_id: 0.into(),
        nonce: 5,
        used_nonces: [0b0010; 2],
    };
    assert!(benchmark_nonce_allowed(&account, 4, 32));
    assert!(!benchmark_nonce_allowed(&account, 3, 32));
    assert!(!benchmark_nonce_allowed(&account, 4, 0));
}

fn status() -> NodeStatus {
    NodeStatus {
        node_id: 0,
        pid: 0,
        chain_id: String::new(),
        signing_enabled: false,
        authentication_rejections: 0,
        faucet_remaining_claims: None,
        market_data: vec![],
        provisional_index_usd_e8: None,
        oracle_prices: vec![],
        perp_risk: vec![],
        perp_positions: vec![],
        funding: vec![],
        insurance_usd_atoms: 0,
        bad_debt_usd_atoms: 0,
        network_ready: true,
        peer_count: 0,
        expected_peers: 0,
        peer_queued_bytes: 0,
        peer_write_timeouts: 0,
        peer_write_errors: 0,
        peer_queue_full: 0,
        peer_truncated_frames: 0,
        peer_reconnects: 0,
        chain_height: 1,
        latest_sequence: Some(0),
        mempool_size: 0,
        seen_transactions: 0,
        current_view: 0,
        current_primary: 0,
        next_sequence: 1,
        active_proposal: false,
        consensus_started: true,
        transactions_received: 0,
        blocks_committed: 1,
        transactions_committed: 0,
        order_actions_committed: 0,
        total_open_orders: 0,
        open_orders_complete: true,
        transactions_failed: 0,
        trades_committed: 0,
        average_commit_latency_ms: 0.0,
        last_commit_latency_ms: None,
        recent_commit_latency_ms: vec![],
        catch_up_events: 0,
        blocks_caught_up: 0,
        branch_sync_attempts: 0,
        branch_sync_failures: 0,
        proposal_parent_misses: 0,
        recent_blocks: vec![],
        markets: vec![light_dex::node::MarketSnapshot {
            id: 0,
            config: light_dex::exchange::market_config::MarketConfig {
                base_asset: light_dex::exchange::market::BTC,
                quote_asset: light_dex::exchange::market::USD,
                price: light_dex::exchange::market_config::DecimalStep {
                    decimals: 0,
                    step: 1,
                },
                quantity: light_dex::exchange::market_config::DecimalStep {
                    decimals: 0,
                    step: 1,
                },
                market_type: light_dex::exchange::market_config::MarketType::Spot,
                fees: Default::default(),
                maintenance_margin_bps: 0,
                funding_interval_updates: 0,
                max_funding_bps: 0,
                max_liquidation_slippage_bps: 0,
            },
        }],
        account_nonces: vec![
            AccountNonce {
                account_id: 0.into(),
                nonce: 0,
                used_nonces: [0; 2],
            },
            AccountNonce {
                account_id: 1.into(),
                nonce: 0,
                used_nonces: [0; 2],
            },
        ],
        recent_transaction_outcomes: vec![],
        order_books: vec![],
        recent_trades: vec![],
        agent_balances: vec![],
        open_orders: vec![],
    }
}

#[test]
fn failed_sender_keeps_nonce_and_other_sender_can_continue() {
    let mut state = ControllerInner::default();
    state.wallet.insert(0, 0.into());
    state.wallet.insert(1, 1.into());
    state.load = Some(LoadRuntime {
        id: 1,
        config: LoadConfig {
            preset: LoadPreset::RoundRobinTransfers {
                first_account: 0,
                account_count: 2,
                asset: 0,
                amount: 10,
            },
            target: TargetNode::Random,
            tps: 5.0,
        },
        cursor: 0,
        pending: HashMap::new(),
        failed: HashMap::new(),
    });
    let first =
        next_load_transaction(state.load.as_mut().unwrap(), &status(), &state.wallet).unwrap();
    assert_eq!(first.account_id(), 0.into());
    assert_eq!(first.nonce(), 0);
    let digest = state.load.as_ref().unwrap().pending[&0].digest.clone();
    let mut after_failure = status();
    after_failure
        .recent_transaction_outcomes
        .push(TransactionOutcome {
            digest,
            account_id: 0.into(),
            nonce: 0,
            success: false,
            error: Some("InsufficientBalance".into()),
        });
    reconcile_load(&mut state, &after_failure);
    assert_eq!(state.load.as_ref().unwrap().failed[&0], 0);
    let second =
        next_load_transaction(state.load.as_mut().unwrap(), &after_failure, &state.wallet).unwrap();
    assert_eq!(second.account_id(), 1.into());
    assert_eq!(second.nonce(), 0);
}

#[test]
fn taker_targets_best_opposite_price() {
    let mut status = status();
    status
        .order_books
        .push(light_dex::exchange::orderbook::OrderBookSnapshot {
            market_id: 0,
            bids: vec![light_dex::exchange::orderbook::BookLevel {
                price: 98,
                quantity: 2,
                order_count: 1,
            }],
            asks: vec![light_dex::exchange::orderbook::BookLevel {
                price: 102,
                quantity: 2,
                order_count: 1,
            }],
        });
    let mut load = LoadRuntime {
        id: 1,
        config: LoadConfig {
            preset: LoadPreset::BookTaker {
                account_id: 1,
                market_id: 0,
                side: light_dex::exchange::order::Side::Buy,
            },
            target: TargetNode::Random,
            tps: 1.0,
        },
        cursor: 0,
        pending: HashMap::new(),
        failed: HashMap::new(),
    };
    let wallet = BTreeMap::from([(1, 1.into())]);
    assert!(
        matches!(next_load_transaction(&mut load, &status, &wallet), Some(Transaction::PlaceOrder {
            account_id, market_id: 0, price: 102, quantity: light_dex::exchange::market::BTC_ATOMS, nonce: 0,
            order_type: light_dex::exchange::order::OrderType::IOC, ..
        }) if account_id == Address::fixture(1))
    );
}

#[test]
#[ignore = "spawns four node processes and requires a built light_dex binary"]
fn hotstuff2_batched_view_change_survives_primary_kill() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let data = std::env::temp_dir().join(format!(
        "light-dex-hotstuff2-fault-{}",
        rand::random::<u64>()
    ));
    let controller = ControllerState::default();
    controller
        .launch(LaunchConfig {
            project_dir: project.display().to_string(),
            binary_path: "target/debug/light_dex".into(),
            data_dir: data.display().to_string(),
            recover_existing: false,
            remote_placements: vec![],
            node_count: 4,
            tx_batch_window_ms: 25,
            block_time_ms: 150,
            timeout_ms: 1_500,
            auto_view_change_blocks: Some(4),
            poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus: crate::types::ConsensusAlgorithm::Hotstuff2,
            chained_hotstuff: false,
            claim_limit: 4,
            claim_usd: 100,
            claim_btc: 2,
        })
        .unwrap();
    wait_for(&controller, |snapshot| {
        snapshot.running
            && snapshot.metrics.responsive_nodes == 4
            && snapshot.metrics.height_min == Some(1)
    });
    controller.claim_new_accounts(2).unwrap();
    wait_for(&controller, |snapshot| {
        snapshot.metrics.height_min.unwrap_or(0) >= 2
    });
    controller
        .kill_node(TargetNode::Node { node_id: 0 })
        .unwrap();
    wait_for(&controller, |snapshot| {
        snapshot.metrics.responsive_nodes == 3
            && snapshot.metrics.height_min.unwrap_or(0) >= 3
            && snapshot
                .metrics
                .primaries
                .iter()
                .all(|primary| *primary != 0)
    });
    controller.claim_new_accounts(1).unwrap();
    wait_for(&controller, |snapshot| {
        snapshot.metrics.height_min.unwrap_or(0) >= 4
    });
    controller.stop_all();
}

#[test]
#[ignore = "spawns four signed validators and pauses one across a certified rotation"]
fn hotstuff2_certified_handoff_paused_replica_rejoins() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let data = std::env::temp_dir().join(format!(
        "light-dex-fast-rotation-rejoin-{}",
        rand::random::<u64>()
    ));
    let controller = ControllerState::default();
    controller
        .launch(LaunchConfig {
            project_dir: project.display().to_string(),
            binary_path: "target/debug/light_dex".into(),
            data_dir: data.display().to_string(),
            recover_existing: false,
            remote_placements: vec![],
            node_count: 4,
            tx_batch_window_ms: 25,
            block_time_ms: 150,
            timeout_ms: 3_000,
            auto_view_change_blocks: Some(4),
            poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus: crate::types::ConsensusAlgorithm::Hotstuff2,
            chained_hotstuff: false,
            claim_limit: 20,
            claim_usd: 100,
            claim_btc: 2,
        })
        .unwrap();
    wait_for(&controller, |snapshot| {
        snapshot.running
            && snapshot.metrics.responsive_nodes == 4
            && snapshot.metrics.height_min == Some(1)
    });
    controller
        .pause_node(TargetNode::Node { node_id: 3 }, 8_000)
        .unwrap();
    wait_for(&controller, |snapshot| {
        snapshot
            .nodes
            .iter()
            .any(|node| node.node_id == 3 && node.state == "paused")
    });
    for _ in 0..6 {
        controller.claim_new_accounts(1).unwrap();
    }
    wait_for(&controller, |snapshot| {
        snapshot
            .nodes
            .iter()
            .filter(|node| node.node_id != 3)
            .all(|node| {
                node.status
                    .as_ref()
                    .is_some_and(|status| status.chain_height >= 7)
            })
    });
    let caught_up = wait_for(&controller, |snapshot| {
        snapshot.metrics.responsive_nodes == 4
            && snapshot.metrics.chain_consistency == "consistent"
            && snapshot.metrics.views.len() == 1
            && snapshot.nodes.iter().all(|node| {
                node.status
                    .as_ref()
                    .is_some_and(|status| status.chain_height >= 7)
            })
    });
    assert_eq!(caught_up.metrics.chain_consistency, "consistent");
    controller.claim_new_accounts(1).unwrap();
    wait_for(&controller, |snapshot| {
        snapshot.metrics.responsive_nodes == 4
            && snapshot.metrics.chain_consistency == "consistent"
            && snapshot.metrics.views.len() == 1
            && snapshot.nodes.iter().all(|node| {
                node.status
                    .as_ref()
                    .is_some_and(|status| status.chain_height >= 8)
            })
    });
    controller.stop_all();
    wait_for(&controller, |snapshot| {
        !snapshot.running && !snapshot.stopping && snapshot.metrics.alive_nodes == 0
    });
    std::fs::remove_dir_all(data).unwrap();
}

#[test]
#[ignore = "spawns four node processes and requires a built light_dex binary"]
fn fresh_signed_cluster_claims_accounts_and_replays() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let base_data =
        std::env::temp_dir().join(format!("light-dex-claim-smoke-{}", rand::random::<u64>()));
    let controller = ControllerState::default();
    controller
        .launch(LaunchConfig {
            project_dir: project.display().to_string(),
            binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(),
            recover_existing: false,
            remote_placements: vec![],
            node_count: 4,
            tx_batch_window_ms: 50,
            block_time_ms: 150,
            timeout_ms: 1_500,
            auto_view_change_blocks: None,
            poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus: crate::types::ConsensusAlgorithm::Pbft,
            chained_hotstuff: false,
            claim_limit: 3,
            claim_usd: 100,
            claim_btc: 2,
        })
        .unwrap();
    wait_for(&controller, |snapshot| {
        snapshot.running
            && snapshot.metrics.responsive_nodes == 4
            && snapshot.metrics.height_min == Some(1)
    });
    let initial = controller.snapshot();
    assert!(initial.wallet.is_empty());
    assert!(
        initial
            .nodes
            .iter()
            .filter_map(|node| node.status.as_ref())
            .all(|status| status.account_nonces.is_empty()
                && status.faucet_remaining_claims == Some(3))
    );
    let wallets = controller.claim_new_accounts(2).unwrap();
    assert_eq!(
        wallets
            .iter()
            .map(|wallet| wallet.label)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    let claimed = wait_for(&controller, |snapshot| {
        snapshot.metrics.height_min.unwrap_or_default() >= 2
            && snapshot
                .nodes
                .iter()
                .filter_map(|node| node.status.as_ref())
                .all(|status| {
                    wallets.iter().all(|wallet| {
                        status.account_nonces.iter().any(|account| {
                            account.account_id == wallet.address && account.nonce == 1
                        })
                    })
                })
    });
    assert_eq!(
        claimed
            .nodes
            .iter()
            .filter_map(|node| node.status.as_ref())
            .next()
            .unwrap()
            .faucet_remaining_claims,
        Some(1)
    );
    assert!(controller.claim_new_accounts(2).is_err());
    controller
        .submit_transaction(
            Transaction::Transfer {
                from: wallets[0].address,
                to: wallets[1].address,
                asset: 0,
                amount: 10,
                nonce: 1,
            },
            TargetNode::Primary,
        )
        .unwrap();
    let transferred = wait_for(&controller, |snapshot| {
        snapshot
            .nodes
            .iter()
            .filter_map(|node| node.status.as_ref())
            .any(|status| {
                status
                    .account_nonces
                    .iter()
                    .any(|account| account.account_id == wallets[0].address && account.nonce == 2)
            })
    });
    let session_dir = PathBuf::from(transferred.active_data_dir.as_ref().unwrap());
    controller.stop_all();
    wait_for(&controller, |snapshot| {
        !snapshot.running
            && !snapshot.launching
            && !snapshot.stopping
            && snapshot.metrics.alive_nodes == 0
    });
    let manifest = Arc::new(SessionManifest::load(&session_dir.join("manifest.json")).unwrap());
    let replayed = light_dex::blockchain::BlockchainState::load_signed(
        session_dir.join("node-0.json"),
        manifest,
    )
    .unwrap();
    assert_eq!(
        replayed.exchange_state.accounts()[&wallets[0].address].nonce,
        2
    );
    assert_eq!(
        replayed.exchange_state.accounts()[&wallets[0].address].balances[&0].available,
        100 * light_dex::exchange::market::USD_ATOMS - 10
    );
    assert_eq!(
        replayed.exchange_state.accounts()[&wallets[1].address].balances[&0].available,
        100 * light_dex::exchange::market::USD_ATOMS + 10
    );
    std::fs::remove_dir_all(base_data).unwrap();
}

#[test]
#[ignore = "spawns four local validators twice to verify stop and fresh launch"]
fn stop_all_allows_new_signed_session_in_same_controller() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let base_data =
        std::env::temp_dir().join(format!("light-dex-relaunch-{}", rand::random::<u64>()));
    let config = LaunchConfig {
        project_dir: project.display().to_string(),
        binary_path: "target/debug/light_dex".into(),
        data_dir: base_data.display().to_string(),
        recover_existing: false,
        remote_placements: vec![],
        node_count: 4,
        tx_batch_window_ms: 50,
        block_time_ms: 200,
        timeout_ms: 1_500,
        auto_view_change_blocks: None,
        poll_ms: 100,
        poll_mode: crate::types::PollMode::All,
        build_before_launch: false,
        consensus: crate::types::ConsensusAlgorithm::Pbft,
        chained_hotstuff: false,
        claim_limit: 3,
        claim_usd: 100,
        claim_btc: 2,
    };
    let controller = ControllerState::default();
    let mut directories = Vec::new();
    for _ in 0..2 {
        controller.launch(config.clone()).unwrap();
        let started = wait_for(&controller, |snapshot| {
            snapshot.running
                && snapshot.metrics.responsive_nodes == 4
                && snapshot.metrics.height_min == Some(1)
        });
        directories.push(started.active_data_dir.unwrap());
        assert!(started.wallet.is_empty());
        assert!(started.nodes.iter().all(|node| {
            node.status
                .as_ref()
                .is_some_and(|status| status.faucet_remaining_claims == Some(3))
        }));
        controller.stop_all();
        wait_for(&controller, |snapshot| {
            !snapshot.running
                && !snapshot.launching
                && !snapshot.stopping
                && snapshot.metrics.alive_nodes == 0
        });
    }
    assert_ne!(directories[0], directories[1]);
    std::fs::remove_dir_all(base_data).unwrap();
}

fn live_oracle_marks_commit_on_every_validator(consensus: crate::types::ConsensusAlgorithm) {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let base_data = std::env::temp_dir().join(format!(
        "light-dex-market-data-smoke-{}",
        rand::random::<u64>()
    ));
    let controller = ControllerState::default();
    controller
        .launch(LaunchConfig {
            project_dir: project.display().to_string(),
            binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(),
            recover_existing: false,
            remote_placements: vec![],
            node_count: 4,
            tx_batch_window_ms: 50,
            block_time_ms: 250,
            timeout_ms: 1_500,
            auto_view_change_blocks: None,
            poll_ms: 200,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus,
            chained_hotstuff: false,
            claim_limit: 4,
            claim_usd: 100,
            claim_btc: 1,
        })
        .unwrap();
    wait_for(&controller, |snapshot| {
        snapshot.running
            && snapshot.nodes.len() == 4
            && snapshot.nodes.iter().all(|node| {
                node.status.as_ref().is_some_and(|status| {
                    status.market_data.len() == 3
                        && status
                            .market_data
                            .iter()
                            .all(|feed| feed.bid.is_some() && feed.ask.is_some())
                        && status.oracle_prices.iter().any(|price| {
                            price.pair
                                == light_dex::oracle::OraclePair::new(
                                    light_dex::exchange::market::BTC,
                                    light_dex::exchange::market::USD,
                                )
                                && price.committed_mark_usd_e8.is_some()
                        })
                })
            })
    });
    controller.stop_all();
    wait_for(&controller, |snapshot| {
        !snapshot.running && !snapshot.stopping && snapshot.metrics.alive_nodes == 0
    });
    std::fs::remove_dir_all(base_data).unwrap();
}

#[test]
#[ignore = "requires live Binance, OKX, and Coinbase feeds and a built node binary"]
fn pbft_oracle_marks_commit_on_every_validator() {
    live_oracle_marks_commit_on_every_validator(crate::types::ConsensusAlgorithm::Pbft);
}

#[test]
#[ignore = "requires live Binance, OKX, and Coinbase feeds and a built node binary"]
fn hotstuff_oracle_marks_commit_on_every_validator() {
    live_oracle_marks_commit_on_every_validator(crate::types::ConsensusAlgorithm::Hotstuff);
}

/// Process-level market-data test with deterministic local WebSocket fixtures.
/// No exchange credentials or public WebSocket availability is required.
fn local_multimarket_oracle_smoke(
    consensus: crate::types::ConsensusAlgorithm,
    exercise: impl FnOnce(&ControllerState),
) {
    local_multimarket_oracle_smoke_with_binary(consensus, "target/debug/light_dex", exercise);
}

fn local_multimarket_oracle_smoke_with_binary(
    consensus: crate::types::ConsensusAlgorithm,
    binary_path: &str,
    exercise: impl FnOnce(&ControllerState),
) {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let base_data =
        std::env::temp_dir().join(format!("light-dex-multi-oracle-{}", rand::random::<u64>()));
    let mut overridden = Vec::new();
    for (base, symbols, bid, ask) in [
        (
            light_dex::exchange::market::BTC,
            ["BTCUSDT", "BTC-USDT", "BTC-USD"],
            "99999",
            "100001",
        ),
        (
            light_dex::exchange::market::ETH,
            ["ETHUSDT", "ETH-USDT", "ETH-USD"],
            "2999",
            "3001",
        ),
        (
            light_dex::exchange::market::BNB,
            ["BNBUSDT", "BNB-USDT", ""],
            "499",
            "501",
        ),
    ] {
        for venue in light_dex::oracle::Venue::ALL {
            let Some(symbol) = light_dex::oracle::config::venue_symbol(
                light_dex::oracle::OraclePair::new(base, light_dex::exchange::market::USD),
                venue,
            ) else {
                continue;
            };
            let _ = symbols; // Documents the expected per-venue symbol set.
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("ws://{}", listener.local_addr().unwrap());
            let key = if base == light_dex::exchange::market::BTC {
                format!("LIGHT_DEX_{}_WS_URL", venue.as_str().to_ascii_uppercase())
            } else {
                format!(
                    "LIGHT_DEX_{}_{}_WS_URL",
                    symbol.replace('-', "_").to_ascii_uppercase(),
                    venue.as_str().to_ascii_uppercase()
                )
            };
            // SAFETY: this ignored integration test is run alone; no other
            // thread modifies the process environment during this test.
            unsafe {
                std::env::set_var(&key, url);
            }
            overridden.push(key);
            let payload = match venue {
                light_dex::oracle::Venue::Binance => {
                    format!(r#"{{"s":"{symbol}","b":"{bid}","a":"{ask}"}}"#)
                }
                light_dex::oracle::Venue::Okx => format!(
                    r#"{{"data":[{{"instId":"{symbol}","bidPx":"{bid}","askPx":"{ask}"}}]}}"#
                ),
                light_dex::oracle::Venue::Coinbase => format!(
                    r#"{{"type":"ticker","product_id":"{symbol}","best_bid":"{bid}","best_ask":"{ask}"}}"#
                ),
            };
            thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                let mut socket = tungstenite::accept(stream).unwrap();
                if venue != light_dex::oracle::Venue::Binance {
                    let _ = socket.read();
                }
                for _ in 0..200 {
                    if socket
                        .send(tungstenite::Message::Text(payload.clone().into()))
                        .is_err()
                    {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            });
        }
    }
    let controller = ControllerState::default();
    controller
        .launch(LaunchConfig {
            project_dir: project.display().to_string(),
            binary_path: binary_path.into(),
            data_dir: base_data.display().to_string(),
            recover_existing: false,
            remote_placements: vec![],
            node_count: 4,
            tx_batch_window_ms: 50,
            block_time_ms: 250,
            timeout_ms: 1_500,
            auto_view_change_blocks: Some(4),
            poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus,
            chained_hotstuff: false,
            claim_limit: 8,
            claim_usd: 1_000_000,
            claim_btc: 10,
        })
        .unwrap();
    let reached = wait_for(&controller, |snapshot| {
        snapshot.running
            && snapshot.nodes.len() == 4
            && snapshot.nodes.iter().all(|node| {
                node.status.as_ref().is_some_and(|status| {
                    status.markets.len() == 10
                        && [1, 2, 4].iter().all(|base| {
                            status.oracle_prices.iter().any(|price| {
                                price.pair.base_asset == *base
                                    && price.committed_mark_usd_e8.is_some()
                            })
                        })
                })
            })
    });
    assert_eq!(reached.metrics.chain_consistency, "consistent");
    exercise(&controller);
    controller.stop_all();
    wait_for(&controller, |snapshot| {
        !snapshot.running && !snapshot.stopping && snapshot.metrics.alive_nodes == 0
    });
    for key in overridden {
        // SAFETY: this test is run alone, as for set_var above.
        unsafe {
            std::env::remove_var(key);
        }
    }
    std::fs::remove_dir_all(base_data).unwrap();
}

#[test]
#[ignore = "spawns four PBFT validators with local WebSocket fixtures"]
fn pbft_multimarket_oracles_commit() {
    local_multimarket_oracle_smoke(crate::types::ConsensusAlgorithm::Pbft, |_| {});
}

#[test]
#[ignore = "spawns four HotStuff validators with local WebSocket fixtures"]
fn hotstuff_multimarket_oracles_commit() {
    local_multimarket_oracle_smoke(crate::types::ConsensusAlgorithm::Hotstuff, |_| {});
}

#[test]
#[ignore = "spawns release PBFT then HotStuff validators; run alone with --test-threads=1"]
fn signed_quote_batches_trade_perps_on_both_algorithms() {
    use light_dex::blockchain::QuoteOrder;
    use light_dex::exchange::order::{OrderType, Side};
    for algorithm in [
        crate::types::ConsensusAlgorithm::Pbft,
        crate::types::ConsensusAlgorithm::Hotstuff,
    ] {
        local_multimarket_oracle_smoke_with_binary(
            algorithm,
            "target/release/light_dex",
            |controller| {
                let wallets = controller.claim_new_accounts(2).unwrap();
                let maker = wallets[0].address;
                let taker = wallets[1].address;
                let at_nonce = |owner, nonce| {
                    wait_for(controller, |snapshot| {
                        snapshot.nodes.iter().all(|node| {
                            node.status.as_ref().is_some_and(|status| {
                                status
                                    .account_nonces
                                    .iter()
                                    .any(|a| a.account_id == owner && a.nonce == nonce)
                            })
                        })
                    })
                };
                for owner in [maker, taker] {
                    controller
                        .submit_transaction(
                            Transaction::SetMarginMode {
                                account_id: owner,
                                mode: light_dex::exchange::perp::MarginMode::Cross,
                                nonce: 1,
                            },
                            TargetNode::Random,
                        )
                        .unwrap();
                    at_nonce(owner, 2);
                    controller
                        .submit_transaction(
                            Transaction::DepositPerpCollateral {
                                account_id: owner,
                                market_id: 3,
                                amount: 20_000_000_000,
                                nonce: 2,
                            },
                            TargetNode::Random,
                        )
                        .unwrap();
                    at_nonce(owner, 3);
                }
                let snapshot = controller.snapshot();
                let status = snapshot
                    .nodes
                    .iter()
                    .find_map(|node| node.status.as_ref())
                    .unwrap();
                let market = &status.markets.iter().find(|m| m.id == 3).unwrap().config;
                let price = market.price.round_e8_to_tick(3000 * 100_000_000).unwrap();
                let quantity = market.quantity.scale();
                controller
                    .submit_transaction(
                        Transaction::UpdateQuotes {
                            reduce: vec![],
                            account_id: maker,
                            market_id: 3,
                            nonce: 3,
                            cancel: vec![],
                            place: vec![
                                QuoteOrder {
                                    order_type: OrderType::PostOnly,
                                    side: Side::Buy,
                                    price: price - 100,
                                    quantity,
                                    reduce_only: false,
                                },
                                QuoteOrder {
                                    order_type: OrderType::PostOnly,
                                    side: Side::Sell,
                                    price: price + 100,
                                    quantity,
                                    reduce_only: false,
                                },
                            ],
                        },
                        TargetNode::Random,
                    )
                    .unwrap();
                let quoted = at_nonce(maker, 4);
                let ids: Vec<_> = quoted
                    .nodes
                    .iter()
                    .find_map(|node| node.status.as_ref())
                    .unwrap()
                    .open_orders
                    .iter()
                    .filter(|order| order.account_id == maker)
                    .map(|order| order.order_id)
                    .collect();
                assert_eq!(ids.len(), 2);
                let bid_id = quoted
                    .nodes
                    .iter()
                    .find_map(|node| node.status.as_ref())
                    .unwrap()
                    .open_orders
                    .iter()
                    .find(|order| order.account_id == maker && order.side == Side::Buy)
                    .unwrap()
                    .order_id;
                controller
                    .submit_transaction(
                        Transaction::UpdateQuotes {
                            account_id: maker,
                            market_id: 3,
                            nonce: 4,
                            cancel: vec![],
                            place: vec![],
                            reduce: vec![light_dex::blockchain::QuoteReduction {
                                order_id: bid_id,
                                remaining_quantity: quantity / 2,
                            }],
                        },
                        TargetNode::Random,
                    )
                    .unwrap();
                let reduced = at_nonce(maker, 5);
                assert!(reduced.nodes.iter().all(|node| {
                    node.status
                        .as_ref()
                        .unwrap()
                        .open_orders
                        .iter()
                        .any(|order| {
                            order.order_id == bid_id && order.remaining_quantity == quantity / 2
                        })
                }));
                controller
                    .submit_transaction(
                        Transaction::UpdateQuotes {
                            reduce: vec![],
                            account_id: taker,
                            market_id: 3,
                            nonce: 3,
                            cancel: vec![],
                            place: vec![QuoteOrder {
                                order_type: OrderType::FOK,
                                side: Side::Buy,
                                price: price + 100,
                                quantity,
                                reduce_only: false,
                            }],
                        },
                        TargetNode::Random,
                    )
                    .unwrap();
                let traded = at_nonce(taker, 4);
                assert!(traded.nodes.iter().all(|node| {
                    node.status
                        .as_ref()
                        .unwrap()
                        .recent_trades
                        .iter()
                        .any(|trade| {
                            trade.market_id == 3
                                && trade.maker_account == maker
                                && trade.taker_account == taker
                        })
                }));
                controller
                    .submit_transaction(
                        Transaction::UpdateQuotes {
                            reduce: vec![],
                            account_id: maker,
                            market_id: 3,
                            nonce: 5,
                            cancel: ids,
                            place: vec![],
                        },
                        TargetNode::Random,
                    )
                    .unwrap();
                let done = at_nonce(maker, 6);
                assert!(done.nodes.iter().all(|node| {
                    node.status
                        .as_ref()
                        .unwrap()
                        .open_orders
                        .iter()
                        .all(|order| order.account_id != maker)
                }));
                let session = PathBuf::from(done.active_data_dir.as_ref().unwrap());
                let manifest =
                    Arc::new(SessionManifest::load(&session.join("manifest.json")).unwrap());
                let replayed = light_dex::blockchain::BlockchainState::load_signed(
                    session.join("node-0.json"),
                    manifest,
                )
                .unwrap();
                assert!(replayed.exchange_state.verify_state_tree());
                assert!(
                    replayed
                        .exchange_state
                        .perp_positions_for(taker)
                        .iter()
                        .any(|position| position.market_id == 3 && position.size_atoms > 0)
                );
            },
        );
    }
}

#[test]
#[ignore = "spawns four PBFT validators and signs a live cross-margin ETH perp trade"]
fn live_cross_margin_eth_perp_trade() {
    local_multimarket_oracle_smoke(crate::types::ConsensusAlgorithm::Pbft, |controller| {
        let wallets = controller.claim_new_accounts(2).unwrap();
        let long = wallets[0].address;
        let short = wallets[1].address;
        controller
            .submit_transaction(
                Transaction::SetMarginMode {
                    account_id: long,
                    mode: light_dex::exchange::perp::MarginMode::Cross,
                    nonce: 1,
                },
                TargetNode::Random,
            )
            .unwrap();
        wait_for(controller, |snapshot| {
            snapshot.nodes.iter().all(|node| {
                node.status.as_ref().is_some_and(|status| {
                    status
                        .account_nonces
                        .iter()
                        .any(|account| account.account_id == long && account.nonce == 2)
                })
            })
        });
        controller
            .submit_transaction(
                Transaction::DepositPerpCollateral {
                    account_id: long,
                    market_id: 3,
                    amount: 20_000_000_000,
                    nonce: 2,
                },
                TargetNode::Random,
            )
            .unwrap();
        controller
            .submit_transaction(
                Transaction::DepositPerpCollateral {
                    account_id: short,
                    market_id: 3,
                    amount: 20_000_000_000,
                    nonce: 1,
                },
                TargetNode::Random,
            )
            .unwrap();
        wait_for(controller, |snapshot| {
            snapshot.nodes.iter().all(|node| {
                node.status.as_ref().is_some_and(|status| {
                    status
                        .account_nonces
                        .iter()
                        .any(|account| account.account_id == long && account.nonce == 3)
                        && status
                            .account_nonces
                            .iter()
                            .any(|account| account.account_id == short && account.nonce == 2)
                })
            })
        });
        let snapshot = controller.snapshot();
        let status = snapshot
            .nodes
            .iter()
            .find_map(|node| node.status.as_ref())
            .unwrap();
        let market = status.markets.iter().find(|market| market.id == 3).unwrap();
        let mark = status
            .oracle_prices
            .iter()
            .find(|price| price.pair.base_asset == 2)
            .and_then(|price| price.committed_mark_usd_e8)
            .unwrap();
        let price = market.config.price.round_e8_to_tick(mark).unwrap();
        let quantity = market.config.quantity.scale();
        controller
            .submit_transaction(
                Transaction::PlacePerpOrder {
                    account_id: short,
                    market_id: 3,
                    order_type: light_dex::exchange::order::OrderType::LO,
                    side: light_dex::exchange::order::Side::Sell,
                    price,
                    quantity,
                    reduce_only: false,
                    nonce: 2,
                },
                TargetNode::Random,
            )
            .unwrap();
        wait_for(controller, |snapshot| {
            snapshot.nodes.iter().all(|node| {
                node.status.as_ref().is_some_and(|status| {
                    status
                        .open_orders
                        .iter()
                        .any(|order| order.account_id == short && order.market_id == 3)
                })
            })
        });
        controller
            .submit_transaction(
                Transaction::PlacePerpOrder {
                    account_id: long,
                    market_id: 3,
                    order_type: light_dex::exchange::order::OrderType::FOK,
                    side: light_dex::exchange::order::Side::Buy,
                    price,
                    quantity,
                    reduce_only: false,
                    nonce: 3,
                },
                TargetNode::Random,
            )
            .unwrap();
        let settled = wait_for(controller, |snapshot| {
            snapshot.nodes.iter().all(|node| {
                node.status.as_ref().is_some_and(|status| {
                    status
                        .recent_trades
                        .iter()
                        .any(|trade| trade.market_id == 3 && trade.taker_account == long)
                })
            })
        });
        assert_eq!(settled.metrics.chain_consistency, "consistent");
        assert!(
            settled
                .nodes
                .iter()
                .filter_map(|node| node.status.as_ref())
                .all(
                    |status| status.perp_risk.iter().any(|risk| risk.account_id == long
                        && risk.mode == light_dex::exchange::perp::MarginMode::Cross)
                )
        );
        let session = PathBuf::from(settled.active_data_dir.as_ref().unwrap());
        let manifest = Arc::new(SessionManifest::load(&session.join("manifest.json")).unwrap());
        let replayed = light_dex::blockchain::BlockchainState::load_signed(
            session.join("node-0.json"),
            manifest,
        )
        .unwrap();
        assert!(
            replayed
                .exchange_state
                .perp_position_snapshots()
                .iter()
                .any(|position| position.account_id == long
                    && position.market_id == 3
                    && position.size_atoms > 0)
        );
    });
}

#[test]
#[ignore = "spawns four PBFT validators and a live ETH perp random participant"]
fn live_perp_random_participant_bootstraps_margin_and_quotes() {
    local_multimarket_oracle_smoke(crate::types::ConsensusAlgorithm::Pbft, |controller| {
        let wallet = controller.claim_new_accounts(1).unwrap().remove(0);
        controller
            .start_agent(crate::types::AgentConfig {
                agent_id: 90,
                account_id: wallet.label,
                market_id: 3,
                strategy: light_dex::participants::strategies::AgentStrategy::RandomOrder {
                    order_type: light_dex::exchange::order::OrderType::LO,
                    tps: 2.0,
                    passive_offset_ticks: 10,
                    max_resting_orders: 2,
                    resting_ttl_ms: 5_000,
                },
                target: TargetNode::Random,
                wait_for_confirmation: false,
            })
            .unwrap();
        let snapshot = wait_for(controller, |snapshot| {
            snapshot.nodes.iter().all(|node| {
                node.status.as_ref().is_some_and(|status| {
                    status.perp_risk.iter().any(|risk| {
                        risk.account_id == wallet.address
                            && risk.market_id == Some(3)
                            && risk.collateral_usd_atoms > 0
                    }) && status
                        .open_orders
                        .iter()
                        .any(|order| order.account_id == wallet.address && order.market_id == 3)
                })
            })
        });
        assert_eq!(snapshot.metrics.chain_consistency, "consistent");
        controller.stop_agent(90).unwrap();
        wait_for(controller, |snapshot| {
            snapshot.agents.iter().all(|agent| agent.agent_id != 90)
        });
    });
}

fn local_signed_transfer_probe(consensus: crate::types::ConsensusAlgorithm) {
    local_multimarket_oracle_smoke_with_binary(
        consensus,
        "target/release/light_dex",
        |controller| {
            let wallets = controller.claim_new_accounts(5).unwrap();
            let before = controller
                .snapshot()
                .nodes
                .iter()
                .find_map(|node| node.status.as_ref())
                .unwrap()
                .transactions_committed;
            let started = Instant::now();
            for batch in 0..10 {
                for nonce in (1 + batch * 20)..=(20 + batch * 20) {
                    for sender in &wallets[..4] {
                        controller
                            .submit_benchmark_transaction(
                                Transaction::Transfer {
                                    from: sender.address,
                                    to: wallets[4].address,
                                    asset: 0,
                                    amount: 1,
                                    nonce,
                                },
                                TargetNode::Random,
                            )
                            .unwrap();
                    }
                }
                wait_for(controller, |snapshot| {
                    snapshot.nodes.iter().all(|node| {
                        node.status.as_ref().is_some_and(|status| {
                            status.transactions_committed >= before + (batch + 1) * 80
                        })
                    })
                });
            }
            // Status replies are cached independently for one second per node.
            // Reaching the transaction count does not mean all sampled tips have
            // caught up to that same block yet.
            let result = wait_for(controller, |snapshot| {
                snapshot.metrics.chain_consistency == "consistent"
                    && snapshot.nodes.iter().all(|node| {
                        node.status
                            .as_ref()
                            .is_some_and(|status| status.transactions_committed >= before + 800)
                    })
            });
            let seconds = started.elapsed().as_secs_f64();
            // Admission is idempotent for an already committed exact transaction,
            // but a different stale-nonce transaction must be rejected explicitly.
            let duplicate = Transaction::Transfer {
                from: wallets[0].address,
                to: wallets[4].address,
                asset: 0,
                amount: 1,
                nonce: 200,
            };
            let (address, auth_key) = {
                let state = controller.inner.lock().unwrap();
                (
                    resolve_target(&state, &TargetNode::Random).unwrap().1,
                    state.auth_key,
                )
            };
            send_signed_transaction(&controller.inner, address, duplicate, &auth_key).unwrap();
            assert!(
                send_signed_transaction(
                    &controller.inner,
                    address,
                    Transaction::Transfer {
                        from: wallets[0].address,
                        to: wallets[4].address,
                        asset: 0,
                        amount: 2,
                        nonce: 0,
                    },
                    &auth_key
                )
                .is_err()
            );
            println!(
                "SIGNED_TRANSFER_PROBE consensus={consensus:?} committed=800 elapsed_s={seconds:.3} effective_tps={:.1} height_min={:?} height_max={:?}",
                800.0 / seconds,
                result.metrics.height_min,
                result.metrics.height_max
            );
        },
    );
}

#[test]
#[ignore = "spawns four PBFT validators and commits 800 signed transfers"]
fn pbft_local_signed_transfer_probe() {
    local_signed_transfer_probe(crate::types::ConsensusAlgorithm::Pbft);
}

#[test]
#[ignore = "spawns four HotStuff validators and commits 800 signed transfers"]
fn hotstuff_local_signed_transfer_probe() {
    local_signed_transfer_probe(crate::types::ConsensusAlgorithm::Hotstuff);
}

#[test]
#[ignore = "spawns four release validators with four-block rotation and kills the observed primary"]
fn hotstuff_rotation_four_survives_validator_loss() {
    local_multimarket_oracle_smoke_with_binary(
        crate::types::ConsensusAlgorithm::Hotstuff,
        "target/release/light_dex",
        |controller| {
            let wallets = controller.claim_new_accounts(2).unwrap();
            for nonce in 1..=30 {
                if nonce == 16 {
                    controller.kill_node(TargetNode::Primary).unwrap();
                }
                controller
                    .submit_benchmark_transaction(
                        Transaction::Transfer {
                            from: wallets[0].address,
                            to: wallets[1].address,
                            asset: 0,
                            amount: 1,
                            nonce,
                        },
                        TargetNode::Random,
                    )
                    .unwrap();
            }
            let recovered = wait_for(controller, |snapshot| {
                let done = snapshot.metrics.responsive_nodes == 3
                    && snapshot
                        .nodes
                        .iter()
                        .filter(|node| node.state != "dead")
                        .all(|node| {
                            node.status.as_ref().is_some_and(|status| {
                                status.account_nonces.iter().any(|account| {
                                    account.account_id == wallets[0].address && account.nonce == 31
                                })
                            })
                        });
                // Admission is not a replicated commit. The killed validator
                // may have held a nonce predecessor that had not been forwarded.
                // Retry the SAME transactions, as real participant loops do.
                if !done {
                    for nonce in 1..=30 {
                        let _ = controller.submit_benchmark_transaction(
                            Transaction::Transfer {
                                from: wallets[0].address,
                                to: wallets[1].address,
                                asset: 0,
                                amount: 1,
                                nonce,
                            },
                            TargetNode::Random,
                        );
                    }
                }
                done
            });
            assert_eq!(recovered.metrics.alive_nodes, 3);
        },
    );
}

#[test]
#[ignore = "requires Tokyo SSH/Tailscale profile; launches three local and two remote validators"]
fn two_tokyo_validators_keep_quorum_after_local_loss() {
    use crate::types::RemotePlacement;
    let controller = ControllerState::default();
    struct Stop(ControllerState);
    impl Drop for Stop {
        fn drop(&mut self) {
            self.0.stop_all();
        }
    }
    let _stop = Stop(controller.clone());
    let mut config = crate::default_config();
    config.node_count = 5;
    config.remote_placements = vec![RemotePlacement {
        profile_id: "tokyo".into(),
        count: 2,
    }];
    config.binary_path = "target/release/light_dex".into();
    config.build_before_launch = false;
    config.block_time_ms = 1_000;
    config.timeout_ms = 8_000;
    config.poll_mode = crate::types::PollMode::All;
    let session_dir =
        PathBuf::from(&config.project_dir).join("benchmark_results/remote-fault-tests");
    config.data_dir = session_dir.display().to_string();
    controller.launch(config).unwrap();
    let wait = |predicate: &dyn Fn(&ControlSnapshot) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let snapshot = controller.snapshot();
            if predicate(&snapshot) {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "WAN cluster did not advance: {:?}",
                snapshot.logs.iter().rev().take(12).collect::<Vec<_>>()
            );
            thread::sleep(Duration::from_millis(250));
        }
    };
    let ready = wait(&|snapshot| {
        snapshot.running
            && snapshot.metrics.responsive_nodes == 5
            && snapshot
                .metrics
                .height_min
                .is_some_and(|height| height >= 3)
    });
    let height = ready.metrics.height_min.unwrap();
    controller
        .kill_node(TargetNode::Node { node_id: 0 })
        .unwrap();
    let advanced = wait(&|snapshot| {
        snapshot.metrics.responsive_nodes == 4
            && snapshot
                .nodes
                .iter()
                .filter(|node| node.state != "dead")
                .count()
                == 4
            && snapshot
                .metrics
                .height_min
                .is_some_and(|current| current >= height + 3)
    });
    assert_eq!(advanced.metrics.alive_nodes, 4);
    controller.stop_all();
    let _ = wait(&|snapshot| !snapshot.running && !snapshot.launching && !snapshot.stopping);
}

fn auto_rotation_smoke(consensus: crate::types::ConsensusAlgorithm) {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let base_data =
        std::env::temp_dir().join(format!("light-dex-auto-rotation-{}", rand::random::<u64>()));
    let controller = ControllerState::default();
    controller
        .launch(LaunchConfig {
            project_dir: project.display().to_string(),
            binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(),
            recover_existing: false,
            remote_placements: vec![],
            node_count: 4,
            tx_batch_window_ms: 50,
            block_time_ms: 100,
            timeout_ms: 1_500,
            auto_view_change_blocks: Some(2),
            poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus,
            chained_hotstuff: false,
            claim_limit: 10,
            claim_usd: 100,
            claim_btc: 2,
        })
        .unwrap();
    wait_for(&controller, |snapshot| {
        snapshot.running
            && snapshot.metrics.responsive_nodes == 4
            && snapshot.metrics.height_min == Some(1)
    });
    let wallet = controller.claim_new_accounts(1).unwrap().remove(0);
    controller
        .submit_transaction(
            Transaction::Transfer {
                from: wallet.address,
                to: wallet.address,
                asset: 0,
                amount: 1,
                nonce: 1,
            },
            TargetNode::Primary,
        )
        .unwrap();
    let rotated = wait_for(&controller, |snapshot| {
        snapshot.metrics.responsive_nodes == 4
            && snapshot
                .nodes
                .iter()
                .filter_map(|node| node.status.as_ref())
                .all(|status| {
                    status.chain_height >= 3
                        && status.current_view == 1
                        && status.current_primary == 1
                })
    });
    assert_eq!(rotated.metrics.chain_consistency, "consistent");
    controller
        .submit_transaction(
            Transaction::Transfer {
                from: wallet.address,
                to: wallet.address,
                asset: 0,
                amount: 1,
                nonce: 2,
            },
            TargetNode::Primary,
        )
        .unwrap();
    let continued = wait_for(&controller, |snapshot| {
        snapshot.metrics.responsive_nodes == 4
            && snapshot
                .nodes
                .iter()
                .filter_map(|node| node.status.as_ref())
                .all(|status| {
                    status.chain_height >= 4
                        && status.account_nonces.iter().any(|account| {
                            account.account_id == wallet.address && account.nonce == 3
                        })
                })
    });
    assert_eq!(continued.metrics.chain_consistency, "consistent");
    controller
        .submit_transaction(
            Transaction::Transfer {
                from: wallet.address,
                to: wallet.address,
                asset: 0,
                amount: 1,
                nonce: 3,
            },
            TargetNode::Primary,
        )
        .unwrap();
    let rotated_again = wait_for(&controller, |snapshot| {
        snapshot.metrics.responsive_nodes == 4
            && snapshot
                .nodes
                .iter()
                .filter_map(|node| node.status.as_ref())
                .all(|status| {
                    status.chain_height >= 5
                        && status.current_view == 2
                        && status.current_primary == 2
                })
    });
    assert_eq!(rotated_again.metrics.chain_consistency, "consistent");
    controller.stop_all();
    wait_for(&controller, |snapshot| {
        !snapshot.running
            && !snapshot.launching
            && !snapshot.stopping
            && snapshot.metrics.alive_nodes == 0
    });
    std::fs::remove_dir_all(base_data).unwrap();
}

#[test]
#[ignore = "spawns four node processes and requires a built light_dex binary"]
fn pbft_automatically_rotates_after_two_blocks() {
    auto_rotation_smoke(crate::types::ConsensusAlgorithm::Pbft);
}

#[test]
#[ignore = "spawns four node processes and requires a built light_dex binary"]
fn hotstuff_automatically_rotates_after_two_blocks() {
    auto_rotation_smoke(crate::types::ConsensusAlgorithm::Hotstuff);
}

#[test]
#[ignore = "spawns four node processes and requires a built light_dex binary"]
fn chained_hotstuff_finalizes_and_rotates_with_signed_claims() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let base_data =
        std::env::temp_dir().join(format!("light-dex-chained-{}", rand::random::<u64>()));
    let controller = ControllerState::default();
    controller
        .launch(LaunchConfig {
            project_dir: project.display().to_string(),
            binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(),
            recover_existing: false,
            remote_placements: vec![],
            node_count: 4,
            tx_batch_window_ms: 50,
            block_time_ms: 150,
            timeout_ms: 3_000,
            auto_view_change_blocks: Some(4),
            poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus: crate::types::ConsensusAlgorithm::Hotstuff,
            chained_hotstuff: true,
            claim_limit: 10,
            claim_usd: 1_000_000,
            claim_btc: 1_000,
        })
        .unwrap();
    wait_for(&controller, |snapshot| {
        snapshot.running
            && snapshot.metrics.responsive_nodes == 4
            && snapshot.metrics.height_min == Some(1)
    });
    let wallet = controller.claim_new_accounts(1).unwrap().remove(0);
    let rotated = wait_for(&controller, |snapshot| {
        snapshot.metrics.responsive_nodes == 4
            && snapshot.metrics.chain_consistency == "consistent"
            && snapshot
                .nodes
                .iter()
                .filter_map(|node| node.status.as_ref())
                .all(|status| {
                    status.current_view >= 1
                        && status.account_nonces.iter().any(|account| {
                            account.account_id == wallet.address && account.nonce == 1
                        })
                })
    });
    assert_eq!(rotated.metrics.chain_consistency, "consistent");
    controller
        .submit_transaction(
            Transaction::Transfer {
                from: wallet.address,
                to: wallet.address,
                asset: 0,
                amount: 1,
                nonce: 1,
            },
            TargetNode::Primary,
        )
        .unwrap();
    let continued = wait_for(&controller, |snapshot| {
        snapshot.metrics.responsive_nodes == 4
            && snapshot.metrics.chain_consistency == "consistent"
            && snapshot
                .nodes
                .iter()
                .filter_map(|node| node.status.as_ref())
                .all(|status| {
                    status
                        .account_nonces
                        .iter()
                        .any(|account| account.account_id == wallet.address && account.nonce == 2)
                })
    });
    assert_eq!(continued.metrics.chain_consistency, "consistent");
    let data_dir = PathBuf::from(continued.active_data_dir.as_ref().unwrap());
    controller.stop_all();
    wait_for(&controller, |snapshot| {
        !snapshot.running && !snapshot.stopping && snapshot.metrics.alive_nodes == 0
    });
    let manifest = Arc::new(
        light_dex::crypto::SessionManifest::load(&data_dir.join("manifest.json")).unwrap(),
    );
    for id in 0..4 {
        let replayed = light_dex::blockchain::BlockchainState::load_signed(
            data_dir.join(format!("node-{id}.json")),
            manifest.clone(),
        )
        .unwrap();
        assert!(replayed.blocks.len() >= 4);
    }
    std::fs::remove_dir_all(base_data).unwrap();
}

#[test]
#[ignore = "spawns four node processes and requires a built light_dex binary"]
fn transfer_from_claimed_wallet_creates_unfunded_recipient() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let base_data =
        std::env::temp_dir().join(format!("light-dex-airdrop-{}", rand::random::<u64>()));
    let controller = ControllerState::default();
    controller
        .launch(LaunchConfig {
            project_dir: project.display().to_string(),
            binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(),
            recover_existing: false,
            remote_placements: vec![],
            node_count: 4,
            tx_batch_window_ms: 50,
            block_time_ms: 150,
            timeout_ms: 1_500,
            auto_view_change_blocks: None,
            poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus: crate::types::ConsensusAlgorithm::Pbft,
            chained_hotstuff: false,
            claim_limit: 1_000,
            claim_usd: 1_000_000,
            claim_btc: 1_000,
        })
        .unwrap();
    wait_for(&controller, |snapshot| {
        snapshot.running
            && snapshot.metrics.responsive_nodes == 4
            && snapshot.metrics.height_min == Some(1)
    });
    let funded = controller.claim_new_accounts(1).unwrap().remove(0);
    let unfunded = controller.create_address().unwrap();
    assert_eq!(funded.label, 0);
    assert_eq!(unfunded.label, 1);
    controller
        .submit_transaction(
            Transaction::Transfer {
                from: funded.address,
                to: unfunded.address,
                asset: 0,
                amount: 5 * light_dex::exchange::market::USD_ATOMS,
                nonce: 1,
            },
            TargetNode::Primary,
        )
        .unwrap();
    let committed = wait_for(&controller, |snapshot| {
        snapshot.metrics.height_min.unwrap_or_default() >= 3
            && snapshot
                .nodes
                .iter()
                .filter(|node| node.state == "ok")
                .filter_map(|node| node.status.as_ref())
                .all(|status| {
                    status.agent_balances.iter().any(|balance| {
                        balance.account_id == funded.address
                            && balance.usd_available
                                == 999_995 * light_dex::exchange::market::USD_ATOMS
                    }) && status.agent_balances.iter().any(|balance| {
                        balance.account_id == unfunded.address
                            && balance.usd_available == 5 * light_dex::exchange::market::USD_ATOMS
                    }) && status
                        .account_nonces
                        .iter()
                        .any(|account| account.account_id == funded.address && account.nonce == 2)
                })
    });
    let session_dir = PathBuf::from(committed.active_data_dir.as_ref().unwrap());
    assert!(session_dir.join("keys/account-0.key").exists());
    assert_eq!(
        committed
            .wallet
            .iter()
            .find(|entry| entry.label == 0)
            .unwrap()
            .address,
        funded.address
    );
    controller.stop_all();
    wait_for(&controller, |snapshot| {
        !snapshot.running && !snapshot.launching && !snapshot.stopping
    });
    let manifest = Arc::new(SessionManifest::load(&session_dir.join("manifest.json")).unwrap());
    let replayed = light_dex::blockchain::BlockchainState::load_signed(
        session_dir.join("node-0.json"),
        manifest,
    )
    .unwrap();
    assert_eq!(
        replayed.exchange_state.accounts()[&funded.address].balances[&0].available,
        999_995
    );
    assert_eq!(
        replayed.exchange_state.accounts()[&unfunded.address].balances[&0].available,
        5
    );
    std::fs::remove_dir_all(base_data).unwrap();
}

#[test]
#[ignore = "spawns four node processes and requires a built light_dex binary"]
fn cluster_commits_order_book_and_replays_signed_archive() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let base_data = std::env::temp_dir().join(format!("light-dex-smoke-{}", rand::random::<u64>()));
    let config = LaunchConfig {
        project_dir: project.display().to_string(),
        binary_path: "target/debug/light_dex".into(),
        data_dir: base_data.display().to_string(),
        recover_existing: false,
        remote_placements: vec![],
        node_count: 4,
        tx_batch_window_ms: 50,
        block_time_ms: 100,
        timeout_ms: 800,
        auto_view_change_blocks: None,
        poll_ms: 100,
        poll_mode: crate::types::PollMode::All,
        build_before_launch: false,
        consensus: crate::types::ConsensusAlgorithm::Pbft,
        chained_hotstuff: false,
        claim_limit: 1_000,
        claim_usd: 1_000_000,
        claim_btc: 1_000,
    };
    let controller = ControllerState::default();
    controller.launch(config.clone()).unwrap();
    let ready = wait_for(&controller, |snapshot| {
        snapshot.running
            && snapshot.metrics.responsive_nodes == 4
            && snapshot.metrics.height_min == Some(1)
    });
    let queried = controller.refresh_node_status(1).unwrap();
    assert_eq!(queried.node_id, 1);
    assert!(queried.last_status_at_ms.is_some());
    assert!(queried.status.is_some());
    let data_dir = ready.active_data_dir.clone().unwrap();
    controller.claim_new_accounts(3).unwrap();
    controller
        .submit_transaction(
            Transaction::PlaceOrder {
                account_id: wallet_addr(&controller, 0),
                market_id: 0,
                order_type: light_dex::exchange::order::OrderType::LO,
                side: light_dex::exchange::order::Side::Sell,
                price: 100_000,
                quantity: light_dex::exchange::market::BTC_ATOMS,
                nonce: 1,
            },
            TargetNode::Random,
        )
        .unwrap();
    let committed = wait_for(&controller, |snapshot| {
        snapshot.metrics.height_min.unwrap_or_default() >= 2
            && snapshot.metrics.chain_consistency == "consistent"
            && snapshot
                .nodes
                .iter()
                .filter_map(|node| node.status.as_ref())
                .any(|status| {
                    status.order_books.iter().any(|book| {
                        book.market_id == 0
                            && book.asks.first().is_some_and(|level| {
                                level.price == 100_000
                                    && level.quantity == light_dex::exchange::market::BTC_ATOMS
                            })
                    })
                })
    });
    assert_eq!(committed.metrics.chain_consistency, "consistent");
    controller
        .kill_node(TargetNode::Node { node_id: 0 })
        .unwrap();
    assert!(controller.refresh_node_status(0).is_err());
    controller
        .submit_transaction(
            Transaction::Transfer {
                from: wallet_addr(&controller, 1),
                to: wallet_addr(&controller, 2),
                asset: 0,
                amount: 5 * light_dex::exchange::market::USD_ATOMS,
                nonce: 1,
            },
            TargetNode::Node { node_id: 2 },
        )
        .unwrap();
    let after_failover = wait_for(&controller, |snapshot| {
        snapshot.metrics.responsive_nodes == 3
            && snapshot.metrics.chain_consistency == "consistent"
            && snapshot.metrics.views.contains(&1)
            && snapshot.metrics.height_min.unwrap_or_default() >= 3
            && snapshot
                .nodes
                .iter()
                .filter(|node| node.state == "ok")
                .filter_map(|node| node.status.as_ref())
                .all(|status| {
                    status.account_nonces.iter().any(|account| {
                        account.account_id == wallet_addr(&controller, 1) && account.nonce == 2
                    })
                })
    });
    controller.stop_all();
    wait_for(&controller, |snapshot| {
        !snapshot.running
            && !snapshot.launching
            && !snapshot.stopping
            && snapshot.metrics.alive_nodes == 0
    });
    assert_eq!(after_failover.metrics.chain_consistency, "consistent");
    let manifest =
        Arc::new(SessionManifest::load(&PathBuf::from(&data_dir).join("manifest.json")).unwrap());
    let replayed = light_dex::blockchain::BlockchainState::load_signed(
        PathBuf::from(&data_dir).join("node-1.json"),
        manifest,
    )
    .unwrap();
    assert!(replayed.blocks.len() >= 3);
    assert!(replayed.exchange_state.order_books().get(&0).is_some());
    let mut recovery = config;
    recovery.data_dir = data_dir;
    recovery.recover_existing = true;
    controller.launch(recovery).unwrap();
    wait_for(&controller, |snapshot| {
        snapshot
            .logs
            .iter()
            .any(|line| line.contains("live recovery is unavailable"))
    });
    std::fs::remove_dir_all(base_data).unwrap();
}

#[test]
#[ignore = "spawns node processes and requires a built light_dex binary"]
fn five_tps_across_accounts_can_batch_transactions() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let base_data = std::env::temp_dir().join(format!("light-dex-batch-{}", rand::random::<u64>()));
    let controller = ControllerState::default();
    controller
        .launch(LaunchConfig {
            project_dir: project.display().to_string(),
            binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(),
            recover_existing: false,
            remote_placements: vec![],
            node_count: 4,
            tx_batch_window_ms: 50,
            block_time_ms: 500,
            timeout_ms: 1500,
            auto_view_change_blocks: None,
            poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus: crate::types::ConsensusAlgorithm::Pbft,
            chained_hotstuff: false,
            claim_limit: 1_000,
            claim_usd: 1_000_000,
            claim_btc: 1_000,
        })
        .unwrap();
    wait_for(&controller, |snapshot| {
        snapshot.running
            && snapshot.metrics.responsive_nodes == 4
            && snapshot.metrics.height_min == Some(1)
    });
    controller.claim_new_accounts(10).unwrap();
    controller
        .start_load(LoadConfig {
            preset: LoadPreset::RoundRobinTransfers {
                first_account: 0,
                account_count: 10,
                asset: 0,
                amount: 1,
            },
            target: TargetNode::Node { node_id: 0 },
            tps: 5.0,
        })
        .unwrap();
    let observed = wait_for(&controller, |snapshot| {
        snapshot
            .nodes
            .iter()
            .filter(|node| node.state == "ok")
            .filter_map(|node| node.status.as_ref())
            .any(|status| {
                status
                    .recent_blocks
                    .iter()
                    .any(|block| block.transaction_count >= 2)
            })
    });
    controller.stop_all();
    wait_for(&controller, |snapshot| {
        !snapshot.running
            && !snapshot.launching
            && !snapshot.stopping
            && snapshot.metrics.alive_nodes == 0
    });
    assert!(observed.metrics.submitted >= 2);
    std::fs::remove_dir_all(base_data).unwrap();
}

#[test]
#[ignore = "spawns node processes and requires a built light_dex binary"]
fn invalid_account_signature_is_rejected_before_mempool() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let base_data = std::env::temp_dir().join(format!(
        "light-dex-invalid-signature-{}",
        rand::random::<u64>()
    ));
    let controller = ControllerState::default();
    controller
        .launch(LaunchConfig {
            project_dir: project.display().to_string(),
            binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(),
            recover_existing: false,
            remote_placements: vec![],
            node_count: 4,
            tx_batch_window_ms: 50,
            block_time_ms: 200,
            timeout_ms: 1_500,
            auto_view_change_blocks: None,
            poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus: crate::types::ConsensusAlgorithm::Pbft,
            chained_hotstuff: false,
            claim_limit: 1_000,
            claim_usd: 1_000_000,
            claim_btc: 1_000,
        })
        .unwrap();
    wait_for(&controller, |snapshot| {
        snapshot.running
            && snapshot.metrics.height_min == Some(1)
            && snapshot.metrics.responsive_nodes == 4
    });
    controller.claim_new_accounts(2).unwrap();
    let (address, auth_key, mut signed) = {
        let state = controller.inner.lock().unwrap();
        let transaction = Transaction::Transfer {
            from: state.wallet[&0],
            to: state.wallet[&1],
            asset: 0,
            amount: 1,
            nonce: 1,
        };
        let signed = sign_transaction(
            transaction,
            state.signing_manifest.as_ref().unwrap(),
            state.account_keys.get(&0).unwrap(),
        )
        .unwrap();
        (state.nodes[&0].address.unwrap(), state.auth_key, signed)
    };
    let changed = if &signed.signature[..2] == "ff" {
        "fe"
    } else {
        "ff"
    };
    signed.signature.replace_range(..2, changed);
    send_message(
        address,
        &Message::SignedTransaction(signed),
        Duration::from_millis(500),
        &auth_key,
    )
    .unwrap();
    let observed = wait_for(&controller, |snapshot| {
        snapshot
            .nodes
            .iter()
            .filter_map(|node| node.status.as_ref())
            .any(|status| status.node_id == 0 && status.authentication_rejections > 0)
    });
    assert!(observed.metrics.height_min >= Some(2));
    assert!(
        observed
            .nodes
            .iter()
            .filter_map(|node| node.status.as_ref())
            .all(|status| status
                .account_nonces
                .iter()
                .any(|nonce| nonce.account_id == wallet_addr(&controller, 0) && nonce.nonce == 1))
    );
    controller.stop_all();
    wait_for(&controller, |snapshot| {
        !snapshot.running && !snapshot.launching && !snapshot.stopping
    });
    std::fs::remove_dir_all(base_data).unwrap();
}

#[test]
#[ignore = "spawns seven node processes and requires a built light_dex binary"]
fn seven_nodes_batch_quotes_and_commit_after_primary_kill() {
    use light_dex::exchange::order::{OrderType, Side};
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let base_data =
        std::env::temp_dir().join(format!("light-dex-failover-{}", rand::random::<u64>()));
    let controller = ControllerState::default();
    controller
        .launch(LaunchConfig {
            project_dir: project.display().to_string(),
            binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(),
            recover_existing: false,
            remote_placements: vec![],
            node_count: 7,
            tx_batch_window_ms: 50,
            block_time_ms: 500,
            timeout_ms: 1200,
            auto_view_change_blocks: None,
            poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus: crate::types::ConsensusAlgorithm::Pbft,
            chained_hotstuff: false,
            claim_limit: 1_000,
            claim_usd: 1_000_000,
            claim_btc: 1_000,
        })
        .unwrap();
    wait_for(&controller, |s| {
        s.running && s.metrics.responsive_nodes == 7 && s.metrics.height_min == Some(1)
    });
    controller.claim_new_accounts(1).unwrap();
    let (primary_address, auth_key) = {
        let state = controller.inner.lock().unwrap();
        (state.nodes[&0].address.unwrap(), state.auth_key)
    };
    for (nonce, side, price) in [
        (1, Side::Buy, 100_000),
        (2, Side::Sell, 100_010),
        (3, Side::Buy, 99_999),
        (4, Side::Sell, 100_011),
    ] {
        send_signed_transaction(
            &controller.inner,
            primary_address,
            Transaction::PlaceOrder {
                account_id: wallet_addr(&controller, 0),
                market_id: 0,
                order_type: OrderType::LO,
                side,
                price,
                quantity: light_dex::exchange::market::BTC_ATOMS,
                nonce,
            },
            &auth_key,
        )
        .unwrap();
    }
    let batched = wait_for(&controller, |s| {
        s.metrics.height_min.unwrap_or_default() >= 2
            && s.nodes
                .iter()
                .filter_map(|n| n.status.as_ref())
                .any(|status| {
                    status
                        .recent_blocks
                        .iter()
                        .any(|b| b.transaction_count >= 2)
                })
            && s.nodes
                .iter()
                .filter_map(|n| n.status.as_ref())
                .any(|status| {
                    status
                        .account_nonces
                        .iter()
                        .any(|a| a.account_id == wallet_addr(&controller, 0) && a.nonce == 5)
                })
    });
    assert!(batched.metrics.height_min.unwrap_or_default() >= 2);
    controller
        .kill_node(TargetNode::Node { node_id: 0 })
        .unwrap();
    wait_for(&controller, |s| {
        s.metrics.responsive_nodes == 6
            && s.nodes
                .iter()
                .filter(|n| n.state == "ok")
                .filter_map(|n| n.status.as_ref())
                .all(|status| status.current_view >= 1 && status.current_primary == 1)
    });
    // Stale traffic must not occupy the successor's mempool or prevent
    // the next valid nonce from being proposed.
    for _ in 0..4 {
        let follower_address = controller.inner.lock().unwrap().nodes[&2].address.unwrap();
        send_signed_transaction(
            &controller.inner,
            follower_address,
            Transaction::PlaceOrder {
                account_id: wallet_addr(&controller, 0),
                market_id: 0,
                order_type: OrderType::LO,
                side: Side::Buy,
                price: 100_000,
                quantity: light_dex::exchange::market::BTC_ATOMS,
                nonce: 0,
            },
            &auth_key,
        )
        .unwrap();
    }
    controller
        .submit_transaction(
            Transaction::PlaceOrder {
                account_id: wallet_addr(&controller, 0),
                market_id: 0,
                order_type: OrderType::LO,
                side: Side::Buy,
                price: 99_998,
                quantity: light_dex::exchange::market::BTC_ATOMS,
                nonce: 5,
            },
            TargetNode::Node { node_id: 2 },
        )
        .unwrap();
    let recovered = wait_for(&controller, |s| {
        s.metrics.responsive_nodes == 6
            && s.nodes
                .iter()
                .filter(|n| n.state == "ok")
                .filter_map(|n| n.status.as_ref())
                .all(|status| {
                    status.current_view >= 1
                        && status
                            .account_nonces
                            .iter()
                            .any(|a| a.account_id == wallet_addr(&controller, 0) && a.nonce == 6)
                })
    });
    assert_eq!(recovered.metrics.chain_consistency, "consistent");
    controller.stop_all();
    wait_for(&controller, |s| {
        !s.running && !s.launching && !s.stopping && s.metrics.alive_nodes == 0
    });
    std::fs::remove_dir_all(base_data).unwrap();
}

#[test]
#[ignore = "spawns four node processes and requires a built light_dex binary"]
fn market_maker_commits_both_sides_with_batching() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let base_data =
        std::env::temp_dir().join(format!("light-dex-maker-batch-{}", rand::random::<u64>()));
    let controller = ControllerState::default();
    controller
        .launch(LaunchConfig {
            project_dir: project.display().to_string(),
            binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(),
            recover_existing: false,
            remote_placements: vec![],
            node_count: 4,
            tx_batch_window_ms: 50,
            block_time_ms: 500,
            timeout_ms: 1500,
            auto_view_change_blocks: None,
            poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus: crate::types::ConsensusAlgorithm::Pbft,
            chained_hotstuff: false,
            claim_limit: 1_000,
            claim_usd: 1_000_000,
            claim_btc: 1_000,
        })
        .unwrap();
    wait_for(&controller, |s| {
        s.running && s.metrics.responsive_nodes == 4 && s.metrics.height_min == Some(1)
    });
    controller.claim_new_accounts(1).unwrap();
    let id = 1;
    {
        let mut state = controller.inner.lock().unwrap();
        state.market_maker = Some(MarketMakerRuntime {
            id,
            config: MarketMakerConfig {
                account_id: 0,
                levels: 2,
                refresh_ms: 1_000,
                target: TargetNode::Node { node_id: 0 },
            },
            active: true,
            stopping: false,
            latest_depth: Some(PartialDepth {
                last_update_id: 1,
                bids: vec![["100000".into(), "1".into()], ["99999".into(), "1".into()]],
                asks: vec![["100010".into(), "1".into()], ["100011".into(), "1".into()]],
            }),
            feed_at: Some(Instant::now()),
            desired: DesiredBook::default(),
            last_applied: None,
            pending: VecDeque::new(),
            last_error: None,
        });
    }
    assert!(market_maker_step(&controller.inner, id));
    let committed = wait_for(&controller, |s| {
        s.nodes
            .iter()
            .filter_map(|n| n.status.as_ref())
            .any(|status| {
                status
                    .recent_blocks
                    .iter()
                    .any(|b| b.transaction_count >= 2)
                    && status
                        .account_nonces
                        .iter()
                        .any(|a| a.account_id == wallet_addr(&controller, 0) && a.nonce == 5)
            })
    });
    assert_eq!(committed.market_maker.own_open_orders, 4);
    controller.stop_all();
    wait_for(&controller, |s| {
        !s.running && !s.launching && !s.stopping && s.metrics.alive_nodes == 0
    });
    std::fs::remove_dir_all(base_data).unwrap();
}

#[test]
#[ignore = "spawns node processes and requires a built light_dex binary"]
fn hotstuff_commits_and_changes_leader_after_primary_kill() {
    hotstuff_primary_kill_smoke(false);
}

#[test]
#[ignore = "spawns node processes and requires a built light_dex binary"]
fn chained_hotstuff_commits_after_primary_kill() {
    hotstuff_primary_kill_smoke(true);
}

fn hotstuff_primary_kill_smoke(chained: bool) {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let base_data =
        std::env::temp_dir().join(format!("light-dex-hotstuff-{}", rand::random::<u64>()));
    let controller = ControllerState::default();
    controller
        .launch(LaunchConfig {
            project_dir: project.display().to_string(),
            binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(),
            recover_existing: false,
            remote_placements: vec![],
            node_count: 4,
            tx_batch_window_ms: 50,
            block_time_ms: 200,
            timeout_ms: 1500,
            auto_view_change_blocks: None,
            poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus: crate::types::ConsensusAlgorithm::Hotstuff,
            chained_hotstuff: chained,
            claim_limit: 1_000,
            claim_usd: 1_000_000,
            claim_btc: 1_000,
        })
        .unwrap();
    wait_for(&controller, |s| {
        s.running && s.metrics.responsive_nodes == 4 && s.metrics.height_min == Some(1)
    });
    controller.claim_new_accounts(3).unwrap();
    controller
        .submit_transaction(
            Transaction::Transfer {
                from: wallet_addr(&controller, 0),
                to: wallet_addr(&controller, 1),
                asset: 0,
                amount: 1,
                nonce: 1,
            },
            TargetNode::Random,
        )
        .unwrap();
    wait_for(&controller, |s| {
        s.nodes
            .iter()
            .filter_map(|node| node.status.as_ref())
            .all(|status| {
                status.account_nonces.iter().any(|account| {
                    account.account_id == wallet_addr(&controller, 0) && account.nonce == 2
                })
            })
    });
    controller
        .kill_node(TargetNode::Node { node_id: 0 })
        .unwrap();
    controller
        .submit_transaction(
            Transaction::Transfer {
                from: wallet_addr(&controller, 1),
                to: wallet_addr(&controller, 2),
                asset: 0,
                amount: 1,
                nonce: 1,
            },
            TargetNode::Node { node_id: 2 },
        )
        .unwrap();
    let recovered = wait_for(&controller, |s| {
        s.metrics.responsive_nodes == 3
            && s.nodes
                .iter()
                .filter(|node| node.state == "ok")
                .filter_map(|node| node.status.as_ref())
                .all(|status| status.chain_height >= 4 && status.current_view > 0)
    });
    assert_eq!(recovered.metrics.chain_consistency, "consistent");
    controller.stop_all();
    wait_for(&controller, |s| {
        !s.running && !s.launching && !s.stopping && s.metrics.alive_nodes == 0
    });
    std::fs::remove_dir_all(base_data).unwrap();
}

#[test]
#[ignore = "spawns seven node processes and requires a built light_dex binary"]
fn hotstuff_two_paused_replicas_rejoin() {
    hotstuff_paused_rejoin_smoke(false);
}

#[test]
#[ignore = "spawns seven node processes and requires a built light_dex binary"]
fn chained_hotstuff_two_paused_replicas_rejoin() {
    hotstuff_paused_rejoin_smoke(true);
}

fn hotstuff_paused_rejoin_smoke(chained: bool) {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let base_data =
        std::env::temp_dir().join(format!("light-dex-hotstuff-sync-{}", rand::random::<u64>()));
    let controller = ControllerState::default();
    controller
        .launch(LaunchConfig {
            project_dir: project.display().to_string(),
            binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(),
            recover_existing: false,
            remote_placements: vec![],
            node_count: 7,
            tx_batch_window_ms: 50,
            block_time_ms: 150,
            timeout_ms: 3000,
            auto_view_change_blocks: None,
            poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus: crate::types::ConsensusAlgorithm::Hotstuff,
            chained_hotstuff: chained,
            claim_limit: 1_000,
            claim_usd: 1_000_000,
            claim_btc: 1_000,
        })
        .unwrap();
    wait_for(&controller, |s| {
        s.running && s.metrics.responsive_nodes == 7 && s.metrics.height_min == Some(1)
    });
    controller.claim_new_accounts(14).unwrap();
    for node_id in [5, 6] {
        controller
            .pause_node(TargetNode::Node { node_id }, 8_000)
            .unwrap();
    }
    wait_for(&controller, |s| {
        [5, 6].iter().all(|id| {
            s.nodes
                .iter()
                .any(|node| node.node_id == *id && node.state == "paused")
        })
    });
    for account in 0..4 {
        controller
            .submit_transaction(
                Transaction::Transfer {
                    from: wallet_addr(&controller, account),
                    to: wallet_addr(&controller, account + 10),
                    asset: 0,
                    amount: 1,
                    nonce: 1,
                },
                TargetNode::Node { node_id: 0 },
            )
            .unwrap();
        let height = account as usize + 2;
        wait_for(&controller, |s| {
            s.nodes.iter().filter(|node| node.node_id < 5).all(|node| {
                node.status
                    .as_ref()
                    .is_some_and(|status| status.chain_height >= height)
            })
        });
    }
    let caught_up = wait_for(&controller, |s| {
        s.metrics.responsive_nodes == 7
            && s.metrics.chain_consistency == "consistent"
            && s.nodes.iter().all(|node| {
                node.status
                    .as_ref()
                    .is_some_and(|status| status.chain_height >= 5)
            })
    });
    assert_eq!(caught_up.metrics.chain_consistency, "consistent");
    if chained {
        let sender = wallet_addr(&controller, 0);
        controller
            .submit_transaction(
                Transaction::Transfer {
                    from: sender,
                    to: wallet_addr(&controller, 10),
                    asset: 0,
                    amount: 1,
                    nonce: 2,
                },
                TargetNode::Node { node_id: 0 },
            )
            .unwrap();
        let resumed = wait_for(&controller, |snapshot| {
            snapshot.metrics.responsive_nodes == 7
                && snapshot.metrics.chain_consistency == "consistent"
                && snapshot
                    .nodes
                    .iter()
                    .filter_map(|node| node.status.as_ref())
                    .all(|status| {
                        status
                            .account_nonces
                            .iter()
                            .any(|account| account.account_id == sender && account.nonce == 3)
                    })
        });
        assert_eq!(resumed.metrics.chain_consistency, "consistent");
    }
    controller.stop_all();
    wait_for(&controller, |s| {
        !s.running && !s.launching && !s.stopping && s.metrics.alive_nodes == 0
    });
    std::fs::remove_dir_all(base_data).unwrap();
}

#[test]
#[ignore = "spawns a HotStuff cluster and thirteen participants"]
fn hotstuff_thirteen_participants_stay_live() {
    struct StopOnDrop(ControllerState);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.stop_all();
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                let snapshot = self.0.snapshot();
                if !snapshot.running
                    && !snapshot.launching
                    && !snapshot.stopping
                    && snapshot.metrics.alive_nodes == 0
                {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let node_count = std::env::var("LIGHT_DEX_FLEET_TEST_NODES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(7);
    let binary_path = std::env::var("LIGHT_DEX_FLEET_TEST_BINARY")
        .unwrap_or_else(|_| "target/debug/light_dex".into());
    let base_data = std::env::temp_dir().join(format!(
        "light-dex-hotstuff-fleet-{}",
        rand::random::<u64>()
    ));
    let controller = ControllerState::default();
    let _guard = StopOnDrop(controller.clone());
    controller
        .launch(LaunchConfig {
            project_dir: project.display().to_string(),
            binary_path,
            data_dir: base_data.display().to_string(),
            recover_existing: false,
            remote_placements: vec![],
            node_count,
            tx_batch_window_ms: 50,
            block_time_ms: 500,
            timeout_ms: 1_500,
            auto_view_change_blocks: None,
            poll_ms: 200,
            poll_mode: crate::types::PollMode::RoundRobin,
            build_before_launch: false,
            consensus: crate::types::ConsensusAlgorithm::Hotstuff,
            chained_hotstuff: false,
            claim_limit: 1_000,
            claim_usd: 1_000_000,
            claim_btc: 1_000,
        })
        .unwrap();
    wait_for(&controller, |s| {
        s.running
            && s.metrics.responsive_nodes == node_count as usize
            && s.metrics.height_min == Some(1)
    });
    {
        let mut state = controller.inner.lock().unwrap();
        state.feed_started = true;
        state.feed_depth = Some(PartialDepth {
            last_update_id: 1,
            bids: vec![
                ["100000".into(), "1".into()],
                ["99999".into(), "1".into()],
                ["99998".into(), "1".into()],
            ],
            asks: vec![
                ["100010".into(), "1".into()],
                ["100011".into(), "1".into()],
                ["100012".into(), "1".into()],
            ],
        });
        state.feed_at = Some(Instant::now());
        state.feed_history.push_back((Instant::now(), 100_005.0));
    }
    let mut fleet = crate::participant_fleet_template();
    let wallets = controller.claim_new_accounts(fleet.len() as u64).unwrap();
    for (agent, wallet) in fleet.iter_mut().zip(wallets) {
        agent.account_id = wallet.label;
    }
    controller.start_agents(fleet).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        {
            let mut state = controller.inner.lock().unwrap();
            state.feed_at = Some(Instant::now());
        }
        let snapshot = controller.snapshot();
        assert_eq!(
            snapshot.metrics.alive_nodes,
            node_count as usize,
            "logs: {:?}",
            snapshot.logs.iter().rev().take(20).collect::<Vec<_>>()
        );
        assert_eq!(snapshot.agents.len(), 13);
        thread::sleep(Duration::from_millis(200));
    }
    let snapshot = controller.snapshot();
    eprintln!(
        "HotStuff fleet under load: height {:?}-{:?}, responsive {}, submitted {}, committed {:?}",
        snapshot.metrics.height_min,
        snapshot.metrics.height_max,
        snapshot.metrics.responsive_nodes,
        snapshot.metrics.submitted,
        snapshot
            .nodes
            .iter()
            .filter_map(|node| node
                .status
                .as_ref()
                .map(|status| status.transactions_committed))
            .max()
    );
    controller.stop_agents((1..=13).collect()).unwrap();
    let settled = wait_for(&controller, |s| {
        s.agents.is_empty()
            && s.metrics.responsive_nodes == node_count as usize
            && shared_block_at_min_height(s)
    });
    eprintln!(
        "HotStuff fleet settled: height {:?}, all {} nodes consistent",
        settled.metrics.height_min, settled.metrics.responsive_nodes
    );
    controller.stop_all();
    wait_for(&controller, |s| {
        !s.running && !s.launching && !s.stopping && s.metrics.alive_nodes == 0
    });
    std::fs::remove_dir_all(base_data).unwrap();
    assert!(snapshot.metrics.submitted > 0);
    assert!(snapshot.metrics.height_min.unwrap_or_default() > 1);
    assert_eq!(snapshot.metrics.responsive_nodes, node_count as usize);
}

/// Round-robin status samples need not have identical heights while HotStuff
/// keeps emitting empty flush blocks. Compare a height present on every node.
fn shared_block_at_min_height(snapshot: &ControlSnapshot) -> bool {
    let Some(sequence) = snapshot
        .metrics
        .height_min
        .and_then(|height| height.checked_sub(1))
    else {
        return false;
    };
    let mut common = None;
    for node in &snapshot.nodes {
        let Some(block) = node.status.as_ref().and_then(|status| {
            status
                .recent_blocks
                .iter()
                .find(|block| block.sequence == sequence as u64)
        }) else {
            return false;
        };
        if let Some(digest) = common {
            if digest != &block.digest {
                return false;
            }
        } else {
            common = Some(&block.digest);
        }
    }
    common.is_some()
}

#[test]
#[ignore = "requires live Binance feed and spawns node processes"]
fn market_maker_quotes_live_depth_and_cancels_on_stop() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let base_data = std::env::temp_dir().join(format!("light-dex-maker-{}", rand::random::<u64>()));
    let controller = ControllerState::default();
    controller
        .launch(LaunchConfig {
            project_dir: project.display().to_string(),
            binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(),
            recover_existing: false,
            remote_placements: vec![],
            node_count: 4,
            tx_batch_window_ms: 50,
            block_time_ms: 100,
            timeout_ms: 1500,
            auto_view_change_blocks: None,
            poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus: crate::types::ConsensusAlgorithm::Pbft,
            chained_hotstuff: false,
            claim_limit: 1_000,
            claim_usd: 1_000_000,
            claim_btc: 1_000,
        })
        .unwrap();
    wait_for(&controller, |snapshot| {
        snapshot.running
            && snapshot.metrics.responsive_nodes == 4
            && snapshot.metrics.height_min == Some(1)
    });
    controller.claim_new_accounts(2).unwrap();
    controller
        .start_market_maker(MarketMakerConfig {
            account_id: 0,
            levels: 1,
            refresh_ms: 500,
            target: TargetNode::Node { node_id: 0 },
        })
        .unwrap();
    let quoted = wait_for(&controller, |snapshot| {
        snapshot.market_maker.last_update_id.is_some() && snapshot.market_maker.own_open_orders >= 2
    });
    assert!(
        quoted.market_maker.desired_bids.len() == 1 && quoted.market_maker.desired_asks.len() == 1
    );
    controller
        .start_market_maker(MarketMakerConfig {
            account_id: 0,
            levels: 2,
            refresh_ms: 750,
            target: TargetNode::Node { node_id: 0 },
        })
        .unwrap();
    wait_for(&controller, |snapshot| {
        snapshot.market_maker.desired_bids.len() == 2
            && snapshot.market_maker.desired_asks.len() == 2
            && snapshot.market_maker.own_open_orders >= 4
    });
    controller
        .start_load(LoadConfig {
            preset: LoadPreset::BookTaker {
                account_id: 1,
                market_id: 0,
                side: light_dex::exchange::order::Side::Buy,
            },
            target: TargetNode::Node { node_id: 0 },
            tps: 1.0,
        })
        .unwrap();
    wait_for(&controller, |snapshot| {
        snapshot
            .nodes
            .iter()
            .filter(|node| node.state == "ok")
            .filter_map(|node| node.status.as_ref())
            .any(|status| {
                status.account_nonces.iter().any(|account| {
                    account.account_id == wallet_addr(&controller, 1) && account.nonce > 1
                })
            })
    });
    controller.stop_load();
    controller.stop_market_maker();
    let cleared = wait_for(&controller, |snapshot| {
        !snapshot.market_maker.running
            && !snapshot.market_maker.stopping
            && snapshot.market_maker.own_open_orders == 0
    });
    controller.stop_all();
    wait_for(&controller, |snapshot| {
        !snapshot.running
            && !snapshot.launching
            && !snapshot.stopping
            && snapshot.metrics.alive_nodes == 0
    });
    assert!(cleared.metrics.height_min.unwrap_or_default() >= 3);
    std::fs::remove_dir_all(base_data).unwrap();
}

fn wallet_addr(controller: &ControllerState, label: u64) -> Address {
    controller.inner.lock().unwrap().wallet[&label]
}

fn wait_for(
    controller: &ControllerState,
    predicate: impl Fn(&ControlSnapshot) -> bool,
) -> ControlSnapshot {
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let snapshot = controller.snapshot();
        if predicate(&snapshot) {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for cluster; logs: {:?}; nodes: {:?}; agents: {:?}; consistency: {}",
            snapshot.logs.iter().rev().take(30).collect::<Vec<_>>(),
            snapshot
                .nodes
                .iter()
                .map(|node| (
                    node.node_id,
                    node.state.clone(),
                    node.status.as_ref().map(|status| (
                        status.chain_height,
                        status.current_view,
                        status.mempool_size,
                    ))
                ))
                .collect::<Vec<_>>(),
            snapshot
                .agents
                .iter()
                .map(|agent| (
                    agent.agent_id,
                    agent.state.as_str(),
                    agent.pending,
                    agent.own_open_orders,
                    agent.last_error.as_deref()
                ))
                .collect::<Vec<_>>(),
            snapshot.metrics.chain_consistency,
        );
        thread::sleep(Duration::from_millis(100));
    }
}
