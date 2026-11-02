use crate::binance_depth::{PartialDepth, STREAM_URL, desired_book_for_market, mid_price};
use crate::types::{
    AgentConfig, AgentSnapshot, ClaimProgress, ControlSnapshot, LaunchConfig, LoadConfig,
    LoadPreset, LoadSnapshot, MarketMakerConfig, MarketMakerSnapshot, MetricsSnapshot,
    NodeRuntimeSnapshot, TargetNode, WalletAccount,
};
use ed25519_dalek::SigningKey;
use light_dex::{
    blockchain::Transaction,
    crypto::{
        FaucetTerms, SessionManifest, create_session_with_mode, new_key, read_secret,
        sign_transaction, write_secret,
    },
    exchange::{address::Address, market_config::DecimalStep},
    message::Message,
    message::{hex_encode, sign_line, verify_line},
    node::{ControlMessage, NodeId, NodeStatus},
    participants::market_maker::{DesiredBook, QuoteAction, plan_actions},
    participants::strategies::AgentStrategy,
};
use light_dex_market_data_hub::HubHandle;
use rand::{Rng, SeedableRng, rngs::StdRng};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    io::{self, BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

mod agents;
mod cluster;
mod feed;
mod keeper;
mod load;
mod market_maker;
mod monitoring;
mod remote;
mod transport;
mod wallet;

use agents::*;
use cluster::*;
use feed::*;
use keeper::*;
use load::*;
use monitoring::*;
use remote::*;
use transport::*;

type Shared = Arc<Mutex<ControllerInner>>;

fn market_tick(status: &NodeStatus, market_id: u64) -> Option<DecimalStep> {
    status
        .markets
        .iter()
        .find(|market| market.id == market_id)
        .map(|market| market.config.price)
}

fn slot_address(state: &ControllerInner, slot: u64) -> Option<Address> {
    state.wallet.get(&slot).copied()
}

fn address_slot(state: &ControllerInner, address: Address) -> Option<u64> {
    if let Some(slot) = state.wallet_slots.get(&address) {
        return Some(*slot);
    }
    state
        .wallet
        .iter()
        .find_map(|(slot, candidate)| (*candidate == address).then_some(*slot))
}

struct RuntimeNode {
    child: Child,
    remote: Option<RemoteRuntime>,
    address: Option<SocketAddr>,
    read_api_address: Option<SocketAddr>,
    status: Option<NodeStatus>,
    state: String,
    last_sample: Option<(Instant, u64, u64)>,
    last_poll_started: Option<Instant>,
    last_status_at_ms: Option<u64>,
    status_rtt_ms: Option<u64>,
    status_failures: u8,
    last_commit_seen: Instant,
    tx_rate: f64,
    block_rate: f64,
    paused_until: Option<Instant>,
}

struct ControllerInner {
    nodes: BTreeMap<NodeId, RuntimeNode>,
    remote_hosts: BTreeMap<String, RemoteHostRuntime>,
    market_data_hub: Option<HubHandle>,
    running: bool,
    launching: bool,
    stopping: bool,
    submitted: u64,
    submit_times: VecDeque<Instant>,
    logs: VecDeque<String>,
    run_id: u64,
    load: Option<LoadRuntime>,
    next_load_id: u64,
    auth_key: [u8; 32],
    active_data_dir: Option<String>,
    signing_manifest: Option<Arc<SessionManifest>>,
    account_keys: BTreeMap<u64, SigningKey>,
    wallet: BTreeMap<u64, Address>,
    wallet_slots: HashMap<Address, u64>,
    watched_order_accounts: Vec<Address>,
    leader_hint: Option<(u64, NodeId)>,
    pending_airdrops: HashMap<Address, (u64, Option<String>)>,
    pending_claims: Vec<WalletAccount>,
    claim_progress: Option<ClaimProgress>,
    market_maker: Option<MarketMakerRuntime>,
    next_market_maker_id: u64,
    agents: BTreeMap<u64, AgentRuntime>,
    next_group_id: u64,
    feed_depth: Option<PartialDepth>,
    feed_at: Option<Instant>,
    feed_history: VecDeque<(Instant, f64)>,
    feed_started: bool,
    local_mid_history: VecDeque<f64>,
    last_book_height: Option<usize>,
}

struct AgentRuntime {
    config: AgentConfig,
    rng: StdRng,
    desired: DesiredBook,
    active: bool,
    stopping: bool,
    pending: VecDeque<PendingAgentTransaction>,
    /// Digests already queued for, or undergoing, admission. A slow remote
    /// reply must not create duplicate retry jobs for the same transaction.
    dispatching: HashSet<String>,
    last_step: Instant,
    last_error: Option<String>,
    resting_since: HashMap<u64, Instant>,
    group_id: Option<u64>,
}

fn new_agent(config: AgentConfig, group_id: Option<u64>) -> AgentRuntime {
    AgentRuntime {
        config,
        rng: StdRng::seed_from_u64(rand::random()),
        desired: DesiredBook::default(),
        active: true,
        stopping: false,
        pending: VecDeque::new(),
        dispatching: HashSet::new(),
        last_step: Instant::now() - Duration::from_secs(1),
        last_error: None,
        resting_since: HashMap::new(),
        group_id,
    }
}

struct PendingAgentTransaction {
    transaction: Transaction,
    digest: String,
    action: Option<QuoteAction>,
    last_sent: Instant,
}

const MAX_AGENT_IN_FLIGHT: usize = 8;

fn next_agent_nonce(confirmed: u64, pending: &VecDeque<PendingAgentTransaction>) -> Option<u64> {
    let mut next = confirmed;
    for transaction in pending {
        let nonce = transaction.transaction.nonce();
        if nonce < next {
            continue;
        }
        if nonce > next {
            break;
        }
        next = next.checked_add(1)?;
    }
    Some(next)
}

struct MarketMakerRuntime {
    id: u64,
    config: MarketMakerConfig,
    active: bool,
    stopping: bool,
    latest_depth: Option<PartialDepth>,
    feed_at: Option<Instant>,
    desired: DesiredBook,
    last_applied: Option<Instant>,
    pending: VecDeque<PendingQuote>,
    last_error: Option<String>,
}

struct PendingQuote {
    action: QuoteAction,
    transaction: Transaction,
    digest: String,
    last_sent: Instant,
}

struct LoadRuntime {
    id: u64,
    config: LoadConfig,
    cursor: u64,
    pending: HashMap<u64, PendingLoadTransaction>,
    failed: HashMap<u64, u64>,
}

struct PendingLoadTransaction {
    transaction: Transaction,
    digest: String,
    last_sent: Instant,
}

impl Default for ControllerInner {
    fn default() -> Self {
        Self {
            nodes: BTreeMap::new(),
            remote_hosts: BTreeMap::new(),
            market_data_hub: None,
            running: false,
            launching: false,
            stopping: false,
            submitted: 0,
            submit_times: VecDeque::new(),
            logs: VecDeque::new(),
            run_id: 0,
            load: None,
            next_load_id: 0,
            auth_key: [0; 32],
            active_data_dir: None,
            signing_manifest: None,
            account_keys: BTreeMap::new(),
            wallet: BTreeMap::new(),
            wallet_slots: HashMap::new(),
            watched_order_accounts: Vec::new(),
            leader_hint: None,
            pending_airdrops: HashMap::new(),
            pending_claims: Vec::new(),
            claim_progress: None,
            market_maker: None,
            next_market_maker_id: 0,
            agents: BTreeMap::new(),
            next_group_id: 0,
            feed_depth: None,
            feed_at: None,
            feed_history: VecDeque::new(),
            feed_started: false,
            local_mid_history: VecDeque::new(),
            last_book_height: None,
        }
    }
}

impl ControllerInner {
    fn log(&mut self, message: impl Into<String>) {
        const MAX_LOGS: usize = 3000;
        let message = message.into();
        if message.starts_with("Node ") && message.contains(" exited:") {
            if let Some(data_dir) = &self.active_data_dir {
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(PathBuf::from(data_dir).join("validator-exits.log"))
                {
                    let _ = writeln!(file, "{message}");
                }
            }
        }
        self.logs.push_back(message);
        while self.logs.len() > MAX_LOGS {
            self.logs.pop_front();
        }
    }
}

#[derive(Clone, Default)]
pub struct ControllerState {
    inner: Shared,
}

fn nonce_in_window(confirmed: u64, requested: u64, future_window: u64) -> bool {
    requested >= confirmed && requested <= confirmed.saturating_add(future_window)
}

fn benchmark_nonce_allowed(
    account: &light_dex::node::AccountNonce,
    requested: u64,
    future_window: u64,
) -> bool {
    nonce_in_window(account.nonce, requested, future_window)
        || (future_window > 0 && requested < account.nonce && !account.consumed(requested))
}

impl ControllerState {
    pub fn remote_profiles(&self) -> Result<Vec<crate::types::RemoteChoice>, String> {
        let project = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        available_profiles(&project).map_err(|error| error.to_string())
    }
    pub fn read_api_target(&self, node_id: NodeId) -> Option<(SocketAddr, [u8; 32])> {
        let state = self.inner.lock().ok()?;
        let node = state.nodes.get(&node_id)?;
        (node.state != "dead").then_some((node.read_api_address?, state.auth_key))
    }

    pub fn read_node_projection(
        &self,
        node_id: NodeId,
        path: &str,
    ) -> Result<serde_json::Value, String> {
        let (address, key) = self
            .read_api_target(node_id)
            .ok_or("node read API unavailable")?;
        request_read_api(address, path, &key).map_err(|error| error.to_string())
    }
    pub fn launch(&self, config: LaunchConfig) -> Result<(), String> {
        if config.node_count == 0 {
            return Err("node count must be at least one".into());
        }
        if config.auto_view_change_blocks == Some(0) {
            return Err("automatic view-change interval must be at least one block".into());
        }
        if config.tx_batch_window_ms > 1_000 {
            return Err("transaction batch window must be 0..1000 ms".into());
        }
        let run_id = {
            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            if state.running || state.launching || state.stopping {
                return Err("simulation already running".into());
            }
            state.run_id += 1;
            state.leader_hint = None;
            state.auth_key = rand::random();
            state.launching = true;
            state.submitted = 0;
            state.submit_times.clear();
            state.nodes.clear();
            state.remote_hosts.clear();
            state.market_data_hub = None;
            state.load = None;
            state.market_maker = None;
            state.agents.clear();
            state.feed_depth = None;
            state.feed_at = None;
            state.feed_history.clear();
            state.feed_started = false;
            state.local_mid_history.clear();
            state.last_book_height = None;
            state.logs.clear();
            state.active_data_dir = None;
            state.signing_manifest = None;
            state.account_keys.clear();
            state.wallet.clear();
            state.wallet_slots.clear();
            state.watched_order_accounts.clear();
            state.pending_airdrops.clear();
            state.pending_claims.clear();
            state.claim_progress = None;
            state.log("Starting simulation...");
            state.run_id
        };
        let shared = self.inner.clone();
        thread::spawn(move || {
            if let Err(error) = launch_cluster(shared.clone(), config, run_id) {
                if shared.lock().unwrap().run_id == run_id {
                    log(&shared, format!("Launch failed: {error}"));
                    stop_cluster(shared, true);
                }
            }
        });
        Ok(())
    }

    pub fn stop_all(&self) {
        let shared = self.inner.clone();
        {
            let mut state = shared.lock().unwrap();
            if state.stopping || (!state.running && !state.launching) {
                return;
            }
            state.stopping = true;
            state.running = false;
            state.launching = false;
            state.run_id += 1;
            state.load = None;
            state.market_maker = None;
            state.agents.clear();
            state.log("Stopping simulation...");
        }
        thread::spawn(move || stop_cluster(shared, false));
    }

    pub fn submit_transaction(
        &self,
        transaction: Transaction,
        target: TargetNode,
    ) -> Result<(), String> {
        if matches!(
            &transaction,
            Transaction::PlaceOrder { .. }
                | Transaction::PlacePerpOrder { .. }
                | Transaction::CancelOrder { .. }
                | Transaction::UpdateQuotes { .. }
        ) {
            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            let owner = transaction.account_id();
            if state.wallet_slots.contains_key(&owner)
                && !state.watched_order_accounts.contains(&owner)
            {
                state.watched_order_accounts.push(owner);
            }
        }
        self.submit_transaction_with_window(transaction, target, 0)
    }

    /// Used only by the headless benchmark driver. The UI command keeps the
    /// confirmed-nonce-only rule; this path permits a bounded pipeline.
    pub fn submit_benchmark_transaction(
        &self,
        transaction: Transaction,
        target: TargetNode,
    ) -> Result<(), String> {
        self.submit_transaction_with_window(transaction, target, 32)
    }

    fn submit_transaction_with_window(
        &self,
        transaction: Transaction,
        target: TargetNode,
        future_nonce_window: u64,
    ) -> Result<(), String> {
        let random_target = matches!(target, TargetNode::Random);
        let (target, auth_key, transaction) = {
            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            let account_id = transaction.account_id();
            let account_slot = address_slot(&state, account_id)
                .ok_or("sender address is not in the controller wallet")?;
            if state.pending_airdrops.contains_key(&account_id) {
                return Err(format!("account {account_id} is funding another address"));
            }
            if state.market_maker.as_ref().is_some_and(|mm| {
                (mm.active || mm.stopping) && mm.config.account_id == account_slot
            }) {
                return Err(format!(
                    "account {account_id} is controlled by the market maker"
                ));
            }
            if state
                .load
                .as_ref()
                .is_some_and(|load| load.pending.contains_key(&account_slot))
            {
                return Err(format!(
                    "account {account_id} already has a load transaction awaiting confirmation"
                ));
            }
            let confirmed = state
                .nodes
                .values()
                .filter(|node| node.state == "ok")
                .filter_map(|node| {
                    node.status.as_ref().and_then(|status| {
                        status
                            .account_nonces
                            .iter()
                            .find(|account| account.account_id == account_id)
                            .map(|account| (status.chain_height, account.clone()))
                    })
                })
                .max_by_key(|(height, _)| *height)
                .map(|(_, account)| account)
                .ok_or_else(|| format!("no confirmed nonce available for account {account_id}"))?;
            let confirmed_nonce = confirmed.nonce;
            if let Some(agent_id) = state
                .agents
                .iter()
                .find_map(|(&id, agent)| (agent.config.account_id == account_slot).then_some(id))
            {
                if future_nonce_window != 0 {
                    return Err("benchmark driver cannot use an agent wallet".into());
                }
                let agent = state.agents.get_mut(&agent_id).expect("found agent");
                if agent.stopping || agent.pending.len() >= MAX_AGENT_IN_FLIGHT {
                    return Err("participant is stopping or has a full transaction queue".into());
                }
                let nonce = next_agent_nonce(confirmed_nonce, &agent.pending)
                    .ok_or("participant nonce queue is not contiguous")?;
                let transaction = transaction.with_nonce(nonce);
                let target_address = resolve_target(&state, &target);
                if target_address.is_none() {
                    return Err("no available target node".into());
                }
                let agent = state.agents.get_mut(&agent_id).expect("found agent");
                agent.pending.push_back(PendingAgentTransaction {
                    digest: hex_encode(&transaction.to_digest()),
                    transaction: transaction.clone(),
                    action: None,
                    last_sent: Instant::now(),
                });
                (target_address, state.auth_key, transaction)
            } else {
                if !benchmark_nonce_allowed(&confirmed, transaction.nonce(), future_nonce_window) {
                    return Err(format!(
                        "account {account_id} permits nonce {confirmed_nonce}..={}; transaction uses {}",
                        confirmed_nonce.saturating_add(future_nonce_window),
                        transaction.nonce()
                    ));
                }
                (resolve_target(&state, &target), state.auth_key, transaction)
            }
        };
        let target = target.ok_or_else(|| "no available target node".to_string())?;
        if future_nonce_window != 0 {
            // The benchmark needs actual delivery feedback. Spawning one
            // controller thread (and formatting a full transaction log) for
            // every offered write both distorts TPS and hides failed sends.
            send_signed_transaction_with_failover(
                &self.inner,
                target,
                random_target,
                transaction,
                &auth_key,
            )
            .map_err(|error| error.to_string())?;
            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            state.submitted += 1;
            state.submit_times.push_back(Instant::now());
            return Ok(());
        }
        let shared = self.inner.clone();
        thread::spawn(move || {
            match send_signed_transaction_with_failover(
                &shared,
                target,
                random_target,
                transaction.clone(),
                &auth_key,
            ) {
                Ok(submitted_to) => {
                    let mut state = shared.lock().unwrap();
                    state.submitted += 1;
                    state.submit_times.push_back(Instant::now());
                    state.log(format!("Submitted {transaction:?} to node {submitted_to}"));
                }
                Err(error) => log(
                    &shared,
                    format!(
                        "Transaction submission to node {} failed: {error}",
                        target.0
                    ),
                ),
            }
        });
        Ok(())
    }

    pub fn pause_node(&self, target: TargetNode, millis: u64) -> Result<(), String> {
        let (target, auth_key) = {
            let state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            (resolve_target(&state, &target), state.auth_key)
        };
        let target = target.ok_or_else(|| "no available target node".to_string())?;
        let shared = self.inner.clone();
        thread::spawn(move || {
            match send_message(
                target.1,
                &Message::Control(ControlMessage::Pause { millis }),
                Duration::from_millis(500),
                &auth_key,
            ) {
                Ok(()) => {
                    let mut state = shared.lock().unwrap();
                    if let Some(node) = state.nodes.get_mut(&target.0) {
                        node.paused_until = Some(Instant::now() + Duration::from_millis(millis));
                    }
                    state.log(format!("Paused node {} for {millis} ms", target.0));
                }
                Err(error) => log(&shared, format!("Pause node {} failed: {error}", target.0)),
            }
        });
        Ok(())
    }

    pub fn kill_node(&self, target: TargetNode) -> Result<(), String> {
        let (node_id, remote) = {
            let state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            let node_id = resolve_target_id(&state, &target).ok_or("no available target node")?;
            (
                node_id,
                state
                    .nodes
                    .get(&node_id)
                    .and_then(|node| node.remote.clone()),
            )
        };
        if let Some(remote) = &remote {
            terminate(remote).map_err(|error| error.to_string())?;
        }
        let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
        let runtime = state.nodes.get_mut(&node_id).ok_or("node not found")?;
        if remote.is_none() {
            runtime.child.kill().map_err(|error| error.to_string())?;
        }
        runtime.state = "dead".into();
        state.log(format!("Killed node {node_id}"));
        Ok(())
    }

    pub fn snapshot(&self) -> ControlSnapshot {
        snapshot(&self.inner)
    }

    /// Benchmark hot path: one observed node, no logs, fleet projection, wallet
    /// projection, or cloning the same exchange status once per validator.
    /// Extra owners required by an embedded load driver, without requesting
    /// every unrelated resting order in each monitoring sample.
    pub fn watch_order_accounts(&self, accounts: Vec<Address>) -> Result<(), String> {
        if accounts.len() > 1000 {
            return Err("at most 1000 watched accounts".into());
        }
        self.inner
            .lock()
            .map_err(|_| "controller lock poisoned")?
            .watched_order_accounts = accounts;
        Ok(())
    }

    pub fn benchmark_status(&self) -> Option<NodeStatus> {
        let state = self.inner.lock().ok()?;
        best_status(&state).cloned()
    }

    pub fn refresh_node_status(&self, node_id: NodeId) -> Result<NodeRuntimeSnapshot, String> {
        let (address, auth_key, run_id, remote) = {
            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            if !state.running {
                return Err("cluster is not running".into());
            }
            let node = state.nodes.get_mut(&node_id).ok_or("node not found")?;
            if matches!(node.child.try_wait(), Ok(Some(_))) {
                node.state = "dead".into();
            }
            if node.state == "dead" {
                return Err(format!("node {node_id} is dead"));
            }
            let address = node.address.ok_or("node has no registered address")?;
            let remote = node.remote.is_some();
            (address, state.auth_key, state.run_id, remote)
        };
        let started = Instant::now();
        let result = request_status(
            address,
            &auth_key,
            Duration::from_secs(if remote { 5 } else { 2 }),
        );
        let error = result.as_ref().err().map(ToString::to_string);
        update_status(&self.inner, run_id, node_id, started, result);
        let state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
        if state.run_id != run_id || !state.running {
            return Err("cluster stopped during query".into());
        }
        if let Some(error) = error {
            return Err(format!("node {node_id} status query failed: {error}"));
        }
        let node = state.nodes.get(&node_id).ok_or("node no longer exists")?;
        Ok(node_snapshot(node_id, node))
    }
}

#[cfg(test)]
mod tests;
