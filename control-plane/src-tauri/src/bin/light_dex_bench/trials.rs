use super::*;

pub(super) fn trials(smoke: bool) -> Vec<Trial> {
    if smoke {
        return vec![Trial {
            name: "smoke_hotstuff_4".into(),
            algorithm: Algorithm::Hotstuff,
            nodes: 4,
            timeout_ms: 1500,
            offered_tps: 0,
        }];
    }
    let mut trials = Vec::new();
    for nodes in [4, 7, 13] {
        for algorithm in [Algorithm::Pbft, Algorithm::Hotstuff] {
            trials.push(Trial {
                name: format!("scale_{}_{}", algorithm.label(), nodes),
                algorithm,
                nodes,
                timeout_ms: 1500,
                offered_tps: 0,
            });
        }
    }
    for timeout_ms in [200, 400, 800, 1500, 3000] {
        for algorithm in [Algorithm::Pbft, Algorithm::Hotstuff] {
            trials.push(Trial {
                name: format!("timeout_{}_{}", algorithm.label(), timeout_ms),
                algorithm,
                nodes: 7,
                timeout_ms,
                offered_tps: 0,
            });
        }
    }
    for offered_tps in [15, 40, 80, 160] {
        for algorithm in [Algorithm::Pbft, Algorithm::Hotstuff] {
            trials.push(Trial {
                name: format!("stress_{}_{}", algorithm.label(), offered_tps),
                algorithm,
                nodes: 7,
                timeout_ms: 1500,
                offered_tps,
            });
        }
    }
    for algorithm in [Algorithm::Pbft, Algorithm::Hotstuff] {
        trials.push(Trial {
            name: format!("repeat_{}", algorithm.label()),
            algorithm,
            nodes: 7,
            timeout_ms: 1500,
            offered_tps: 0,
        });
    }
    trials
}

pub(super) fn failed_summary(
    trial: &Trial,
    reason: String,
    workload: Workload,
    window: usize,
    participants: usize,
) -> Summary {
    Summary {
        latency_target_ms: 500,
        latency_slo_met: None,
        end_to_end_p50_ms: None,
        end_to_end_p95_ms: None,
        end_to_end_samples: 0,
        end_to_end_unmatched: 0,
        scheduled_slots: 0,
        blocked_slots: 0,
        unaccounted_slots: 0,
        scheduled_to_commit_p50_ms: None,
        scheduled_to_commit_p95_ms: None,
        offered_submissions: 0,
        rejected_initial_submissions: 0,
        rejected_submission_reasons: BTreeMap::new(),
        admission_p95_ms: None,
        observed_confirmation_p95_ms: None,
        observed_confirmation_samples: 0,
        unresolved_after_drain: 0,
        name: trial.name.clone(),
        algorithm: trial.algorithm,
        nodes: trial.nodes,
        timeout_ms: trial.timeout_ms,
        offered_tps: trial.offered_tps,
        participants,
        driver_accounts: 100usize.saturating_sub(participants).min(70),
        workload,
        in_flight_per_account: window,
        result: "invalid".into(),
        reason: Some(reason),
        measured_seconds: 0.0,
        attempted_extra: 0,
        delivered_extra_tps: 0.0,
        min_responsive_during_measurement: 0,
        repair_submissions: 0,
        retry_submissions: 0,
        pending_at_end: 0,
        submitted_writes: 0,
        successful_tps: 0.0,
        accepted_order_actions_per_second: 0.0,
        included_quote_batches: 0,
        failed_tps: 0.0,
        trades_per_second: 0.0,
        included_transfers: 0,
        included_places: 0,
        included_cancels: 0,
        included_order_transactions_by_market: BTreeMap::new(),
        per_market_outcomes: BTreeMap::new(),
        exact_failure_reasons: BTreeMap::new(),
        trace_outcome_rows: 0,
        trace_rows_expected: 0,
        included_oracle_updates_by_base_asset: BTreeMap::new(),
        observed_failure_reasons: BTreeMap::new(),
        active_accounts: 0,
        open_orders_at_measure_end: 0,
        latency_p50_ms: None,
        latency_p95_ms: None,
        latency_sample_count: 0,
        height_min: None,
        height_max: None,
        view_changes: 0,
        catch_up_events: 0,
        max_backlog: 0,
        peak_rss_kib: 0,
        final_consistency: "unknown".into(),
        data_dir: None,
    }
}

pub(super) fn run_trial(
    trial: &Trial,
    project: &Path,
    run_dir: &Path,
    warmup: Duration,
    measure: Duration,
    workload: Workload,
    window: usize,
    participants: usize,
) -> Result<(Summary, Vec<Sample>), String> {
    let controller = ControllerState::default();
    let _guard = ClusterGuard(controller.clone());
    controller.launch(LaunchConfig {
        project_dir: project.display().to_string(),
        binary_path: arg("--node-binary").unwrap_or_else(|| "target/release/light_dex".into()),
        data_dir: run_dir.join("chains").display().to_string(),
        recover_existing: false,
        remote_placements: remote_placements_arg()?,
        node_count: trial.nodes,
        block_time_ms: arg("--block-time-ms")
            .map(|value| value.parse::<u64>().map_err(|error| error.to_string()))
            .transpose()?
            .unwrap_or(500),
        tx_batch_window_ms: arg("--tx-batch-window-ms")
            .map(|value| value.parse::<u64>().map_err(|error| error.to_string()))
            .transpose()?
            .unwrap_or(
                if matches!(trial.algorithm, Algorithm::Hotstuff2 | Algorithm::Votor) {
                    0
                } else {
                    50
                },
            ),
        timeout_ms: trial.timeout_ms,
        auto_view_change_blocks: arg("--auto-view-change-blocks")
            .map(|value| value.parse::<u64>().map_err(|error| error.to_string()))
            .transpose()?
            .or(Some(4))
            .filter(|blocks| *blocks > 0),
        poll_ms: 200,
        poll_mode: match arg("--poll-mode").as_deref().unwrap_or("round-robin") {
            "all" => PollMode::All,
            "round-robin" => PollMode::RoundRobin,
            "pinned" => PollMode::Pinned,
            _ => return Err("poll mode must be all, round-robin, or pinned".into()),
        },
        build_before_launch: false,
        consensus: trial.algorithm.config(),
        chained_hotstuff: matches!(trial.algorithm, Algorithm::Hotstuff)
            && !std::env::args().any(|arg| arg == "--classic-hotstuff"),
        claim_limit: 1_000,
        // Sustain the multi-market driver for the full measurement without
        // its test wallets exhausting USD or base inventory mid-run.
        claim_usd: if matches!(workload, Workload::Multimarket | Workload::BalancedSix) {
            1_000_000_000
        } else {
            1_000_000
        },
        claim_btc: if matches!(workload, Workload::Multimarket | Workload::BalancedSix) {
            100_000
        } else {
            1_000
        },
    })?;
    wait_for(
        &controller,
        Instant::now()
            + Duration::from_secs(if remote_placements_arg()?.is_empty() {
                40
            } else {
                150
            }),
        |s| {
            s.running
                && s.nodes.len() == trial.nodes as usize
                && s.nodes.iter().all(|node| {
                    node.state == "ok"
                        && node
                            .status
                            .as_ref()
                            .is_some_and(|status| status.chain_height >= 1)
                })
        },
    )?;
    if let Some(cores) = arg("--pin-node-cores") {
        pin_nodes(&controller.snapshot(), &cores, run_dir)?;
    }
    let mut fleet = participant_fleet(participants);
    if matches!(workload, Workload::Multimarket | Workload::BalancedSix) {
        // All six BTC/ETH/SOL spot and perp books must receive independent
        // order flow. Non-BTC makers/history adapters are currently BTC-only,
        // so use supported random orders in those lanes.
        for (index, agent) in fleet.iter_mut().enumerate() {
            agent.market_id = [0, 1, 2, 3, 8, 9][index % 6];
            agent.wait_for_confirmation = false;
            agent.strategy = AgentStrategy::RandomOrder {
                order_type: if (index / 6) % 2 == 0 {
                    OrderType::LO
                } else {
                    OrderType::IOC
                },
                tps: 2.0,
                passive_offset_ticks: 5,
                max_resting_orders: 8,
                resting_ttl_ms: 10_000,
            };
        }
    }
    let driver_count = arg("--driver-accounts")
        .map(|value| value.parse::<usize>().map_err(|error| error.to_string()))
        .transpose()?
        .unwrap_or_else(|| 100usize.saturating_sub(participants).min(70));
    if driver_count > 500 || participants + driver_count > 1_000 {
        return Err("driver accounts must be <= 500 and total accounts <= 1,000".into());
    }
    if workload == Workload::BalancedSix && (driver_count < 12 || driver_count % 12 != 0) {
        return Err("balanced-six needs a multiple of 12 driver accounts (at least 12)".into());
    }
    let mut wallets = Vec::with_capacity(participants + driver_count);
    for batch in (0..participants + driver_count).step_by(100) {
        wallets.extend(
            controller.claim_new_accounts((participants + driver_count - batch).min(100) as u64)?,
        );
    }
    for (agent, wallet) in fleet.iter_mut().zip(&wallets) {
        agent.account_id = wallet.label;
    }
    let driver_accounts = wallets
        .iter()
        .skip(participants)
        .map(|wallet| wallet.label)
        .collect::<Vec<_>>();
    if workload == Workload::BalancedSix {
        prepare_balanced_perp_accounts(&controller, &wallets[participants..])?;
    }
    if matches!(workload, Workload::Mixed) {
        controller.watch_order_accounts(
            wallets
                .iter()
                .skip(participants)
                .map(|wallet| wallet.address)
                .collect(),
        )?;
    }
    for batch in fleet.chunks(100) {
        controller.start_agents(batch.to_vec())?;
    }
    thread::sleep(warmup);
    let warm = controller.snapshot();
    let needs_depth_feed = fleet.iter().any(|agent| agent.strategy.needs_binance());
    let fresh = !needs_depth_feed
        || warm
            .agents
            .iter()
            .any(|agent| agent.feed_age_ms.is_some_and(|age| age < 3000));
    if !fresh || (!fleet.is_empty() && !warm.agents.iter().any(|agent| agent.own_open_orders > 0)) {
        let ages = warm
            .agents
            .iter()
            .filter_map(|agent| agent.feed_age_ms)
            .take(4)
            .collect::<Vec<_>>();
        let orders = warm
            .agents
            .iter()
            .map(|agent| agent.own_open_orders)
            .sum::<usize>();
        let logs = warm.logs.iter().rev().take(6).cloned().collect::<Vec<_>>();
        return Err(format!(
            "warm-up failed: feed_fresh={fresh}, feed_ages_ms={ages:?}, own_open_orders={orders}, logs={logs:?}"
        ));
    }
    let baseline = best_status(&warm)
        .ok_or("no responsive node after warm-up")?
        .clone();
    let start_submitted = warm.metrics.submitted;
    let mut driver = ExtraDriver::new(workload, window, driver_accounts.clone());
    driver.retry_after = Duration::from_millis(retry_after_ms()?);
    driver.wallet = warm
        .wallet
        .iter()
        .map(|account| (account.label, account.address))
        .collect();
    let start = Instant::now();
    let measure_deadline = start + measure;
    driver.schedule_origin = start;
    driver.next_at = start;
    let mut samples = Vec::new();
    let mut failure_digests = HashSet::<String>::new();
    let mut observed_failure_reasons = BTreeMap::<String, u64>::new();
    let mut last_sample = Instant::now() - Duration::from_secs(1);
    while start.elapsed() < measure {
        if let Some(status) = controller.benchmark_status() {
            driver.step_status(
                &controller,
                &status,
                trial.offered_tps,
                Some(measure_deadline),
            );
        }
        if last_sample.elapsed() >= Duration::from_millis(500) {
            let snapshot = controller.snapshot();
            if let Some(status) = best_status(&snapshot) {
                for outcome in &status.recent_transaction_outcomes {
                    if !outcome.success && failure_digests.insert(outcome.digest.clone()) {
                        *observed_failure_reasons
                            .entry(outcome.error.clone().unwrap_or_else(|| "unknown".into()))
                            .or_default() += 1;
                    }
                }
            }
            samples.push(sample(&snapshot, start.elapsed(), driver.attempted));
            last_sample = Instant::now();
        }
        thread::sleep(Duration::from_millis(10));
    }
    if let Some(status) = controller.benchmark_status() {
        // Account for every scheduled slot before the exact measurement
        // deadline, even if the 10 ms outer poll ended a few ms early.
        driver.step_status(
            &controller,
            &status,
            trial.offered_tps,
            Some(measure_deadline),
        );
    }
    let end = controller.snapshot();
    let measured_seconds = start.elapsed().as_secs_f64().max(0.001);
    // The measurement loop stops at `measure`; snapshot/cleanup time after
    // that boundary must not create imaginary scheduled submissions.
    let expected_slots = (measure.as_secs_f64() * trial.offered_tps as f64).ceil() as u64;
    let unaccounted_slots = expected_slots.saturating_sub(driver.scheduled_slots);
    samples.push(sample(&end, start.elapsed(), driver.attempted));
    let pending_at_end = driver.pending_count();
    let repair_submissions = driver.repair_submissions;
    let retry_submissions = driver.retry_submissions;
    let end_status = best_status(&end).ok_or_else(|| {
        format!(
            "no responsive node at measurement end; states={:?}; recent_logs={:?}",
            end.nodes
                .iter()
                .map(|node| (node.node_id, node.state.as_str()))
                .collect::<Vec<_>>(),
            end.logs.iter().rev().take(8).collect::<Vec<_>>(),
        )
    })?;
    let successful = end_status
        .transactions_committed
        .saturating_sub(baseline.transactions_committed);
    let failed = end_status
        .transactions_failed
        .saturating_sub(baseline.transactions_failed);
    let trades = end_status
        .trades_committed
        .saturating_sub(baseline.trades_committed);
    let mut latencies = end_status.recent_commit_latency_ms.clone();
    if latencies.len() >= baseline.recent_commit_latency_ms.len()
        && latencies.starts_with(&baseline.recent_commit_latency_ms)
    {
        latencies.drain(..baseline.recent_commit_latency_ms.len());
    }
    let agent_ids = end
        .agents
        .iter()
        .map(|agent| agent.agent_id)
        .collect::<Vec<_>>();
    controller.stop_agents(agent_ids)?;
    let drain_deadline = Instant::now() + Duration::from_secs(15);
    let drained = loop {
        let current = controller.snapshot();
        driver.step(&controller, &current, 0);
        if driver.pending_count() == 0
            && current.agents.is_empty()
            && current.metrics.responsive_nodes == trial.nodes as usize
            && current.metrics.chain_consistency == "consistent"
            && current.metrics.height_min == current.metrics.height_max
        {
            break current;
        }
        if Instant::now() >= drain_deadline {
            break current;
        }
        thread::sleep(Duration::from_millis(100));
    };
    let live_verdict = correctness(&drained, trial.nodes);
    // Replay only after every validator has closed its journal. Replaying a
    // live file races persistence and can report a spurious mismatch.
    controller.stop_all();
    wait_for(
        &controller,
        Instant::now()
            + Duration::from_secs(if remote_placements_arg()?.is_empty() {
                15
            } else {
                90
            }),
        |s| !s.stopping && s.nodes.iter().all(|node| node.state == "dead"),
    )?;
    let archive_result = wait_for_remote_archive(&drained);
    let verdict = live_verdict
        .and_then(|_| archive_result)
        .and_then(|_| persisted_correctness(&drained, trial.nodes));
    let (mut included_transfers, mut included_places, mut included_cancels) = (0, 0, 0);
    let mut included_quote_batches = 0;
    let mut included_order_transactions_by_market = BTreeMap::<u64, u64>::new();
    let mut trace_rows_expected = 0u64;
    let mut included_oracle_updates_by_base_asset = BTreeMap::<u64, u64>::new();
    let mut active_accounts = std::collections::HashSet::new();
    if let Some(data_dir) = &drained.active_data_dir {
        let manifest = Arc::new(
            SessionManifest::load(&Path::new(data_dir).join("manifest.json"))
                .map_err(|error| error.to_string())?,
        );
        let chain =
            BlockchainState::load_signed(Path::new(data_dir).join("node-0.json"), manifest)?;
        for block in chain.blocks.iter().skip(baseline.chain_height).take(
            end_status
                .chain_height
                .saturating_sub(baseline.chain_height),
        ) {
            trace_rows_expected += block.transactions.len() as u64;
            for update in &block.oracle_updates {
                *included_oracle_updates_by_base_asset
                    .entry(update.pair.base_asset)
                    .or_default() += 1;
            }
            for tx in &block.transactions {
                if matches!(
                    tx,
                    Transaction::PlaceOrder { .. }
                        | Transaction::PlacePerpOrder { .. }
                        | Transaction::CancelOrder { .. }
                        | Transaction::ReduceOrder { .. }
                        | Transaction::UpdateQuotes { .. }
                ) {
                    if let Some(market_id) = tx.market_id() {
                        *included_order_transactions_by_market
                            .entry(market_id)
                            .or_default() += 1;
                    }
                }
                active_accounts.insert(tx.account_id());
                match tx {
                    Transaction::UpdateQuotes { cancel, place, .. } => {
                        included_quote_batches += 1;
                        included_places += place.len() as u64;
                        included_cancels += cancel.len() as u64;
                    }
                    Transaction::ClaimAirdrop { .. } => {}
                    Transaction::Transfer { .. } => included_transfers += 1,
                    Transaction::PlaceOrder { .. } | Transaction::PlacePerpOrder { .. } => {
                        included_places += 1
                    }
                    Transaction::CancelOrder { .. } => included_cancels += 1,
                    Transaction::DepositPerpCollateral { .. }
                    | Transaction::WithdrawPerpCollateral { .. }
                    | Transaction::SetMarginMode { .. }
                    | Transaction::ReduceOrder { .. }
                    | Transaction::LiquidatePerp { .. } => {}
                }
            }
        }
    }
    let (per_market_outcomes, exact_failure_reasons, trace_outcome_rows) = market_trace_outcomes(
        drained.active_data_dir.as_deref(),
        baseline.chain_height as u64,
        end_status.chain_height as u64,
    )?;
    let feed_stale = needs_depth_feed
        && samples
            .iter()
            .any(|s| s.feed_age_ms.is_none_or(|age| age > 3000));
    let min_responsive = samples
        .iter()
        .map(|sample| {
            sample
                .nodes
                .iter()
                .filter(|node| node.state == "ok")
                .count()
        })
        .min()
        .unwrap_or(0);
    let delivered_extra_tps = driver.attempted as f64 / measured_seconds;
    let reason = verdict
        .as_ref().err().cloned()
        .or_else(|| (workload == Workload::BalancedSix && trace_outcome_rows != trace_rows_expected)
            .then(|| format!("market outcome trace incomplete: {trace_outcome_rows}/{trace_rows_expected}")))
        .or_else(|| (workload == Workload::BalancedSix && [0, 1, 2, 3, 8, 9].iter().any(|market| {
            per_market_outcomes.get(market).is_none_or(|outcome| outcome.successful == 0 || outcome.fills == 0)
        })).then(|| "one or more spot/perp markets had no successful filled transactions".into()))
        .or_else(|| driver.fatal_error.clone())
        // Cleanup is outside the measured interval. In-flight writes at
        // shutdown are reported as pending_at_end, not mistaken for a
        // consensus correctness failure; persisted replay is authoritative.
        .or_else(|| feed_stale.then(|| "Binance feed stale during measurement".into()))
        .or_else(|| (trial.name.starts_with("capacity_") && min_responsive < trial.nodes as usize)
            .then(|| format!("only {min_responsive}/{} nodes responsive in at least one measurement sample", trial.nodes)))
        .or_else(|| (trial.name.starts_with("capacity_") && trial.offered_tps > 0
            && delivered_extra_tps < trial.offered_tps as f64 * 0.95)
            .then(|| format!("driver delivered {delivered_extra_tps:.1} of {} requested extra TPS", trial.offered_tps)));
    let seconds = measured_seconds;
    let e2e = commit_trace_latencies(
        drained.active_data_dir.as_deref(),
        &driver.first_submissions,
    )?;
    let scheduled_e2e = commit_trace_latencies(
        drained.active_data_dir.as_deref(),
        &driver.scheduled_submissions,
    )?;
    let mut raw_submissions = driver
        .scheduled_submissions
        .iter()
        .filter_map(|(digest, scheduled)| {
            Some(SubmissionRecord {
                digest: digest.clone(),
                scheduled_unix_us: *scheduled,
                submitted_unix_us: *driver.first_submissions.get(digest)?,
            })
        })
        .collect::<Vec<_>>();
    raw_submissions.sort_by_key(|record| record.scheduled_unix_us);
    fs::write(
        run_dir.join("driver-submissions.json"),
        serde_json::to_vec(&raw_submissions).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let explicit_latency_target = arg("--latency-target-ms");
    let latency_target_ms = explicit_latency_target
        .as_deref()
        .map(str::parse::<u64>)
        .transpose()
        .map_err(|e| e.to_string())?
        .unwrap_or(500);
    let evaluate_latency_target =
        explicit_latency_target.is_some() || remote_placements_arg()?.is_empty();
    let summary = Summary {
        latency_target_ms,
        latency_slo_met: if evaluate_latency_target && e2e.len() == driver.first_submissions.len() {
            percentile(&scheduled_e2e, 0.95).map(|p95| {
                p95 <= latency_target_ms
                    && driver.blocked_slots == 0
                    && unaccounted_slots == 0
                    && driver.scheduled_slots == driver.offered
            })
        } else {
            None
        },
        end_to_end_p50_ms: percentile(&e2e, 0.50),
        end_to_end_p95_ms: percentile(&e2e, 0.95),
        end_to_end_samples: e2e.len(),
        end_to_end_unmatched: driver.first_submissions.len().saturating_sub(e2e.len()),
        scheduled_slots: driver.scheduled_slots,
        blocked_slots: driver.blocked_slots,
        unaccounted_slots,
        scheduled_to_commit_p50_ms: percentile(&scheduled_e2e, 0.50),
        scheduled_to_commit_p95_ms: percentile(&scheduled_e2e, 0.95),
        offered_submissions: driver.offered,
        rejected_initial_submissions: driver.rejected,
        rejected_submission_reasons: driver.rejected_reasons.clone(),
        admission_p95_ms: percentile(&driver.admission_latency_ms, 0.95),
        observed_confirmation_p95_ms: percentile(&driver.observed_confirmation_ms, 0.95),
        observed_confirmation_samples: driver.observed_confirmation_ms.len(),
        unresolved_after_drain: driver.pending_count(),
        name: trial.name.clone(),
        algorithm: trial.algorithm,
        nodes: trial.nodes,
        timeout_ms: trial.timeout_ms,
        offered_tps: trial.offered_tps,
        participants,
        driver_accounts: driver_accounts.len(),
        workload,
        in_flight_per_account: window,
        result: if reason.is_some() { "invalid" } else { "pass" }.into(),
        reason,
        measured_seconds: seconds,
        attempted_extra: driver.attempted,
        delivered_extra_tps,
        min_responsive_during_measurement: min_responsive,
        repair_submissions,
        retry_submissions,
        pending_at_end,
        submitted_writes: end.metrics.submitted.saturating_sub(start_submitted),
        successful_tps: successful as f64 / seconds,
        accepted_order_actions_per_second: end_status
            .order_actions_committed
            .saturating_sub(baseline.order_actions_committed)
            as f64
            / seconds,
        included_quote_batches,
        failed_tps: failed as f64 / seconds,
        trades_per_second: trades as f64 / seconds,
        included_transfers,
        included_places,
        included_cancels,
        included_order_transactions_by_market,
        per_market_outcomes,
        exact_failure_reasons,
        trace_outcome_rows,
        trace_rows_expected,
        included_oracle_updates_by_base_asset,
        observed_failure_reasons,
        active_accounts: active_accounts.len(),
        open_orders_at_measure_end: end_status.total_open_orders,
        latency_p50_ms: percentile(&latencies, 0.50),
        latency_p95_ms: percentile(&latencies, 0.95),
        latency_sample_count: latencies.len(),
        height_min: verdict.as_ref().ok().map(|persisted| persisted.height_min),
        height_max: verdict.as_ref().ok().map(|persisted| persisted.height_max),
        view_changes: samples.last().map_or(0, |last| {
            last.nodes
                .iter()
                .map(|node| {
                    let first = samples
                        .first()
                        .and_then(|sample| sample.nodes.iter().find(|entry| entry.id == node.id))
                        .and_then(|entry| entry.view)
                        .unwrap_or(0);
                    node.view.unwrap_or(first).saturating_sub(first)
                })
                .sum()
        }),
        catch_up_events: end_status
            .catch_up_events
            .saturating_sub(baseline.catch_up_events),
        max_backlog: samples
            .iter()
            .flat_map(|sample| sample.nodes.iter().filter_map(|node| node.mempool))
            .max()
            .unwrap_or(0),
        peak_rss_kib: samples
            .iter()
            .flat_map(|sample| sample.nodes.iter().filter_map(|node| node.rss_kib))
            .max()
            .unwrap_or(0),
        final_consistency: verdict
            .as_ref()
            .ok()
            .map(|persisted| {
                if persisted.consistent_tip {
                    "consistent"
                } else {
                    "lagging"
                }
            })
            .unwrap_or("unknown")
            .to_string(),
        data_dir: drained.active_data_dir,
    };
    Ok((summary, samples))
}

pub(super) fn archived_remote_journals(
    data_dir: &Path,
    remote_ids: &[u64],
) -> Option<Result<(), String>> {
    let log = fs::read_to_string(data_dir.join("remote-archive.log")).ok()?;
    if !log.lines().any(|line| line == "Remote archival complete") {
        return None;
    }
    let missing = remote_ids
        .iter()
        .filter(|id| {
            !data_dir.join(format!("node-{id}.json.journal")).is_file()
                && !data_dir.join(format!("node-{id}.audit.json")).is_file()
        })
        .copied()
        .collect::<Vec<_>>();
    Some(if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "remote journal retrieval incomplete for nodes {missing:?}: {log}"
        ))
    })
}

fn wait_for_remote_archive(snapshot: &ControlSnapshot) -> Result<(), String> {
    let remote_ids = snapshot
        .nodes
        .iter()
        .filter(|node| node.host_label != "Local")
        .map(|node| node.node_id)
        .collect::<Vec<_>>();
    if remote_ids.is_empty() {
        return Ok(());
    }
    let data_dir = Path::new(
        snapshot
            .active_data_dir
            .as_deref()
            .ok_or("missing node data directory")?,
    );
    let deadline = Instant::now() + Duration::from_secs(330);
    loop {
        if let Some(result) = archived_remote_journals(data_dir, &remote_ids) {
            return result;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out archiving remote journals for nodes {remote_ids:?}"
            ));
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn pin_nodes(snapshot: &ControlSnapshot, requested: &str, run_dir: &Path) -> Result<(), String> {
    let cores = requested
        .split(',')
        .map(|cpu| cpu.parse::<usize>().map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    if cores.len() != snapshot.nodes.len()
        || cores.iter().collect::<HashSet<_>>().len() != cores.len()
    {
        return Err("--pin-node-cores requires one distinct CPU per validator".into());
    }
    let mut physical = HashSet::new();
    for &cpu in &cores {
        let topology = PathBuf::from(format!("/sys/devices/system/cpu/cpu{cpu}/topology"));
        let core =
            fs::read_to_string(topology.join("core_id")).map_err(|error| error.to_string())?;
        let package = fs::read_to_string(topology.join("physical_package_id"))
            .map_err(|error| error.to_string())?;
        if !physical.insert((package.trim().to_owned(), core.trim().to_owned())) {
            return Err(
                "--pin-node-cores must select different physical cores, not SMT siblings".into(),
            );
        }
    }
    let mut nodes = snapshot.nodes.iter().collect::<Vec<_>>();
    nodes.sort_by_key(|node| node.node_id);
    let mut applied = Vec::new();
    for (node, cpu) in nodes.into_iter().zip(cores) {
        let output = Command::new("taskset")
            .args([
                "--all-tasks",
                "--pid",
                "--cpu-list",
                &cpu.to_string(),
                &node.pid.to_string(),
            ])
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "taskset failed for node {}: {}",
                node.node_id,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let thread_dir = format!("/proc/{}/task", node.pid);
        for thread in fs::read_dir(&thread_dir).map_err(|error| error.to_string())? {
            let status = fs::read_to_string(
                thread
                    .map_err(|error| error.to_string())?
                    .path()
                    .join("status"),
            )
            .map_err(|error| error.to_string())?;
            if !status
                .lines()
                .any(|line| line.trim() == format!("Cpus_allowed_list:\t{cpu}"))
            {
                return Err(format!(
                    "node {} has a thread outside CPU {cpu}",
                    node.node_id
                ));
            }
        }
        applied.push(serde_json::json!({"node_id": node.node_id, "pid": node.pid, "cpu": cpu}));
    }
    fs::write(
        run_dir.join("node-affinity.json"),
        serde_json::to_vec_pretty(&applied).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn prepare_balanced_perp_accounts(
    controller: &ControllerState,
    wallets: &[light_dex_control_plane_lib::types::WalletAccount],
) -> Result<(), String> {
    let perp_wallets = wallets
        .iter()
        .enumerate()
        .filter(|(relative, _)| matches!(balanced_market(*relative as u64), 1 | 3 | 9))
        .map(|(relative, wallet)| (balanced_market(relative as u64), wallet))
        .collect::<Vec<_>>();
    wait_for(
        controller,
        Instant::now() + Duration::from_secs(60),
        |snapshot| {
            best_status(snapshot).is_some_and(|status| {
                perp_wallets.iter().all(|(_, wallet)| {
                    status
                        .account_nonces
                        .iter()
                        .any(|entry| entry.account_id == wallet.address && entry.nonce >= 1)
                })
            })
        },
    )?;
    for (_, wallet) in &perp_wallets {
        controller.submit_transaction(
            Transaction::SetMarginMode {
                account_id: wallet.address,
                mode: light_dex::exchange::perp::MarginMode::Cross,
                nonce: 1,
            },
            TargetNode::Random,
        )?;
    }
    wait_for(
        controller,
        Instant::now() + Duration::from_secs(60),
        |snapshot| {
            best_status(snapshot).is_some_and(|status| {
                perp_wallets.iter().all(|(_, wallet)| {
                    status
                        .account_nonces
                        .iter()
                        .any(|entry| entry.account_id == wallet.address && entry.nonce >= 2)
                })
            })
        },
    )?;
    for (market_id, wallet) in &perp_wallets {
        controller.submit_transaction(
            Transaction::DepositPerpCollateral {
                account_id: wallet.address,
                market_id: *market_id,
                amount: 100_000_000 * light_dex::exchange::market::USD_ATOMS,
                nonce: 2,
            },
            TargetNode::Random,
        )?;
    }
    wait_for(
        controller,
        Instant::now() + Duration::from_secs(60),
        |snapshot| {
            best_status(snapshot).is_some_and(|status| {
                perp_wallets.iter().all(|(_, wallet)| {
                    status
                        .account_nonces
                        .iter()
                        .any(|entry| entry.account_id == wallet.address && entry.nonce >= 3)
                })
            })
        },
    )?;
    Ok(())
}
