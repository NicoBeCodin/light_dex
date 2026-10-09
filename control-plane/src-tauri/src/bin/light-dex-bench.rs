//! Headless, opt-in pilot benchmarks. Nothing is launched by `cargo test`.
use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use light_dex::{
    blockchain::{BlockchainState, Transaction},
    crypto::SessionManifest,
    exchange::order::{OrderType, Side},
    message::hex_encode,
    node::{NodeStatus, TransactionOutcome},
    participants::strategies::AgentStrategy,
};
use light_dex_control_plane_lib::{
    controller::ControllerState,
    participant_fleet_template,
    types::{AgentConfig, ConsensusAlgorithm, ControlSnapshot, LaunchConfig, PollMode, TargetNode},
};
use serde::Serialize;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Algorithm {
    Pbft,
    Hotstuff,
}
impl Algorithm {
    fn label(self) -> &'static str {
        match self {
            Self::Pbft => "pbft",
            Self::Hotstuff => "hotstuff",
        }
    }
    fn config(self) -> ConsensusAlgorithm {
        match self {
            Self::Pbft => ConsensusAlgorithm::Pbft,
            Self::Hotstuff => ConsensusAlgorithm::Hotstuff,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Workload {
    Mixed,
    Transfer,
    Resting,
}
impl Workload {
    fn label(self) -> &'static str {
        match self {
            Self::Mixed => "mixed",
            Self::Transfer => "transfer",
            Self::Resting => "resting",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct Trial {
    name: String,
    algorithm: Algorithm,
    nodes: u64,
    timeout_ms: u64,
    offered_tps: u64,
}

#[derive(Serialize)]
struct NodeSample {
    id: u64,
    state: String,
    height: Option<usize>,
    view: Option<u64>,
    mempool: Option<usize>,
    seen: Option<usize>,
    successful: Option<u64>,
    failed: Option<u64>,
    trades: Option<u64>,
    catch_up_events: Option<u64>,
    rss_kib: Option<u64>,
    cpu_ticks: Option<u64>,
    write_bytes: Option<u64>,
}

#[derive(Serialize)]
struct Sample {
    elapsed_ms: u64,
    submitted: u64,
    attempted_extra: u64,
    controller_cpu_ticks: Option<u64>,
    controller_rss_kib: Option<u64>,
    feed_age_ms: Option<u64>,
    nodes: Vec<NodeSample>,
}

#[derive(Serialize)]
struct Summary {
    name: String,
    algorithm: Algorithm,
    nodes: u64,
    timeout_ms: u64,
    offered_tps: u64,
    participants: usize,
    driver_accounts: usize,
    workload: Workload,
    in_flight_per_account: usize,
    result: String,
    reason: Option<String>,
    measured_seconds: f64,
    attempted_extra: u64,
    delivered_extra_tps: f64,
    min_responsive_during_measurement: usize,
    repair_submissions: u64,
    retry_submissions: u64,
    pending_at_end: usize,
    submitted_writes: u64,
    successful_tps: f64,
    failed_tps: f64,
    trades_per_second: f64,
    included_transfers: u64,
    included_places: u64,
    included_cancels: u64,
    active_accounts: usize,
    open_orders_at_measure_end: usize,
    latency_p50_ms: Option<u64>,
    latency_p95_ms: Option<u64>,
    latency_sample_count: usize,
    height_min: Option<usize>,
    height_max: Option<usize>,
    view_changes: u64,
    catch_up_events: u64,
    max_backlog: usize,
    peak_rss_kib: u64,
    final_consistency: String,
    data_dir: Option<String>,
}

#[derive(Serialize)]
struct TrialRecord {
    trial: Trial,
    summary: Summary,
    samples: Vec<Sample>,
    started_unix_ms: u128,
    git_head: String,
    git_diff_sha256: String,
    git_status: String,
    cpu_model: String,
    node_binary: String,
    node_binary_sha256: String,
}

fn arg(name: &str) -> Option<String> {
    let args = env::args().collect::<Vec<_>>();
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}
fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}
fn project_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}
fn command_output(project: &Path, cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .current_dir(project)
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default()
}
fn cpu_model() -> String {
    fs::read_to_string("/proc/cpuinfo")
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix("model name\t: "))
        .unwrap_or("unknown")
        .to_string()
}
fn diff_hash(project: &Path) -> String {
    let output = Command::new("git")
        .args(["diff", "--binary"])
        .current_dir(project)
        .output()
        .ok();
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    if let Some(output) = output {
        hash.update(output.stdout);
    }
    hex_encode(&hash.finalize())
}
fn file_hash(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = fs::read(path)?;
    let mut hash = Sha256::new();
    hash.update(bytes);
    Ok(hex_encode(&hash.finalize()))
}
fn proc_number(pid: u32, file: &str, key: &str) -> Option<u64> {
    fs::read_to_string(format!("/proc/{pid}/{file}"))
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix(key)?
                .trim()
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
}
fn cpu_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    Some(tail.get(11)?.parse::<u64>().ok()? + tail.get(12)?.parse::<u64>().ok()?)
}
fn sample(snapshot: &ControlSnapshot, elapsed: Duration, attempted_extra: u64) -> Sample {
    Sample {
        elapsed_ms: elapsed.as_millis().min(u64::MAX as u128) as u64,
        submitted: snapshot.metrics.submitted,
        attempted_extra,
        controller_cpu_ticks: cpu_ticks(std::process::id()),
        controller_rss_kib: proc_number(std::process::id(), "status", "VmRSS:"),
        feed_age_ms: snapshot
            .agents
            .iter()
            .filter_map(|agent| agent.feed_age_ms)
            .min(),
        nodes: snapshot
            .nodes
            .iter()
            .map(|node| NodeSample {
                id: node.node_id,
                state: node.state.clone(),
                height: node.status.as_ref().map(|s| s.chain_height),
                view: node.status.as_ref().map(|s| s.current_view),
                mempool: node.status.as_ref().map(|s| s.mempool_size),
                seen: node.status.as_ref().map(|s| s.seen_transactions),
                successful: node.status.as_ref().map(|s| s.transactions_committed),
                failed: node.status.as_ref().map(|s| s.transactions_failed),
                trades: node.status.as_ref().map(|s| s.trades_committed),
                catch_up_events: node.status.as_ref().map(|s| s.catch_up_events),
                rss_kib: proc_number(node.pid, "status", "VmRSS:"),
                cpu_ticks: cpu_ticks(node.pid),
                write_bytes: proc_number(node.pid, "io", "write_bytes:"),
            })
            .collect(),
    }
}
fn best_status(snapshot: &ControlSnapshot) -> Option<&NodeStatus> {
    snapshot
        .nodes
        .iter()
        .filter(|node| node.state == "ok")
        .filter_map(|node| node.status.as_ref())
        .max_by_key(|status| status.chain_height)
}
fn percentile(values: &[u64], fraction: f64) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut values = values.to_vec();
    values.sort_unstable();
    let index = ((values.len() - 1) as f64 * fraction).ceil() as usize;
    values.get(index).copied()
}
fn correctness(snapshot: &ControlSnapshot, expected_nodes: u64) -> Result<(), String> {
    if snapshot.metrics.responsive_nodes != expected_nodes as usize {
        return Err("one or more nodes unresponsive".into());
    }
    if snapshot.metrics.chain_consistency != "consistent" {
        return Err("replica tips disagree".into());
    }
    if snapshot.metrics.height_min != snapshot.metrics.height_max {
        return Err("replica heights disagree".into());
    }
    if !snapshot.agents.is_empty() {
        return Err("participant transactions or cancellations did not drain".into());
    }
    let data_dir = snapshot
        .active_data_dir
        .as_ref()
        .ok_or("missing node data directory")?;
    let manifest = Arc::new(SessionManifest::load(&Path::new(data_dir).join("manifest.json")).map_err(|error| error.to_string())?);
    let mut expected_tip = None;
    for id in 0..expected_nodes {
        let state = BlockchainState::load_signed(Path::new(data_dir).join(format!("node-{id}.json")), manifest.clone())
            .map_err(|error| format!("node {id} chain replay failed: {error}"))?;
        let tip = (
            state.blocks.len(),
            state.blocks.last().map(|block| block.to_digest()),
            state.exchange_state.state_digest(),
        );
        if let Some(expected) = expected_tip {
            if tip != expected {
                return Err(format!("node {id} persisted tip or state root diverged"));
            }
        }
        expected_tip = Some(tip);
    }
    Ok(())
}
fn wait_for(
    controller: &ControllerState,
    deadline: Instant,
    predicate: impl Fn(&ControlSnapshot) -> bool,
) -> Result<ControlSnapshot, String> {
    loop {
        let snapshot = controller.snapshot();
        if predicate(&snapshot) {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "cluster wait timed out: {:?}",
                snapshot.logs.iter().rev().take(6).collect::<Vec<_>>()
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

struct ClusterGuard(ControllerState);
impl Drop for ClusterGuard {
    fn drop(&mut self) {
        self.0.stop_all();
        let _ = wait_for(&self.0, Instant::now() + Duration::from_secs(8), |s| {
            !s.running && !s.launching && !s.stopping && s.metrics.alive_nodes == 0
        });
    }
}

struct PendingTx {
    transaction: Transaction,
    digest: String,
    last_sent: Instant,
    failed: bool,
    repair_attempts: u8,
}

#[derive(Debug, PartialEq)]
enum MaintenanceAction {
    Repair(Transaction),
    Retry(Transaction),
}

struct ExtraDriver {
    pending: HashMap<u64, BTreeMap<u64, PendingTx>>,
    confirmed: HashMap<u64, u64>,
    workload: Workload,
    accounts: Vec<u64>,
    wallet: std::collections::BTreeMap<u64, light_dex::exchange::address::Address>,
    window: usize,
    cursor: u64,
    attempted: u64,
    repair_submissions: u64,
    retry_submissions: u64,
    fatal_error: Option<String>,
    next_at: Instant,
}
impl ExtraDriver {
    fn new(workload: Workload, window: usize, accounts: Vec<u64>) -> Self {
        Self {
            pending: HashMap::new(),
            confirmed: HashMap::new(),
            workload,
            accounts,
            wallet: BTreeMap::new(),
            window,
            cursor: 0,
            attempted: 0,
            repair_submissions: 0,
            retry_submissions: 0,
            fatal_error: None,
            next_at: Instant::now(),
        }
    }

    fn observe_account(
        &mut self,
        account: u64,
        observed_nonce: u64,
        outcomes: &[TransactionOutcome],
    ) {
        let confirmed = self.confirmed.entry(account).or_insert(observed_nonce);
        *confirmed = (*confirmed).max(observed_nonce);
        if let Some(queue) = self.pending.get_mut(&account) {
            queue.retain(|nonce, pending| {
                if *nonce < *confirmed {
                    return false;
                }
                if outcomes
                    .iter()
                    .any(|outcome| outcome.digest == pending.digest && !outcome.success)
                {
                    pending.failed = true;
                }
                true
            });
        }
    }

    fn next_nonce(&self, account: u64) -> Option<u64> {
        let confirmed = *self.confirmed.get(&account)?;
        let queue = self.pending.get(&account);
        if queue
            .is_some_and(|queue| queue.len() >= self.window || queue.values().any(|tx| tx.failed))
        {
            return None;
        }
        match queue.and_then(BTreeMap::last_key_value) {
            Some((&nonce, _)) => nonce.checked_add(1).map(|next| next.max(confirmed)),
            None => Some(confirmed),
        }
    }

    fn pending_count(&self) -> usize {
        self.pending.values().map(BTreeMap::len).sum()
    }

    fn remember(&mut self, transaction: Transaction, now: Instant) {
        let Some(account) = self.wallet.iter().find_map(|(slot, address)| (*address == transaction.account_id()).then_some(*slot)) else { return; };
        let nonce = transaction.nonce();
        self.pending.entry(account).or_default().insert(
            nonce,
            PendingTx {
                digest: hex_encode(&transaction.to_digest()),
                transaction,
                last_sent: now,
                failed: false,
                repair_attempts: 0,
            },
        );
    }

    fn maintenance_action(&self, account: u64, now: Instant) -> Option<MaintenanceAction> {
        let confirmed = *self.confirmed.get(&account)?;
        let pending = self.pending.get(&account)?.get(&confirmed)?;
        if pending.failed {
            Some(MaintenanceAction::Repair(repair_transfer(
                self.wallet[&account], confirmed,
            )))
        } else if now.duration_since(pending.last_sent) >= Duration::from_secs(2) {
            Some(MaintenanceAction::Retry(pending.transaction.clone()))
        } else {
            None
        }
    }

    fn step(&mut self, controller: &ControllerState, snapshot: &ControlSnapshot, tps: u64) {
        let Some(status) = best_status(snapshot) else {
            return;
        };
        for entry in &status.account_nonces {
            if let Some(slot) = snapshot.wallet.iter().find(|account| account.address == entry.account_id).map(|account| account.label) {
                self.observe_account(slot, entry.nonce, &status.recent_transaction_outcomes);
            }
        }
        self.maintain(controller);
        if tps == 0 {
            return;
        }
        // The outer measurement loop polls every 20 ms; catch up scheduled
        // slots so the requested rate is not silently capped at 50 TPS.
        for _ in 0..64 {
            let now = Instant::now();
            if now < self.next_at {
                break;
            }
            self.next_at += Duration::from_secs_f64(1.0 / tps as f64);
            if self.next_at < now - Duration::from_secs(1) {
                self.next_at = now;
            }
            if !self.send_one(controller, status) {
                break;
            }
        }
    }

    fn maintain(&mut self, controller: &ControllerState) {
        let now = Instant::now();
        let mut sent = 0;
        for account in self.pending.keys().copied().collect::<Vec<_>>() {
            if sent == 8 {
                break;
            }
            let Some(action) = self.maintenance_action(account, now) else {
                continue;
            };
            if matches!(action, MaintenanceAction::Repair(_)) {
                let confirmed = self.confirmed[&account];
                if self.pending[&account][&confirmed].repair_attempts >= 3 {
                    self.fatal_error.get_or_insert_with(|| format!(
                        "account {account} nonce {confirmed} failed after three repair submissions"
                    ));
                    continue;
                }
            }
            let transaction = match &action {
                MaintenanceAction::Repair(transaction) | MaintenanceAction::Retry(transaction) => {
                    transaction.clone()
                }
            };
            if controller
                .submit_benchmark_transaction(transaction.clone(), TargetNode::Primary)
                .is_ok()
            {
                let confirmed = self.confirmed[&account];
                let pending = self
                    .pending
                    .get_mut(&account)
                    .unwrap()
                    .get_mut(&confirmed)
                    .unwrap();
                if matches!(action, MaintenanceAction::Repair(_)) {
                    pending.transaction = transaction.clone();
                    pending.digest = hex_encode(&transaction.to_digest());
                    pending.failed = false;
                    pending.repair_attempts += 1;
                    self.repair_submissions += 1;
                } else {
                    self.retry_submissions += 1;
                }
                pending.last_sent = now;
                sent += 1;
            }
        }
    }

    fn send_one(&mut self, controller: &ControllerState, status: &NodeStatus) -> bool {
        for _ in 0..self.accounts.len() {
            let account = self.accounts[self.cursor as usize % self.accounts.len()];
            self.cursor += 1;
            let Some(nonce) = self.next_nonce(account) else {
                continue;
            };
            let Some(tx) = build_transaction(self.workload, account, nonce, status, &self.wallet) else {
                continue;
            };
            if controller
                .submit_benchmark_transaction(tx.clone(), TargetNode::Primary)
                .is_ok()
            {
                self.remember(tx, Instant::now());
                self.attempted += 1;
            }
            return true;
        }
        false
    }
}

fn participant_fleet(count: usize) -> Vec<AgentConfig> {
    let mut fleet = participant_fleet_template();
    fleet.truncate(count);
    for index in fleet.len()..count {
        let account_id = if index < 30 { 70 + index as u64 } else { (index - 30) as u64 };
        let order_type = if index % 3 == 0 { OrderType::LO } else { OrderType::IOC };
        fleet.push(AgentConfig {
            agent_id: index as u64 + 1,
            account_id,
            market_id: 0,
            target: TargetNode::Random,
            strategy: AgentStrategy::RandomOrder {
                order_type,
                tps: 1.0,
                passive_offset_ticks: 10 + index as u64 % 15,
                max_resting_orders: 2,
                resting_ttl_ms: 5000,
            },
        });
    }
    fleet
}

fn repair_transfer(account: light_dex::exchange::address::Address, nonce: u64) -> Transaction {
    Transaction::Transfer {
        from: account,
        to: account,
        asset: 0,
        amount: 1,
        nonce,
    }
}

fn build_transaction(
    workload: Workload,
    account: u64,
    nonce: u64,
    status: &NodeStatus,
    wallet: &std::collections::BTreeMap<u64, light_dex::exchange::address::Address>,
) -> Option<Transaction> {
    let address = *wallet.get(&account)?;
    let next_address = *wallet.iter().find(|(label, _)| **label != account)?.1;
    if workload == Workload::Transfer {
        return Some(Transaction::Transfer {
            from: address,
            to: next_address,
            asset: 0,
            amount: 1,
            nonce,
        });
    }
    if workload == Workload::Resting {
        return Some(Transaction::PlaceOrder {
            account_id: address,
            market_id: 0,
            order_type: OrderType::LO,
            side: Side::Sell,
            price: 10_000_000 + nonce,
            quantity: 1,
            nonce,
        });
    }
    let book = status.order_books.iter().find(|book| book.market_id == 0);
    let own_order = status
        .open_orders
        .iter()
        .find(|order| order.account_id == address);
    Some(match (nonce + account) % 6 {
        0 => Transaction::Transfer {
            from: address,
            to: next_address,
            asset: 0,
            amount: 1,
            nonce,
        },
        1 => Transaction::PlaceOrder {
            account_id: address,
            market_id: 0,
            order_type: OrderType::LO,
            side: Side::Sell,
            price: book
                .and_then(|b| b.asks.first())
                .map_or(100_010, |level| level.price.saturating_add(2)),
            quantity: 1,
            nonce,
        },
        2 => Transaction::PlaceOrder {
            account_id: address,
            market_id: 0,
            order_type: OrderType::LO,
            side: Side::Buy,
            price: book
                .and_then(|b| b.bids.first())
                .map_or(99_990, |level| level.price.saturating_sub(2).max(1)),
            quantity: 1,
            nonce,
        },
        3 => match own_order {
            Some(order) => Transaction::CancelOrder {
                account_id: address,
                market_id: 0,
                order_id: order.order_id,
                nonce,
            },
            None => Transaction::Transfer {
                from: address,
                to: next_address,
                asset: 0,
                amount: 1,
                nonce,
            },
        },
        4 => Transaction::PlaceOrder {
            account_id: address,
            market_id: 0,
            order_type: OrderType::IOC,
            side: Side::Buy,
            price: book?.asks.first()?.price,
            quantity: 1,
            nonce,
        },
        _ => Transaction::PlaceOrder {
            account_id: address,
            market_id: 0,
            order_type: OrderType::FOK,
            side: Side::Sell,
            price: book?.bids.first()?.price,
            quantity: 1,
            nonce,
        },
    })
}

fn trials(smoke: bool) -> Vec<Trial> {
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

fn failed_summary(trial: &Trial, reason: String, workload: Workload, window: usize, participants: usize) -> Summary {
    Summary {
        name: trial.name.clone(),
        algorithm: trial.algorithm,
        nodes: trial.nodes,
        timeout_ms: trial.timeout_ms,
        offered_tps: trial.offered_tps,
        participants,
        driver_accounts: (100 - participants).min(70),
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
        failed_tps: 0.0,
        trades_per_second: 0.0,
        included_transfers: 0,
        included_places: 0,
        included_cancels: 0,
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

fn run_trial(
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
        binary_path: "target/release/light_dex".into(),
        data_dir: run_dir.join("chains").display().to_string(),
        recover_existing: false,
        node_count: trial.nodes,
        block_time_ms: 500,
        timeout_ms: trial.timeout_ms,
        poll_ms: 500,
        poll_mode: PollMode::All,
        build_before_launch: false,
        consensus: trial.algorithm.config(),
        claim_limit: 1_000,
        claim_usd: 1_000_000,
        claim_btc: 1_000,
    })?;
    wait_for(&controller, Instant::now() + Duration::from_secs(40), |s| {
        s.running
            && s.metrics.responsive_nodes == trial.nodes as usize
            && s.metrics.height_min == Some(1)
    })?;
    let mut fleet = participant_fleet(participants);
    let driver_count = (100 - participants).min(70);
    let wallets = controller.claim_new_accounts((participants + driver_count) as u64)?;
    for (agent, wallet) in fleet.iter_mut().zip(&wallets) {
        agent.account_id = wallet.label;
    }
    let driver_accounts = wallets.iter().skip(participants).map(|wallet| wallet.label).collect::<Vec<_>>();
    controller.start_agents(fleet)?;
    thread::sleep(warmup);
    let warm = controller.snapshot();
    let fresh = warm
        .agents
        .iter()
        .any(|agent| agent.feed_age_ms.is_some_and(|age| age < 3000));
    if !fresh || !warm.agents.iter().any(|agent| agent.own_open_orders > 0) {
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
    driver.wallet = warm.wallet.iter().map(|account| (account.label, account.address)).collect();
    let start = Instant::now();
    let mut samples = Vec::new();
    let mut last_sample = Instant::now() - Duration::from_secs(1);
    while start.elapsed() < measure {
        let snapshot = controller.snapshot();
        driver.step(&controller, &snapshot, trial.offered_tps);
        if last_sample.elapsed() >= Duration::from_millis(500) {
            samples.push(sample(&snapshot, start.elapsed(), driver.attempted));
            last_sample = Instant::now();
        }
        thread::sleep(Duration::from_millis(20));
    }
    let end = controller.snapshot();
    let measured_seconds = start.elapsed().as_secs_f64().max(0.001);
    samples.push(sample(&end, start.elapsed(), driver.attempted));
    let pending_at_end = driver.pending_count();
    let repair_submissions = driver.repair_submissions;
    let retry_submissions = driver.retry_submissions;
    let end_status = best_status(&end).ok_or_else(|| format!(
        "no responsive node at measurement end; states={:?}; recent_logs={:?}",
        end.nodes.iter().map(|node| (node.node_id, node.state.as_str())).collect::<Vec<_>>(),
        end.logs.iter().rev().take(8).collect::<Vec<_>>(),
    ))?;
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
    let verdict = correctness(&drained, trial.nodes);
    let (mut included_transfers, mut included_places, mut included_cancels) = (0, 0, 0);
    let mut active_accounts = std::collections::HashSet::new();
    if let Some(data_dir) = &drained.active_data_dir {
        let manifest = Arc::new(SessionManifest::load(&Path::new(data_dir).join("manifest.json")).map_err(|error| error.to_string())?);
        let chain = BlockchainState::load_signed(Path::new(data_dir).join("node-0.json"), manifest)?;
        for block in chain.blocks.iter().skip(baseline.chain_height).take(end_status.chain_height.saturating_sub(baseline.chain_height)) {
            for tx in &block.transactions {
                active_accounts.insert(tx.account_id());
                match tx {
                    Transaction::ClaimAirdrop { .. } => {},
                    Transaction::Transfer { .. } => included_transfers += 1,
                    Transaction::PlaceOrder { .. } => included_places += 1,
                    Transaction::CancelOrder { .. } => included_cancels += 1,
                }
            }
        }
    }
    let feed_stale = samples
        .iter()
        .any(|s| s.feed_age_ms.is_none_or(|age| age > 3000));
    let min_responsive = samples.iter().map(|sample| sample.nodes.iter()
        .filter(|node| node.state == "ok").count()).min().unwrap_or(0);
    let delivered_extra_tps = driver.attempted as f64 / measured_seconds;
    let reason = verdict
        .err()
        .or_else(|| driver.fatal_error.clone())
        .or_else(|| {
            (driver.pending_count() != 0).then(|| {
                format!(
                    "{} benchmark transactions did not drain",
                    driver.pending_count()
                )
            })
        })
        .or_else(|| feed_stale.then(|| "Binance feed stale during measurement".into()))
        .or_else(|| (trial.name.starts_with("capacity_") && min_responsive < trial.nodes as usize)
            .then(|| format!("only {min_responsive}/{} nodes responsive in at least one measurement sample", trial.nodes)))
        .or_else(|| (trial.name.starts_with("capacity_") && trial.offered_tps > 0
            && delivered_extra_tps < trial.offered_tps as f64 * 0.95)
            .then(|| format!("driver delivered {delivered_extra_tps:.1} of {} requested extra TPS", trial.offered_tps)));
    let seconds = measured_seconds;
    let summary = Summary {
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
        failed_tps: failed as f64 / seconds,
        trades_per_second: trades as f64 / seconds,
        included_transfers,
        included_places,
        included_cancels,
        active_accounts: active_accounts.len(),
        open_orders_at_measure_end: end_status.open_orders.len(),
        latency_p50_ms: percentile(&latencies, 0.50),
        latency_p95_ms: percentile(&latencies, 0.95),
        latency_sample_count: latencies.len(),
        height_min: drained.metrics.height_min,
        height_max: drained.metrics.height_max,
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
        final_consistency: drained.metrics.chain_consistency,
        data_dir: drained.active_data_dir,
    };
    Ok((summary, samples))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = env::args().nth(1).unwrap_or_default();
    if ![
        "pilot",
        "smoke",
        "stress",
        "stress-high",
        "timeout-probe",
        "profile",
        "capacity",
    ]
    .contains(&mode.as_str())
    {
        return Err("usage: light-dex-bench pilot|smoke|stress|stress-high|timeout-probe|profile|capacity [--algorithm pbft|hotstuff] [--nodes 4..25] [--participants 1..99] [--offered-tps N] [--workload mixed|transfer|resting] [--in-flight-per-account 1..32] [--out DIR] [--measure-sec N] [--warmup-sec N]".into());
    }
    let workload = match arg("--workload").as_deref().unwrap_or("mixed") {
        "mixed" => Workload::Mixed,
        "transfer" => Workload::Transfer,
        "resting" => Workload::Resting,
        _ => return Err("--workload must be mixed, transfer or resting".into()),
    };
    let window = arg("--in-flight-per-account")
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(1);
    if !(1..=32).contains(&window) {
        return Err("--in-flight-per-account must be 1..32".into());
    }
    if workload == Workload::Mixed && window != 1 {
        return Err("mixed orders require a window of 1; pipeline the transfer workload for a clean consensus stress test".into());
    }
    let participants = arg("--participants")
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(13);
    if !(1..=99).contains(&participants) {
        return Err("--participants must be 1..99 (at least one driver account remains)".into());
    }
    let project = project_dir();
    let node_binary = project.join("target/release/light_dex");
    if !node_binary.is_file() {
        return Err("build the release node first: cargo build --release --bin light_dex".into());
    }
    let run_root = arg("--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| project.join("benchmark_results/runs"));
    let output = run_root.join(format!("pilot-{}", unix_ms()));
    fs::create_dir_all(&output)?;
    let smoke = mode == "smoke";
    let warmup = Duration::from_secs(
        arg("--warmup-sec")
            .and_then(|s| s.parse().ok())
            .unwrap_or(if smoke { 12 } else { 12 }),
    );
    let measure = Duration::from_secs(
        arg("--measure-sec")
            .and_then(|s| s.parse().ok())
            .unwrap_or(if smoke { 10 } else { 30 }),
    );
    let git_head = command_output(&project, "git", &["rev-parse", "HEAD"]);
    let git_diff_sha256 = diff_hash(&project);
    let git_status = command_output(&project, "git", &["status", "--porcelain"]);
    let node_binary_sha256 = file_hash(&node_binary)?;
    let cpu_model = cpu_model();
    let began = Instant::now();
    let mut summaries = Vec::<Summary>::new();
    let selected = match mode.as_str() {
        "capacity" => {
            let algorithm = match arg("--algorithm").as_deref().unwrap_or("hotstuff") {
                "pbft" => Algorithm::Pbft,
                "hotstuff" => Algorithm::Hotstuff,
                _ => return Err("--algorithm must be pbft or hotstuff".into()),
            };
            let offered_tps = arg("--offered-tps").ok_or("capacity mode requires --offered-tps")?.parse::<u64>()?;
            let nodes = arg("--nodes").map(|value| value.parse::<u64>()).transpose()?.unwrap_or(7);
            if !(4..=25).contains(&nodes) {
                return Err("--nodes must be 4..25".into());
            }
            vec![Trial {
                name: format!("capacity_{}_{}n_{}p_{}_{}", algorithm.label(), nodes, participants, workload.label(), offered_tps),
                algorithm,
                nodes,
                timeout_ms: 1500,
                offered_tps,
            }]
        }
        "stress" => trials(false)
            .into_iter()
            .filter(|trial| trial.name.starts_with("stress_"))
            .collect(),
        "stress-high" => trials(false)
            .into_iter()
            .filter(|trial| trial.name.starts_with("stress_") && trial.offered_tps >= 80)
            .collect(),
        "timeout-probe" => [50, 100, 150]
            .into_iter()
            .flat_map(|timeout_ms| {
                [Algorithm::Pbft, Algorithm::Hotstuff]
                    .into_iter()
                    .map(move |algorithm| Trial {
                        name: format!("probe_{}_{}", algorithm.label(), timeout_ms),
                        algorithm,
                        nodes: 7,
                        timeout_ms,
                        offered_tps: 0,
                    })
            })
            .collect(),
        "profile" => vec![Trial {
            name: "profile_hotstuff_7_80".into(),
            algorithm: Algorithm::Hotstuff,
            nodes: 7,
            timeout_ms: 1500,
            offered_tps: 80,
        }],
        _ => trials(smoke),
    };
    for trial in selected {
        if !smoke && began.elapsed() > Duration::from_secs(28 * 60) {
            break;
        }
        println!("Starting {}", trial.name);
        let trial_dir = output.join(&trial.name);
        fs::create_dir_all(&trial_dir)?;
        let started_unix_ms = unix_ms();
        match run_trial(
            &trial, &project, &trial_dir, warmup, measure, workload, window, participants,
        ) {
            Ok((summary, samples)) => {
                println!(
                    "{}: {} {:.2} successful TPS",
                    trial.name, summary.result, summary.successful_tps
                );
                let record = TrialRecord {
                    trial,
                    summary,
                    samples,
                    started_unix_ms,
                    git_head: git_head.clone(),
                    git_diff_sha256: git_diff_sha256.clone(),
                    git_status: git_status.clone(),
                    cpu_model: cpu_model.clone(),
                    node_binary: node_binary.display().to_string(),
                    node_binary_sha256: node_binary_sha256.clone(),
                };
                fs::write(
                    trial_dir.join("run.json"),
                    serde_json::to_vec_pretty(&record)?,
                )?;
                summaries.push(record.summary);
            }
            Err(error) => {
                eprintln!("{} failed: {error}", trial.name);
                fs::write(trial_dir.join("error.txt"), error.as_bytes())?;
                summaries.push(failed_summary(&trial, error.clone(), workload, window, participants));
                if error.contains("feed_fresh=false") {
                    break;
                }
            }
        }
        fs::write(
            output.join("summary.json"),
            serde_json::to_vec_pretty(&summaries)?,
        )?;
    }
    fs::write(
        output.join("summary.json"),
        serde_json::to_vec_pretty(&summaries)?,
    )?;
    println!("Results: {}", output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn transfer(account: u64, nonce: u64) -> Transaction {
        Transaction::Transfer {
            from: account.into(),
            to: 1.into(),
            asset: 0,
            amount: 1,
            nonce,
        }
    }
    fn failure(transaction: &Transaction) -> TransactionOutcome {
        TransactionOutcome {
            digest: hex_encode(&transaction.to_digest()),
            account_id: transaction.account_id(),
            nonce: transaction.nonce(),
            success: false,
            error: Some("test failure".into()),
        }
    }
    fn test_driver(window: usize) -> ExtraDriver {
        let mut driver = ExtraDriver::new(Workload::Transfer, window, vec![0]);
        driver.wallet.insert(0, 0.into());
        driver
    }
    #[test]
    fn percentile_nearest_rank() {
        assert_eq!(percentile(&[1, 2, 3, 4, 5], 0.95), Some(5));
        assert_eq!(percentile(&[], 0.95), None);
    }
    #[test]
    fn planned_pilot_has_all_axes() {
        let specs = trials(false);
        assert_eq!(specs.len(), 26);
        assert!(specs.iter().any(|s| s.nodes == 13));
        assert!(specs.iter().any(|s| s.timeout_ms == 200));
        assert!(specs.iter().any(|s| s.offered_tps == 160));
    }
    #[test]
    fn pipeline_allocates_contiguous_nonces_up_to_window() {
        let mut driver = test_driver(3);
        let now = Instant::now();
        driver.observe_account(0, 7, &[]);
        for nonce in 7..10 {
            assert_eq!(driver.next_nonce(0), Some(nonce));
            driver.remember(transfer(0, nonce), now);
        }
        assert_eq!(driver.next_nonce(0), None);
        driver.observe_account(0, 9, &[]);
        assert_eq!(driver.pending_count(), 1);
        assert_eq!(driver.next_nonce(0), Some(10));
        // A lagging replica cannot make the driver's confirmed nonce regress.
        driver.observe_account(0, 7, &[]);
        assert_eq!(driver.next_nonce(0), Some(10));
    }
    #[test]
    fn failed_head_is_repaired_at_same_nonce_before_new_work() {
        let mut driver = test_driver(3);
        let now = Instant::now();
        driver.observe_account(0, 5, &[]);
        let head = transfer(0, 5);
        driver.remember(head.clone(), now);
        driver.remember(transfer(0, 6), now);
        driver.observe_account(0, 5, &[failure(&head)]);
        assert_eq!(driver.next_nonce(0), None);
        assert_eq!(
            driver.maintenance_action(0, now),
            Some(MaintenanceAction::Repair(repair_transfer(0.into(), 5)))
        );
        // After nonce 5 is consumed, a failed tail becomes the next head.
        driver.observe_account(0, 6, &[failure(&transfer(0, 6))]);
        assert_eq!(
            driver.maintenance_action(0, now),
            Some(MaintenanceAction::Repair(repair_transfer(0.into(), 6)))
        );
    }
    #[test]
    fn timed_out_head_retries_identical_transaction_and_nonce() {
        let mut driver = test_driver(2);
        let now = Instant::now();
        let transaction = transfer(0, 3);
        driver.observe_account(0, 3, &[]);
        driver.remember(transaction.clone(), now);
        assert_eq!(
            driver.maintenance_action(0, now + Duration::from_secs(1)),
            None
        );
        assert_eq!(
            driver.maintenance_action(0, now + Duration::from_secs(2)),
            Some(MaintenanceAction::Retry(transaction))
        );
    }
}
