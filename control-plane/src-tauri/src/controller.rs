use crate::types::{
    AgentConfig, AgentSnapshot, ClaimProgress, ControlSnapshot, LaunchConfig, LoadConfig, LoadPreset, LoadSnapshot, MarketMakerConfig, WalletAccount,
    MarketMakerSnapshot, MetricsSnapshot,
    NodeRuntimeSnapshot, TargetNode,
};
use crate::binance_depth::{PartialDepth, STREAM_URL, desired_book, mid_price};
use light_dex::{
    blockchain::Transaction,
    crypto::{FaucetTerms, SessionManifest, create_session_with_faucet, read_secret, sign_transaction, new_key, write_secret},
    exchange::address::Address,
    message::Message,
    message::{hex_encode, sign_line, verify_line},
    node::{ControlMessage, NodeId, NodeStatus},
    participants::market_maker::{DesiredBook, QuoteAction, plan_actions},
    participants::strategies::AgentStrategy,
};
use ed25519_dalek::SigningKey;
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

type Shared = Arc<Mutex<ControllerInner>>;

fn slot_address(state: &ControllerInner, slot: u64) -> Option<Address> {
    state.wallet.get(&slot).copied()
}

fn address_slot(state: &ControllerInner, address: Address) -> Option<u64> {
    state.wallet.iter().find_map(|(slot, candidate)| (*candidate == address).then_some(*slot))
}

struct RuntimeNode {
    child: Child,
    address: Option<SocketAddr>,
    status: Option<NodeStatus>,
    state: String,
    last_sample: Option<(Instant, u64, u64)>,
    last_poll_started: Option<Instant>,
    last_status_at_ms: Option<u64>,
    tx_rate: f64,
    block_rate: f64,
    paused_until: Option<Instant>,
}

struct ControllerInner {
    nodes: BTreeMap<NodeId, RuntimeNode>,
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
    pending: Option<PendingAgentTransaction>,
    last_step: Instant,
    last_error: Option<String>,
    resting_since: HashMap<u64, Instant>,
    group_id: Option<u64>,
}

fn new_agent(config: AgentConfig, group_id: Option<u64>) -> AgentRuntime {
    AgentRuntime {
        config, rng: StdRng::seed_from_u64(rand::random()),
        desired: DesiredBook::default(), active: true, stopping: false,
        pending: None, last_step: Instant::now() - Duration::from_secs(1),
        last_error: None, resting_since: HashMap::new(), group_id,
    }
}

struct PendingAgentTransaction {
    transaction: Transaction,
    digest: String,
    action: Option<QuoteAction>,
    last_sent: Instant,
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
        self.logs.push_back(message.into());
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

impl ControllerState {
    pub fn create_address(&self) -> Result<WalletAccount, String> {
        let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
        if !state.running { return Err("start the simulation before creating an address".into()); }
        let dir = PathBuf::from(state.active_data_dir.as_deref().ok_or("no active session directory")?);
        let label = state.wallet.keys().next_back().map_or(0, |last| last.saturating_add(1));
        if state.wallet.contains_key(&label) { return Err("wallet label space exhausted".into()); }
        let key = new_key().map_err(|error| error.to_string())?;
        let address = Address::from_public_key(&key.verifying_key());
        write_secret(&dir.join("keys").join(format!("account-{label}.key")), &key).map_err(|error| error.to_string())?;
        let mut wallet = state.wallet.clone();
        wallet.insert(label, address);
        let temporary = dir.join(format!("wallet.json.tmp-{:016x}", rand::random::<u64>()));
        let result = (|| -> io::Result<()> {
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
            file.write_all(&serde_json::to_vec_pretty(&wallet).map_err(io::Error::other)?)?;
            file.sync_all()?;
            std::fs::rename(&temporary, dir.join("wallet.json"))?;
            #[cfg(unix)]
            std::fs::File::open(&dir)?.sync_all()?;
            Ok(())
        })();
        if let Err(error) = result { let _ = std::fs::remove_file(&temporary); return Err(error.to_string()); }
        state.wallet = wallet;
        state.account_keys.insert(label, key);
        state.log(format!("Created wallet #{label}: {address} (unfunded)"));
        Ok(WalletAccount { label, address })
    }

    /// Create independent participant identities, submit their signed claims,
    /// and return only after every account is funded on-chain.
    pub fn claim_new_accounts(&self, count: u64) -> Result<Vec<WalletAccount>, String> {
        if count == 0 || count > 100 { return Err("claim batch must contain 1–100 accounts".into()); }
        let run_id = {
            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            if !state.running { return Err("launch the cluster before claiming accounts".into()); }
            if state.claim_progress.as_ref().is_some_and(|progress| progress.active) {
                return Err("another claim batch is in progress".into());
            }
            if state.pending_claims.len() > count as usize {
                return Err(format!("{} incomplete claims must be retried first", state.pending_claims.len()));
            }
            let status = best_status(&state).ok_or("wait for a responsive node")?;
            let remaining = status.faucet_remaining_claims.ok_or("claim allowance unavailable")?;
            let already_claimed = state.pending_claims.iter().filter(|wallet| status.account_nonces.iter()
                .any(|account| account.account_id == wallet.address && account.nonce >= 1)).count() as u64;
            if remaining < count - already_claimed { return Err(format!("faucet has {remaining} claims remaining; this batch needs {}", count - already_claimed)); }
            state.claim_progress = Some(ClaimProgress { active: true, requested: count as usize, confirmed: already_claimed as usize, error: None });
            state.run_id
        };
        let result = self.claim_new_accounts_inner(count as usize, run_id);
        if let Ok(mut state) = self.inner.lock() {
            if state.run_id == run_id {
                if let Some(progress) = state.claim_progress.as_mut() {
                    progress.active = false;
                    progress.error = result.as_ref().err().cloned();
                }
                if let Err(error) = &result { state.log(format!("Claim batch stopped: {error}")); }
            }
        }
        result
    }

    fn claim_new_accounts_inner(&self, count: usize, run_id: u64) -> Result<Vec<WalletAccount>, String> {
        while self.inner.lock().map_err(|_| "controller lock poisoned")?.pending_claims.len() < count {
            let wallet = self.create_address()?;
            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            if !state.running || state.run_id != run_id { return Err("cluster stopped during account creation".into()); }
            state.pending_claims.push(wallet);
        }
        let wallets = self.inner.lock().map_err(|_| "controller lock poisoned")?.pending_claims.clone();
        let (auth_key, faucet) = {
            let state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            (state.auth_key, state.signing_manifest.as_ref().ok_or("no active signed session")?.faucet)
        };
        let mut last_sent = HashMap::<Address, Instant>::new();
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut poll_cursor = 0usize;
        loop {
            let (targets, destination, status) = {
                let state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
                if !state.running || state.run_id != run_id { return Err("cluster stopped while waiting for claims".into()); }
                (state.nodes.iter().filter_map(|(&id, node)| (node.state != "dead").then_some((id, node.address?))).collect::<Vec<_>>(),
                    resolve_target(&state, &TargetNode::Primary), best_status(&state).cloned())
            };
            let confirmed = |status: &NodeStatus, wallet: &WalletAccount| {
                status.account_nonces.iter().any(|account| account.account_id == wallet.address && account.nonce >= 1)
                    && status.agent_balances.iter().any(|balance| balance.account_id == wallet.address
                        && balance.usd_available >= faucet.usd_per_claim && balance.btc_available >= faucet.btc_per_claim)
            };
            if let Some(status) = &status {
                let done = wallets.iter().filter(|wallet| confirmed(status, wallet)).count();
                if let Ok(mut state) = self.inner.lock() {
                    if let Some(progress) = state.claim_progress.as_mut() { progress.confirmed = done; }
                }
                if done == wallets.len() {
                    let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
                    state.pending_claims.clear();
                    state.log(format!("{} participant claims committed", wallets.len()));
                    return Ok(wallets);
                }
                if let Some(outcome) = status.recent_transaction_outcomes.iter().find(|outcome| !outcome.success
                    && wallets.iter().any(|wallet| wallet.address == outcome.account_id)) {
                    return Err(format!("claim for {} failed: {}", outcome.account_id, outcome.error.as_deref().unwrap_or("unknown error")));
                }
            }
            if let Some((_, address)) = destination {
                for wallet in &wallets {
                    if status.as_ref().is_some_and(|status| confirmed(status, wallet)) { continue; }
                    if last_sent.get(&wallet.address).is_some_and(|at| at.elapsed() < Duration::from_secs(2)) { continue; }
                    let tx = Transaction::ClaimAirdrop { account_id: wallet.address, nonce: 0 };
                    match send_signed_transaction(&self.inner, address, tx, &auth_key) {
                        Ok(()) => {
                            last_sent.insert(wallet.address, Instant::now());
                            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
                            state.submitted += 1;
                            state.submit_times.push_back(Instant::now());
                        }
                        Err(error) => log(&self.inner, format!("Claim submission for wallet #{} failed; retrying: {error}", wallet.label)),
                    }
                }
            }
            for offset in 0..targets.len() {
                let (node_id, address) = targets[(poll_cursor + offset) % targets.len()];
                let started = Instant::now();
                let Ok(status) = request_status(address, &auth_key, Duration::from_millis(400)) else { continue; };
                update_status(&self.inner, run_id, node_id, started, Ok(status));
                poll_cursor = poll_cursor.wrapping_add(offset + 1);
                break;
            }
            if Instant::now() >= deadline { return Err(format!("claim confirmation timed out; {} wallets remain retryable", wallets.len())); }
            thread::sleep(Duration::from_millis(200));
        }
    }

    pub fn airdrop_new_address(&self, from_label: u64, asset: u64, amount: u64, target: TargetNode) -> Result<WalletAccount, String> {
        if amount == 0 { return Err("airdrop amount must be positive".into()); }
        let (from, nonce, destination, auth_key) = {
            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            if !state.running { return Err("start the simulation before airdropping".into()); }
            let from = slot_address(&state, from_label).ok_or("unknown funding wallet label")?;
            if state.market_maker.as_ref().is_some_and(|mm| (mm.active || mm.stopping) && mm.config.account_id == from_label)
                || state.agents.values().any(|agent| agent.config.account_id == from_label)
                || state.load.as_ref().is_some_and(|load| load_uses_account(&load.config.preset, from_label)) {
                return Err("funding wallet is controlled by an active participant or load sender".into());
            }
            let status = best_status(&state).ok_or("wait for a responsive node")?;
            let nonce = status.account_nonces.iter().find(|account| account.account_id == from)
                .map(|account| account.nonce).ok_or("funding address is not on chain")?;
            let available = status.agent_balances.iter().find(|balance| balance.account_id == from)
                .map(|balance| if asset == 0 { balance.usd_available } else if asset == 1 { balance.btc_available } else { 0 })
                .ok_or("funding balance unavailable")?;
            if available < amount { return Err("funding address has insufficient available balance".into()); }
            let destination = resolve_target(&state, &target).ok_or("no available target node")?;
            if let Some((previous_nonce, digest)) = state.pending_airdrops.get(&from) {
                let failed = digest.as_ref().is_some_and(|digest| status.recent_transaction_outcomes.iter()
                    .any(|outcome| outcome.digest == *digest && !outcome.success));
                if nonce <= *previous_nonce && !failed {
                    return Err("an earlier airdrop from this wallet is still awaiting confirmation".into());
                }
            }
            state.pending_airdrops.insert(from, (nonce, None));
            (from, nonce, destination, state.auth_key)
        };
        let created = match self.create_address() {
            Ok(created) => created,
            Err(error) => { self.inner.lock().unwrap().pending_airdrops.remove(&from); return Err(error); }
        };
        let transaction = Transaction::Transfer { from, to: created.address, asset, amount, nonce };
        if let Err(error) = send_signed_transaction(&self.inner, destination.1, transaction.clone(), &auth_key) {
            self.inner.lock().unwrap().pending_airdrops.remove(&from);
            return Err(format!("wallet #{} was created, but funding submission failed: {error}", created.label));
        }
        let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
        state.pending_airdrops.insert(from, (nonce, Some(hex_encode(&transaction.to_digest()))));
        state.submitted += 1;
        state.submit_times.push_back(Instant::now());
        state.log(format!("Airdrop submitted: wallet #{from_label} -> #{} ({}) amount {amount} asset {asset}, nonce {nonce}, tx {}", created.label, created.address, hex_encode(&transaction.to_digest())));
        Ok(created)
    }

    /// Fund a controller-owned address and wait for the transfer to commit.
    /// Serial confirmation lets a batch reuse a donor without guessing nonces.
    pub fn fund_address(&self, from_label: u64, to: Address, asset: u64, amount: u64, target: TargetNode) -> Result<(), String> {
        if amount == 0 || asset > 1 { return Err("funding requires a positive USD or BTC amount".into()); }
        let (from, nonce, expected_received, destination, auth_key, run_id) = {
            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            if !state.running { return Err("start the simulation before funding addresses".into()); }
            if !state.wallet.values().any(|address| *address == to) { return Err("recipient is not in the controller wallet".into()); }
            let from = slot_address(&state, from_label).ok_or("unknown funding wallet label")?;
            if from == to { return Err("funding wallet and recipient must differ".into()); }
            if state.market_maker.as_ref().is_some_and(|mm| (mm.active || mm.stopping) && mm.config.account_id == from_label)
                || state.agents.values().any(|agent| agent.config.account_id == from_label)
                || state.load.as_ref().is_some_and(|load| load_uses_account(&load.config.preset, from_label)) {
                return Err("funding wallet is controlled by an active participant or load sender".into());
            }
            let status = best_status(&state).ok_or("wait for a responsive node")?;
            let nonce = status.account_nonces.iter().find(|account| account.account_id == from)
                .map(|account| account.nonce).ok_or("funding address is not on chain")?;
            let available = status.agent_balances.iter().find(|balance| balance.account_id == from)
                .map(|balance| if asset == 0 { balance.usd_available } else { balance.btc_available })
                .ok_or("funding balance unavailable")?;
            if available < amount { return Err(format!("wallet #{from_label} has insufficient {}", if asset == 0 { "USD" } else { "BTC" })); }
            let recipient_before = status.agent_balances.iter().find(|balance| balance.account_id == to)
                .map_or(0, |balance| if asset == 0 { balance.usd_available } else { balance.btc_available });
            let expected_received = recipient_before.checked_add(amount).ok_or("recipient balance would overflow")?;
            if let Some((previous_nonce, digest)) = state.pending_airdrops.get(&from) {
                let failed = digest.as_ref().is_some_and(|digest| status.recent_transaction_outcomes.iter()
                    .any(|outcome| outcome.digest == *digest && !outcome.success));
                if nonce <= *previous_nonce && !failed { return Err("an earlier funding transfer from this wallet is still awaiting confirmation".into()); }
            }
            let destination = resolve_target(&state, &target).ok_or("no available target node")?;
            state.pending_airdrops.insert(from, (nonce, None));
            (from, nonce, expected_received, destination, state.auth_key, state.run_id)
        };
        let transaction = Transaction::Transfer { from, to, asset, amount, nonce };
        let digest = hex_encode(&transaction.to_digest());
        if let Err(error) = send_signed_transaction(&self.inner, destination.1, transaction, &auth_key) {
            self.inner.lock().unwrap().pending_airdrops.remove(&from);
            return Err(format!("funding submission failed: {error}"));
        }
        {
            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            state.pending_airdrops.insert(from, (nonce, Some(digest.clone())));
            state.submitted += 1;
            state.submit_times.push_back(Instant::now());
            state.log(format!("Funding submitted: wallet #{from_label} -> {to}, amount {amount} asset {asset}, nonce {nonce}, tx {digest}"));
        }
        let deadline = Instant::now() + Duration::from_secs(45);
        let mut poll_cursor = 0usize;
        loop {
            let targets = {
                let state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
                if !state.running || state.run_id != run_id { return Err("cluster stopped while waiting for funding".into()); }
                state.nodes.iter().filter_map(|(&id, node)| (node.state != "dead").then_some((id, node.address?))).collect::<Vec<_>>()
            };
            for offset in 0..targets.len() {
                let (node_id, address) = targets[(poll_cursor + offset) % targets.len()];
                let started = Instant::now();
                let Ok(status) = request_status(address, &auth_key, Duration::from_millis(400)) else { continue; };
                let outcome = status.recent_transaction_outcomes.iter().find(|outcome| outcome.digest == digest);
                let confirmed = outcome.is_some_and(|outcome| outcome.success)
                    || (status.account_nonces.iter().any(|account| account.account_id == from && account.nonce > nonce)
                        && status.agent_balances.iter().any(|balance| balance.account_id == to &&
                            if asset == 0 { balance.usd_available >= expected_received } else { balance.btc_available >= expected_received }));
                let failure = outcome.filter(|outcome| !outcome.success).map(|outcome| outcome.error.clone().unwrap_or_else(|| "transaction failed".into()));
                update_status(&self.inner, run_id, node_id, started, Ok(status));
                if let Some(error) = failure {
                    self.inner.lock().unwrap().pending_airdrops.remove(&from);
                    return Err(format!("funding transfer failed: {error}"));
                }
                if confirmed {
                    let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
                    if state.run_id != run_id { return Err("cluster stopped while waiting for funding".into()); }
                    state.pending_airdrops.remove(&from);
                    state.log(format!("Funding committed: wallet #{from_label} -> {to}, amount {amount} asset {asset}"));
                    return Ok(());
                }
                poll_cursor = poll_cursor.wrapping_add(offset + 1);
                break;
            }
            if Instant::now() >= deadline { return Err(format!("funding confirmation timed out for {to}; do not retry this donor until the transfer outcome is known")); }
            thread::sleep(Duration::from_millis(200));
        }
    }

    pub fn launch(&self, config: LaunchConfig) -> Result<(), String> {
        if config.node_count == 0 {
            return Err("node count must be at least one".into());
        }
        let run_id = {
            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            if state.running || state.launching || state.stopping {
                return Err("simulation already running".into());
            }
            state.run_id += 1;
            state.auth_key = rand::random();
            state.launching = true;
            state.submitted = 0;
            state.submit_times.clear();
            state.nodes.clear();
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
            if state.stopping || (!state.running && !state.launching) { return; }
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
        let (target, auth_key) = {
            let state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            let account_id = transaction.account_id();
            let account_slot = address_slot(&state, account_id).ok_or("sender address is not in the controller wallet")?;
            if state.pending_airdrops.contains_key(&account_id) {
                return Err(format!("account {account_id} is funding another address"));
            }
            if state.market_maker.as_ref().is_some_and(|mm| (mm.active || mm.stopping)
                && mm.config.account_id == account_slot) {
                return Err(format!("account {account_id} is controlled by the market maker"));
            }
            if state.agents.values().any(|agent| agent.config.account_id == account_slot) {
                return Err(format!("account {account_id} is controlled by a participant"));
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
            let confirmed_nonce = state
                .nodes
                .values()
                .filter(|node| node.state == "ok")
                .filter_map(|node| node.status.as_ref())
                .max_by_key(|status| status.chain_height)
                .and_then(|status| {
                    status
                        .account_nonces
                        .iter()
                        .find(|account| account.account_id == account_id)
                })
                .map(|account| account.nonce)
                .ok_or_else(|| format!("no confirmed nonce available for account {account_id}"))?;
            if !nonce_in_window(confirmed_nonce, transaction.nonce(), future_nonce_window) {
                return Err(format!(
                    "account {account_id} permits nonce {confirmed_nonce}..={}; transaction uses {}",
                    confirmed_nonce.saturating_add(future_nonce_window), transaction.nonce()
                ));
            }
            (resolve_target(&state, &target), state.auth_key)
        }
        ;
        let target = target.ok_or_else(|| "no available target node".to_string())?;
        let shared = self.inner.clone();
        thread::spawn(move || {
            match send_signed_transaction(&shared, target.1, transaction.clone(), &auth_key) {
                Ok(()) => {
                    let mut state = shared.lock().unwrap();
                    state.submitted += 1;
                    state.submit_times.push_back(Instant::now());
                    state.log(format!("Submitted {transaction:?} to node {}", target.0));
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
        }
        ;
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

    pub fn start_load(&self, config: LoadConfig) -> Result<(), String> {
        if !config.tps.is_finite() || config.tps <= 0.0 || config.tps > 10_000.0 {
            return Err("TPS must be a finite value greater than 0 and at most 10,000".into());
        }
        if let LoadPreset::RoundRobinTransfers {
            first_account,
            account_count,
            ..
        }
        | LoadPreset::RandomTransfers {
            first_account,
            account_count,
            ..
        } = &config.preset
        {
            if *account_count < 2
                || *account_count > 1_000
                || first_account.checked_add(*account_count).is_none()
            {
                return Err("transfer presets require 2 to 1,000 accounts in a valid range".into());
            }
        }
        if matches!(&config.preset,
            LoadPreset::RoundRobinTransfers { amount: 0, .. } | LoadPreset::RandomTransfers { amount: 0, .. }) {
            return Err("transfer amount must be greater than zero".into());
        }

        let load_id = {
            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            if !state.running {
                return Err("start the simulation before starting load generation".into());
            }
            let status = state
                .nodes
                .values()
                .filter(|node| node.state == "ok")
                .filter_map(|node| node.status.as_ref())
                .max_by_key(|status| status.chain_height)
                .ok_or_else(|| "wait for a responsive node before starting load".to_string())?;
            let (first, count) = match &config.preset {
                LoadPreset::BookTaker { account_id, .. } => (*account_id, 1),
                LoadPreset::RoundRobinTransfers {
                    first_account,
                    account_count,
                    ..
                }
                | LoadPreset::RandomTransfers {
                    first_account,
                    account_count,
                    ..
                } => (*first_account, *account_count),
            };
            let end = first
                .checked_add(count)
                .ok_or_else(|| "invalid account range".to_string())?;
            if (first..end).any(|id| {
                !status
                    .account_nonces
                    .iter()
                    .any(|account| Some(account.account_id) == slot_address(&state, id))
            }) {
                return Err(
                    "one or more preset accounts do not exist in the current cluster".into(),
                );
            }
            if state.market_maker.as_ref().is_some_and(|mm| (mm.active || mm.stopping)
                && (first..end).contains(&mm.config.account_id)) {
                return Err("sender account range overlaps the market maker".into());
            }
            if state.agents.values().any(|agent| (first..end).contains(&agent.config.account_id)) {
                return Err("sender account range overlaps a participant".into());
            }
            if (first..end).any(|id| slot_address(&state, id).is_some_and(|address| state.pending_airdrops.contains_key(&address))) {
                return Err("sender account range overlaps a pending funding transfer".into());
            }
            state.next_load_id += 1;
            let load_id = state.next_load_id;
            let pending = state
                .load
                .take()
                .map(|load| load.pending)
                .unwrap_or_default();
            state.load = Some(LoadRuntime {
                id: load_id,
                cursor: 0,
                pending,
                failed: HashMap::new(),
                config,
            });
            state.log("Load generator started or updated");
            load_id
        };

        spawn_load_loop(self.inner.clone(), load_id);
        Ok(())
    }

    pub fn stop_load(&self) {
        let mut state = self.inner.lock().unwrap();
        if state.load.take().is_some() {
            state.log("Load generator stopped");
        }
    }

    pub fn start_market_maker(&self, config: MarketMakerConfig) -> Result<(), String> {
        if config.levels == 0 || config.levels > 5 {
            return Err("market maker levels must be between 1 and 5".into());
        }
        if !(250..=60_000).contains(&config.refresh_ms) {
            return Err("market maker refresh must be between 250 and 60,000 ms".into());
        }
        let mut spawn = None;
        {
            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            if !state.running { return Err("start the simulation first".into()); }
            let status = best_status(&state).ok_or("wait for a responsive node")?;
            if !status.account_nonces.iter().any(|account| Some(account.account_id) == slot_address(&state, config.account_id)) {
                return Err("market maker account does not exist".into());
            }
            if !state.market_maker.as_ref().is_some_and(|mm| mm.active)
                && status.open_orders.iter().any(|order| Some(order.account_id) == slot_address(&state, config.account_id)) {
                return Err("market maker account already has open orders; use a clean account".into());
            }
            if state.load.as_ref().is_some_and(|load| load_uses_account(&load.config.preset, config.account_id)) {
                return Err("market maker account is already used by the transaction sender".into());
            }
            if state.agents.values().any(|agent| agent.config.account_id == config.account_id) {
                return Err("market maker account is already used by a participant".into());
            }
            match state.market_maker.as_mut() {
                Some(mm) if mm.active => {
                    if mm.config.account_id != config.account_id {
                        return Err("stop and clear the old account's quotes before changing accounts".into());
                    }
                    mm.config = config;
                    mm.last_applied = None;
                    mm.last_error = None;
                    state.log("Market maker settings updated");
                }
                Some(mm) if mm.stopping => return Err("wait for existing quotes to cancel first".into()),
                _ => {
                    state.next_market_maker_id += 1;
                    let id = state.next_market_maker_id;
                    state.market_maker = Some(MarketMakerRuntime {
                        id, config, active: true, stopping: false,
                        latest_depth: None, feed_at: None, desired: DesiredBook::default(),
                        last_applied: None, pending: VecDeque::new(), last_error: None,
                    });
                    state.log("Market maker started; waiting for Binance BTCUSDT depth");
                    spawn = Some(id);
                }
            }
        }
        if let Some(id) = spawn {
            spawn_binance_feed(self.inner.clone(), id);
            spawn_market_maker_loop(self.inner.clone(), id);
        }
        Ok(())
    }

    pub fn stop_market_maker(&self) {
        let mut state = self.inner.lock().unwrap();
        if let Some(mm) = state.market_maker.as_mut() {
            if mm.active {
                mm.active = false;
                mm.stopping = true;
                mm.desired = DesiredBook::default();
                state.log("Market maker stopping; cancelling remaining quotes");
            }
        }
    }

    pub fn start_agent(&self, config: AgentConfig) -> Result<(), String> {
        config.strategy.validate()?;
        if config.market_id != 0 { return Err("only BTC-USD market 0 exists".into()); }
        let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
        if !state.running { return Err("start the cluster before an agent".into()); }
        let status = best_status(&state).cloned().ok_or("wait for a responsive node")?;
        if !status.account_nonces.iter().any(|account| Some(account.account_id) == slot_address(&state, config.account_id)) {
            return Err("agent account does not exist".into());
        }
        if state.market_maker.as_ref().is_some_and(|mm| (mm.active || mm.stopping) && mm.config.account_id == config.account_id)
            || state.load.as_ref().is_some_and(|load| load_uses_account(&load.config.preset, config.account_id))
            || slot_address(&state, config.account_id).is_some_and(|address| state.pending_airdrops.contains_key(&address))
            || state.agents.values().any(|agent| agent.config.agent_id != config.agent_id && agent.config.account_id == config.account_id) {
            return Err("account already belongs to another participant".into());
        }
        if let Some(agent) = state.agents.get_mut(&config.agent_id) {
            if agent.config.account_id != config.account_id || agent.stopping {
                return Err("stop and clear the old agent before changing its account".into());
            }
            if std::mem::discriminant(&agent.config.strategy) != std::mem::discriminant(&config.strategy) {
                return Err("stop and clear the agent before changing strategy presets".into());
            }
            agent.config = config;
            agent.last_error = None;
            agent.last_step = Instant::now() - Duration::from_secs(1);
            state.log("Participant settings updated");
        } else {
            if status.open_orders.iter().any(|order| Some(order.account_id) == slot_address(&state, config.account_id)) {
                return Err("agent account has existing open orders; choose a clean account".into());
            }
            state.agents.insert(config.agent_id, new_agent(config, None));
            state.log("Participant started");
        }
        let spawn_feed = state.agents.values().any(|agent| agent.active && agent.config.strategy.needs_binance()) && !state.feed_started;
        if spawn_feed { state.feed_started = true; }
        let run_id = state.run_id;
        drop(state);
        if spawn_feed { spawn_shared_binance_feed(self.inner.clone(), run_id); }
        Ok(())
    }

    /// All-or-nothing activation of a fleet. Existing agent IDs are not
    /// overwritten by a group launch; update them individually instead.
    pub fn start_agents(&self, configs: Vec<AgentConfig>) -> Result<u64, String> {
        if configs.is_empty() || configs.len() > 100 { return Err("fleet must contain 1–100 agents".into()); }
        let mut ids = HashSet::new();
        let mut accounts = HashSet::new();
        for config in &configs {
            config.strategy.validate()?;
            if config.market_id != 0 { return Err("only BTC-USD market 0 exists".into()); }
            if !ids.insert(config.agent_id) || !accounts.insert(config.account_id) {
                return Err("fleet agent IDs and accounts must be unique".into());
            }
        }
        let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
        if !state.running { return Err("start the cluster before a fleet".into()); }
        let status = best_status(&state).cloned().ok_or("wait for a responsive node")?;
        for config in &configs {
            if state.agents.contains_key(&config.agent_id) { return Err(format!("agent ID {} is already active", config.agent_id)); }
            if !status.account_nonces.iter().any(|account| Some(account.account_id) == slot_address(&state, config.account_id)) {
                return Err(format!("account {} does not exist", config.account_id));
            }
            if status.open_orders.iter().any(|order| Some(order.account_id) == slot_address(&state, config.account_id))
                || slot_address(&state, config.account_id).is_some_and(|address| state.pending_airdrops.contains_key(&address))
                || state.agents.values().any(|agent| agent.config.account_id == config.account_id)
                || state.market_maker.as_ref().is_some_and(|mm| (mm.active || mm.stopping) && mm.config.account_id == config.account_id)
                || state.load.as_ref().is_some_and(|load| load_uses_account(&load.config.preset, config.account_id)) {
                return Err(format!("account {} is already in use or has open orders", config.account_id));
            }
        }
        state.next_group_id += 1;
        let group_id = state.next_group_id;
        for config in configs {
            state.agents.insert(config.agent_id, new_agent(config, Some(group_id)));
        }
        let spawn_feed = state.agents.values().any(|agent| agent.active && agent.config.strategy.needs_binance()) && !state.feed_started;
        if spawn_feed { state.feed_started = true; }
        let run_id = state.run_id;
        state.log(format!("Participant fleet {group_id} started"));
        drop(state);
        if spawn_feed { spawn_shared_binance_feed(self.inner.clone(), run_id); }
        Ok(group_id)
    }

    pub fn stop_agent(&self, agent_id: u64) -> Result<(), String> {
        let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
        let agent = state.agents.get_mut(&agent_id).ok_or("agent not found")?;
        agent.active = false;
        agent.stopping = true;
        agent.desired = DesiredBook::default();
        state.log(format!("Participant {agent_id} stopping"));
        Ok(())
    }

    pub fn stop_agents(&self, agent_ids: Vec<u64>) -> Result<(), String> {
        let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
        if agent_ids.iter().any(|id| !state.agents.contains_key(id)) { return Err("agent not found".into()); }
        for id in agent_ids {
            let agent = state.agents.get_mut(&id).expect("prevalidated agent ID");
            agent.active = false;
            agent.stopping = true;
            agent.desired = DesiredBook::default();
        }
        state.log("Participant group stopping");
        Ok(())
    }

    pub fn kill_node(&self, target: TargetNode) -> Result<(), String> {
        let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
        let node_id = resolve_target_id(&state, &target)
            .ok_or_else(|| "no available target node".to_string())?;
        let runtime = state
            .nodes
            .get_mut(&node_id)
            .ok_or_else(|| "node not found".to_string())?;
        runtime.child.kill().map_err(|error| error.to_string())?;
        runtime.state = "dead".into();
        state.log(format!("Killed node {node_id}"));
        Ok(())
    }

    pub fn snapshot(&self) -> ControlSnapshot {
        snapshot(&self.inner)
    }

    pub fn refresh_node_status(&self, node_id: NodeId) -> Result<NodeRuntimeSnapshot, String> {
        let (address, auth_key, run_id) = {
            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            if !state.running { return Err("cluster is not running".into()); }
            let node = state.nodes.get_mut(&node_id).ok_or("node not found")?;
            if matches!(node.child.try_wait(), Ok(Some(_))) { node.state = "dead".into(); }
            if node.state == "dead" { return Err(format!("node {node_id} is dead")); }
            (node.address.ok_or("node has no registered address")?, state.auth_key, state.run_id)
        };
        let started = Instant::now();
        let result = request_status(address, &auth_key, Duration::from_secs(2));
        let error = result.as_ref().err().map(ToString::to_string);
        update_status(&self.inner, run_id, node_id, started, result);
        let state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
        if state.run_id != run_id || !state.running { return Err("cluster stopped during query".into()); }
        if let Some(error) = error { return Err(format!("node {node_id} status query failed: {error}")); }
        let node = state.nodes.get(&node_id).ok_or("node no longer exists")?;
        Ok(node_snapshot(node_id, node))
    }
}

fn launch_cluster(shared: Shared, config: LaunchConfig, run_id: u64) -> io::Result<()> {
    let auth_key = shared.lock().unwrap().auth_key;
    let project = PathBuf::from(&config.project_dir).canonicalize()?;
    let requested_data = PathBuf::from(&config.data_dir);
    let base_data = if requested_data.is_absolute() { requested_data } else { project.join(requested_data) };
    let data_dir = if config.recover_existing {
        return Err(io::Error::other("live recovery is unavailable until durable vote and lock state are persisted; use offline replay to inspect old sessions"));
    } else {
        let suffix = format!("run-{}-{:08x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(io::Error::other)?.as_millis(), rand::random::<u32>());
        let path = base_data.join(suffix);
        std::fs::create_dir_all(&path)?;
        std::fs::write(path.join("consensus.txt"), config.consensus.as_str())?;
        path
    };
    let data_dir = data_dir.canonicalize()?;
    let manifest = Arc::new(create_session_with_faucet(&data_dir, config.consensus.as_str(), config.node_count, FaucetTerms {
        max_claims: config.claim_limit,
        usd_per_claim: config.claim_usd,
        btc_per_claim: config.claim_btc,
    })?);
    let wallet: BTreeMap<u64, Address> = serde_json::from_slice(&std::fs::read(data_dir.join("wallet.json"))?)?;
    let mut account_keys = BTreeMap::new();
    for (&id, &address) in &wallet {
        let key = read_secret(&data_dir.join("keys").join(format!("account-{id}.key")))?;
        if Address::from_public_key(&key.verifying_key()) != address { return Err(io::Error::other("wallet key/address mismatch")); }
        account_keys.insert(id, key);
    }
    {
        let mut state = shared.lock().unwrap();
        state.active_data_dir = Some(data_dir.display().to_string());
        state.signing_manifest = Some(manifest);
        state.account_keys = account_keys;
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
        log(&shared, if release { "Running cargo build --release --bin light_dex..." }
            else { "Running cargo build --bin light_dex..." });
        let mut command = Command::new("cargo");
        command.args(["build", "--bin", "light_dex"]);
        if release { command.arg("--release"); }
        let output = command.current_dir(&project)
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

    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let parent_address = listener.local_addr()?;
    log(&shared, format!("Bootstrap listening at {parent_address}"));

    for node_id in 0..config.node_count {
        let mut state = shared.lock().unwrap();
        if state.run_id != run_id { return Err(io::Error::new(io::ErrorKind::Interrupted, "launch cancelled")); }
        let mut child = Command::new(&binary)
            .args([
                "--node",
                &node_id.to_string(),
                "--parent",
                &parent_address.to_string(),
                "--block-time-ms",
                &config.block_time_ms.to_string(),
                "--timeout-ms",
                &config.timeout_ms.to_string(),
                "--consensus",
                config.consensus.as_str(),
                "--data-dir",
                &data_dir.display().to_string(),
                "--chain-config",
                &data_dir.join("manifest.json").display().to_string(),
                "--validator-key",
                &data_dir.join("keys").join(format!("validator-{node_id}.key")).display().to_string(),
            ])
            .current_dir(&project)
            .env("LIGHT_DEX_AUTH_KEY", hex_encode(&auth_key))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        state.nodes.insert(
            node_id,
            RuntimeNode {
                child,
                address: None,
                status: None,
                state: "starting".into(),
                last_sample: None,
                last_poll_started: None,
                last_status_at_ms: None,
                tx_rate: 0.0,
                block_rate: 0.0,
                paused_until: None,
            },
        );
        drop(state);
        if let Some(reader) = stdout {
            spawn_output_reader(node_id, reader, shared.clone());
        }
        if let Some(reader) = stderr {
            spawn_output_reader(node_id, reader, shared.clone());
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
        let mut stream = TcpStream::connect_timeout(address, Duration::from_secs(5))?;
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
                log(&shared, format!("Node {node_id} ready"));
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
    spawn_agent_loop(shared, run_id);
    Ok(())
}

fn spawn_agent_loop(shared: Shared, run_id: u64) {
    thread::spawn(move || loop {
        let ids = {
            let state = shared.lock().unwrap();
            if !state.running || state.run_id != run_id { break; }
            state.agents.keys().copied().collect::<Vec<_>>()
        };
        for id in ids { participant_step(&shared, id); }
        thread::sleep(Duration::from_millis(50));
    });
}

fn participant_step(shared: &Shared, id: u64) {
    let dispatch = {
        let mut state = shared.lock().unwrap();
        let Some(status) = best_status(&state).cloned() else { return; };
        let feed = state.feed_depth.clone();
        let feed_stale = state.feed_at.is_none_or(|at| at.elapsed() > Duration::from_secs(3));
        let feed_history = state.feed_history.clone();
        let local_history = state.local_mid_history.iter().copied().collect::<Vec<_>>();
        let auth_key = state.auth_key;
        let Some(account_address) = state.agents.get(&id).and_then(|agent| slot_address(&state, agent.config.account_id)) else { return; };
        let Some(agent) = state.agents.get_mut(&id) else { return; };
        let Some(nonce) = status.account_nonces.iter().find(|account| account.account_id == account_address).map(|account| account.nonce) else { return; };
        let Some(balance) = status.agent_balances.iter().find(|balance| balance.account_id == account_address) else { return; };
        let now = Instant::now();
        let own_orders = status.open_orders.iter().filter(|order| order.account_id == account_address && order.market_id == agent.config.market_id).collect::<Vec<_>>();
        let own_ids = own_orders.iter().map(|order| order.order_id).collect::<HashSet<_>>();
        agent.resting_since.retain(|order_id, _| own_ids.contains(order_id));
        for order in &own_orders { agent.resting_since.entry(order.order_id).or_insert(now); }
        let mut transaction = None;
        if let Some(pending) = &mut agent.pending {
            let outcome = status.recent_transaction_outcomes.iter().find(|outcome| outcome.digest == pending.digest);
            if let Some(outcome) = outcome.filter(|outcome| !outcome.success) {
                agent.last_error = Some(format!("Transaction at nonce {} failed: {}; replanning at the confirmed nonce", pending.transaction.nonce(), outcome.error.as_deref().unwrap_or("unknown")));
                agent.pending = None;
                agent.last_step = now;
            } else if nonce > pending.transaction.nonce() {
                agent.pending = None;
            } else if now.duration_since(pending.last_sent) >= Duration::from_secs(3) {
                pending.last_sent = now;
                transaction = Some(pending.transaction.clone());
            }
        }
        if agent.pending.is_none() {
            if agent.stopping && own_orders.is_empty() {
                state.agents.remove(&id);
                state.log(format!("Participant {id} stopped; resting orders cleared"));
                return;
            }
            let interval = Duration::from_millis(agent.config.strategy.interval_ms());
            let refresh_due = now.duration_since(agent.last_step) >= interval;
            if refresh_due || agent.stopping || agent.config.strategy.is_maker() || !own_orders.is_empty() {
                if refresh_due { agent.last_step = now; }
                let account_id = account_address;
                let market_id = agent.config.market_id;
                let strategy = agent.config.strategy.clone();
                let book = status.order_books.iter().find(|book| book.market_id == market_id);
                let mut next = None;
                if agent.stopping {
                    if let Some(order) = own_orders.first() {
                        next = Some((Transaction::CancelOrder { account_id, market_id, order_id: order.order_id, nonce }, Some(QuoteAction::Cancel { order_id: order.order_id })));
                    }
                } else if strategy.is_maker() {
                    if refresh_due {
                        if !feed_stale && agent.last_error.as_deref() == Some("Binance feed stale; cancelling quotes") {
                            agent.last_error = None;
                        }
                        agent.desired = if feed_stale { DesiredBook::default() } else {
                            match &strategy {
                                AgentStrategy::BinanceMirror { levels, .. } => feed.as_ref().map(|depth| desired_book(depth, *levels)).unwrap_or_default(),
                                AgentStrategy::AvellanedaStoikov { volatility_window_seconds, .. } => {
                                    let inventory = balance.btc_available.saturating_add(balance.btc_locked);
                                    feed.as_ref().and_then(mid_price)
                                        .zip(feed_variance(&feed_history, *volatility_window_seconds))
                                        .and_then(|(mid, variance)| strategy.avellaneda_quotes(mid, variance, inventory))
                                        .unwrap_or_default()
                                }
                                _ => DesiredBook::default(),
                            }
                        };
                        if let Some(book) = book {
                            if let Some(ask) = book.asks.first() { for bid in &mut agent.desired.bids { *bid = (*bid).min(ask.price.saturating_sub(1)); } }
                            if let Some(bid) = book.bids.first() { for ask in &mut agent.desired.asks { *ask = (*ask).max(bid.price.saturating_add(1)); } }
                        }
                    }
                    if feed_stale { agent.desired = DesiredBook::default(); agent.last_error = Some("Binance feed stale; cancelling quotes".into()); }
                    next = plan_actions(account_id, &status.open_orders, &agent.desired, true, &[])
                        .into_iter().find_map(|action| {
                            let tx = match action {
                                QuoteAction::Cancel { order_id } => Transaction::CancelOrder { account_id, market_id, order_id, nonce },
                                QuoteAction::Place { side, price } => {
                                    if (side == light_dex::exchange::order::Side::Buy && balance.usd_available < price)
                                        || (side == light_dex::exchange::order::Side::Sell && balance.btc_available == 0) { return None; }
                                    Transaction::PlaceOrder { account_id, market_id, order_type: light_dex::exchange::order::OrderType::LO, side, price, quantity: 1, nonce }
                                }
                            };
                            Some((tx, Some(action)))
                        });
                } else {
                    let expired = match &strategy {
                        AgentStrategy::RandomOrder { order_type: light_dex::exchange::order::OrderType::LO, resting_ttl_ms, max_resting_orders, .. } =>
                            own_orders.iter().find(|order| agent.resting_since.get(&order.order_id).is_some_and(|at| now.duration_since(*at) >= Duration::from_millis(*resting_ttl_ms)))
                                .or_else(|| (own_orders.len() >= *max_resting_orders).then(|| own_orders.first()).flatten()),
                        _ => own_orders.first(),
                    };
                    if let Some(order) = expired {
                        next = Some((Transaction::CancelOrder { account_id, market_id, order_id: order.order_id, nonce }, Some(QuoteAction::Cancel { order_id: order.order_id })));
                    } else if refresh_due && agent.active {
                        let side = match &strategy {
                            AgentStrategy::RandomOrder { .. } => strategy.random_side(&mut agent.rng),
                            AgentStrategy::TrendFollower { .. } => strategy.trend_side(&local_history),
                            _ => None,
                        };
                        if let Some(side) = side {
                            let (order_type, passive_offset) = match strategy {
                                AgentStrategy::RandomOrder { order_type, passive_offset_ticks, .. } => (order_type, passive_offset_ticks),
                                _ => (light_dex::exchange::order::OrderType::IOC, 0),
                            };
                            let price = match (order_type, side) {
                                (light_dex::exchange::order::OrderType::LO, light_dex::exchange::order::Side::Buy) => book.and_then(|book| book.bids.first().map(|level| level.price.saturating_sub(passive_offset).max(1))),
                                (light_dex::exchange::order::OrderType::LO, light_dex::exchange::order::Side::Sell) => book.and_then(|book| book.asks.first().map(|level| level.price.saturating_add(passive_offset))),
                                (_, light_dex::exchange::order::Side::Buy) => book.and_then(|book| book.asks.first().map(|level| level.price)),
                                (_, light_dex::exchange::order::Side::Sell) => book.and_then(|book| book.bids.first().map(|level| level.price)),
                            };
                            if let Some(price) = price.filter(|price| *price > 0) {
                                if (side == light_dex::exchange::order::Side::Buy && balance.usd_available >= price)
                                    || (side == light_dex::exchange::order::Side::Sell && balance.btc_available >= 1) {
                                    next = Some((Transaction::PlaceOrder { account_id, market_id, order_type, side, price, quantity: 1, nonce },
                                        (order_type == light_dex::exchange::order::OrderType::LO).then_some(QuoteAction::Place { side, price })));
                                }
                            }
                        }
                    }
                }
                if let Some((tx, action)) = next {
                    agent.pending = Some(PendingAgentTransaction {
                        digest: hex_encode(&tx.to_digest()), transaction: tx.clone(), action, last_sent: now,
                    });
                    transaction = Some(tx);
                }
            }
        }
        transaction.map(|tx| (tx, agent.config.target.clone(), auth_key))
    };
    let Some((tx, target, auth_key)) = dispatch else { return; };
    let target = { resolve_target(&shared.lock().unwrap(), &target) };
    let result = match target {
        Some((_, address)) => send_signed_transaction(&shared, address, tx, &auth_key),
        None => Err(io::Error::new(io::ErrorKind::NotConnected, "no available target node")),
    };
    let mut state = shared.lock().unwrap();
    if let Some(agent) = state.agents.get_mut(&id) {
        match result {
            Ok(()) => { state.submitted += 1; state.submit_times.push_back(Instant::now()); }
            Err(error) => { agent.last_error = Some(format!("Submission failed; retrying: {error}")); }
        }
    }
}

fn accept_until(listener: &TcpListener, deadline: Instant) -> io::Result<(TcpStream, SocketAddr)> {
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

fn spawn_poll_loop(shared: Shared, poll_ms: u64, poll_mode: crate::types::PollMode, run_id: u64) {
    thread::spawn(move || {
        let mut next_node = 0;
        loop {
            let started = Instant::now();
            let (targets, auth_key) = {
                let mut state = shared.lock().unwrap();
                if !state.running || state.run_id != run_id {
                    break;
                }
                for node in state.nodes.values_mut() {
                    if matches!(node.child.try_wait(), Ok(Some(_))) {
                        node.state = "dead".into();
                    }
                }
                let targets = state
                    .nodes
                    .iter()
                    .filter_map(|(id, node)| (node.state != "dead").then_some((*id, node.address?)))
                    .collect::<Vec<_>>();
                (select_poll_targets(targets, poll_mode, &mut next_node), state.auth_key)
            };
            let (tx, rx) = mpsc::channel();
            let count = targets.len();
            for (id, address) in targets {
                let tx = tx.clone();
                thread::spawn(move || {
                    let started = Instant::now();
                    let _ = tx.send((id, started, request_status(address, &auth_key, Duration::from_millis(400))));
                });
            }
            drop(tx);
            for _ in 0..count {
                let Ok((id, started, result)) = rx.recv_timeout(Duration::from_millis(700)) else {
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

fn select_poll_targets(
    targets: Vec<(NodeId, SocketAddr)>,
    poll_mode: crate::types::PollMode,
    next_node: &mut usize,
) -> Vec<(NodeId, SocketAddr)> {
    match poll_mode {
        crate::types::PollMode::All => targets,
        crate::types::PollMode::RoundRobin => {
            if targets.is_empty() { return targets; }
            let target = targets[*next_node % targets.len()];
            *next_node = next_node.wrapping_add(1);
            vec![target]
        }
    }
}

fn update_status(shared: &Shared, run_id: u64, node_id: NodeId, started: Instant, result: io::Result<NodeStatus>) {
    let now = Instant::now();
    let mut state = shared.lock().unwrap();
    if state.run_id != run_id { return; }
    let Some(node) = state.nodes.get_mut(&node_id) else { return; };
    if matches!(node.child.try_wait(), Ok(Some(_))) { node.state = "dead".into(); return; }
    if node.last_poll_started.is_some_and(|previous| started < previous) { return; }
    node.last_poll_started = Some(started);
    match result {
        Ok(status) => {
            reconcile_load(&mut state, &status);
            if state.last_book_height.is_none_or(|height| status.chain_height > height) {
                if let Some(book) = status.order_books.iter().find(|book| book.market_id == 0) {
                    if let Some((bid, ask)) = book.bids.first().zip(book.asks.first()) {
                        if bid.price < ask.price {
                            let mid = (bid.price as f64 + ask.price as f64) / 2.0;
                            if state.local_mid_history.back().is_none_or(|previous| *previous != mid) {
                                state.local_mid_history.push_back(mid);
                                while state.local_mid_history.len() > 101 { state.local_mid_history.pop_front(); }
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
                let seconds = now.duration_since(then).as_secs_f64().max(0.000_001);
                node.tx_rate = status.transactions_committed.saturating_sub(txs) as f64 / seconds;
                node.block_rate = status.blocks_committed.saturating_sub(blocks) as f64 / seconds;
            }
            node.last_sample = Some((now, status.transactions_committed, status.blocks_committed));
            node.status = Some(status);
            node.state = "ok".into();
            node.last_status_at_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).ok()
                .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64);
        }
        Err(_) => {
            let Some(node) = state.nodes.get_mut(&node_id) else {
                return;
            };
            node.state = if matches!(node.child.try_wait(), Ok(Some(_))) {
                "dead"
            } else if node.paused_until.is_some_and(|until| now < until) {
                "paused"
            } else {
                "unresponsive"
            }
            .into();
        }
    }
}

fn reconcile_load(state: &mut ControllerInner, status: &NodeStatus) {
    let wallet = state.wallet.clone();
    let Some(load) = state.load.as_mut() else {
        return;
    };
    let mut completed = Vec::new();
    let mut failed = Vec::new();
    for (&account_id, pending) in &load.pending {
        if let Some(outcome) = status
            .recent_transaction_outcomes
            .iter()
            .find(|outcome| outcome.digest == pending.digest)
        {
            let nonce_advanced = status.account_nonces.iter().any(|account| {
                Some(account.account_id) == wallet.get(&account_id).copied() && account.nonce > pending.transaction.nonce()
            });
            if !outcome.success || !nonce_advanced {
                failed.push((
                    account_id,
                    pending.transaction.nonce(),
                    outcome.error.clone(),
                ));
                continue;
            }
        }
        if status.account_nonces.iter().any(|account| {
            Some(account.account_id) == wallet.get(&account_id).copied() && account.nonce > pending.transaction.nonce()
        }) {
            completed.push(account_id);
        }
    }
    for account_id in completed {
        load.pending.remove(&account_id);
    }
    for (account_id, nonce, _) in &failed {
        load.pending.remove(account_id);
        load.failed.insert(*account_id, *nonce);
    }
    for (account_id, nonce, error) in failed {
        state.log(format!("Load account {account_id} transaction failed at nonce {nonce} ({}); account paused until load is updated", error.unwrap_or_else(|| "nonce did not advance".into())));
    }
}

fn best_status(state: &ControllerInner) -> Option<&NodeStatus> {
    state.nodes.values().filter(|node| node.state == "ok")
        .filter_map(|node| node.status.as_ref()).max_by_key(|status| status.chain_height)
}

fn load_uses_account(preset: &LoadPreset, account_id: u64) -> bool {
    match *preset {
        LoadPreset::BookTaker { account_id: taker, .. } => taker == account_id,
        LoadPreset::RoundRobinTransfers { first_account, account_count, .. }
        | LoadPreset::RandomTransfers { first_account, account_count, .. } =>
            account_id >= first_account && account_id < first_account.saturating_add(account_count),
    }
}

fn spawn_load_loop(shared: Shared, load_id: u64) {
    thread::spawn(move || {
        loop {
            let started = Instant::now();
            let (transaction, target, tps) = {
                let mut state = shared.lock().unwrap();
                if !state.running {
                    break;
                }
                let status = state
                    .nodes
                    .values()
                    .filter(|node| node.state == "ok")
                    .filter_map(|node| node.status.as_ref())
                    .max_by_key(|status| status.chain_height)
                    .cloned();
                let wallet = state.wallet.clone();
                let Some(load) = state.load.as_mut() else {
                    break;
                };
                if load.id != load_id {
                    break;
                }
                let transaction = status
                    .as_ref()
                    .and_then(|status| next_load_transaction(load, status, &wallet));
                (transaction, load.config.target.clone(), load.config.tps)
            };

            if let Some(transaction) = transaction {
                let (target, auth_key) = {
                    let state = shared.lock().unwrap();
                    (resolve_target(&state, &target), state.auth_key)
                };
                if let Some((node_id, address)) = target {
                    match send_signed_transaction(&shared, address, transaction, &auth_key) {
                        Ok(()) => {
                            let mut state = shared.lock().unwrap();
                            if state.load.as_ref().is_some_and(|load| load.id == load_id) {
                                state.submitted += 1;
                                state.submit_times.push_back(Instant::now());
                            }
                        }
                        Err(error) => log(
                            &shared,
                            format!("Load submission to node {node_id} failed: {error}"),
                        ),
                    }
                }
            }

            let interval = Duration::from_secs_f64(1.0 / tps);
            if let Some(remaining) = interval.checked_sub(started.elapsed()) {
                thread::sleep(remaining);
            }
        }
    });
}

fn next_load_transaction(load: &mut LoadRuntime, status: &NodeStatus, wallet: &BTreeMap<u64, Address>) -> Option<Transaction> {
    let (first, count) = match load.config.preset {
        LoadPreset::BookTaker { account_id, .. } => (account_id, 1),
        LoadPreset::RoundRobinTransfers {
            first_account,
            account_count,
            ..
        }
        | LoadPreset::RandomTransfers {
            first_account,
            account_count,
            ..
        } => (first_account, account_count),
    };
    let mut eligible = Vec::new();
    for offset in 0..count {
        let account_id = first + offset;
        if load.failed.contains_key(&account_id) {
            continue;
        }
        if let Some(pending) = load.pending.get_mut(&account_id) {
            if pending.last_sent.elapsed() >= Duration::from_secs(3) {
                pending.last_sent = Instant::now();
                return Some(pending.transaction.clone());
            }
            continue;
        }
        if let Some(account) = status
            .account_nonces
            .iter()
            .find(|account| Some(account.account_id) == wallet.get(&account_id).copied())
        {
            eligible.push((account_id, account.nonce));
        }
    }
    if eligible.is_empty() {
        return None;
    }
    let (account_id, nonce) = match load.config.preset {
        LoadPreset::RandomTransfers { .. } => eligible[rand::rng().random_range(0..eligible.len())],
        _ => {
            let index = eligible
                .iter()
                .position(|(id, _)| *id >= first + load.cursor % count)
                .unwrap_or(0);
            let selected = eligible[index];
            load.cursor = (selected.0 - first + 1) % count;
            selected
        }
    };
    let transaction = match load.config.preset {
        LoadPreset::BookTaker { account_id, market_id, side } => {
            let book = status.order_books.iter().find(|book| book.market_id == market_id)?;
            let price = match side {
                light_dex::exchange::order::Side::Buy => book.asks.first()?.price,
                light_dex::exchange::order::Side::Sell => book.bids.first()?.price,
            };
            Transaction::PlaceOrder { account_id: *wallet.get(&account_id)?, market_id,
                order_type: light_dex::exchange::order::OrderType::IOC,
                side, price, quantity: 1, nonce }
        }
        LoadPreset::RoundRobinTransfers {
            first_account,
            account_count,
            asset,
            amount,
        } => {
            let to = first_account + (account_id - first_account + 1) % account_count;
            Transaction::Transfer {
                from: *wallet.get(&account_id)?,
                to: *wallet.get(&to)?,
                asset,
                amount,
                nonce,
            }
        }
        LoadPreset::RandomTransfers {
            first_account,
            account_count,
            asset,
            amount,
        } => {
            let offset = rand::rng().random_range(1..account_count);
            let to = first_account + (account_id - first_account + offset) % account_count;
            Transaction::Transfer {
                from: *wallet.get(&account_id)?,
                to: *wallet.get(&to)?,
                asset,
                amount,
                nonce,
            }
        }
    };
    let digest = transaction
        .to_digest()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    load.pending.insert(
        account_id,
        PendingLoadTransaction {
            transaction: transaction.clone(),
            digest,
            last_sent: Instant::now(),
        },
    );
    Some(transaction)
}

fn feed_variance(history: &VecDeque<(Instant, f64)>, window_seconds: u64) -> Option<f64> {
    let now = Instant::now();
    let values = history.iter().filter(|(at, _)| now.duration_since(*at) <= Duration::from_secs(window_seconds))
        .map(|(_, mid)| *mid).collect::<Vec<_>>();
    if values.len() < 10 { return None; }
    let changes = values.windows(2).map(|pair| pair[1] - pair[0]).collect::<Vec<_>>();
    let mean = changes.iter().sum::<f64>() / changes.len() as f64;
    Some(changes.iter().map(|change| (change - mean).powi(2)).sum::<f64>() / changes.len() as f64)
}

/// One Binance socket serves every fleet maker. No participant owns the feed.
fn spawn_shared_binance_feed(shared: Shared, run_id: u64) {
    thread::spawn(move || loop {
        if { let state = shared.lock().unwrap(); !state.running || state.run_id != run_id } { break; }
        match tungstenite::connect(STREAM_URL) {
            Ok((mut socket, _)) => {
                use tungstenite::stream::MaybeTlsStream;
                let timeout = Some(Duration::from_secs(2));
                match socket.get_mut() {
                    MaybeTlsStream::Plain(stream) => { let _ = stream.set_read_timeout(timeout); }
                    MaybeTlsStream::NativeTls(stream) => { let _ = stream.get_mut().set_read_timeout(timeout); }
                    _ => {}
                }
                loop {
                    if { let state = shared.lock().unwrap(); !state.running || state.run_id != run_id } { return; }
                    match socket.read() {
                        Ok(tungstenite::Message::Text(text)) => {
                            if let Ok(depth) = serde_json::from_str::<PartialDepth>(&text) {
                                if let Some(mid) = mid_price(&depth) {
                                    let mut state = shared.lock().unwrap();
                                    if state.feed_depth.as_ref().is_none_or(|old| depth.last_update_id >= old.last_update_id) {
                                        let now = Instant::now();
                                        if state.feed_history.back().is_none_or(|(at, _)| now.duration_since(*at) >= Duration::from_secs(1)) {
                                            state.feed_history.push_back((now, mid));
                                            while state.feed_history.len() > 601 { state.feed_history.pop_front(); }
                                        }
                                        state.feed_depth = Some(depth);
                                        state.feed_at = Some(now);
                                    }
                                }
                            }
                        }
                        Ok(tungstenite::Message::Ping(_)) => { let _ = socket.flush(); }
                        Ok(tungstenite::Message::Close(_)) => break,
                        Ok(_) => {},
                        Err(tungstenite::Error::Io(error)) if error.kind() == io::ErrorKind::WouldBlock
                            || error.kind() == io::ErrorKind::TimedOut => {},
                        Err(error) => { log(&shared, format!("Shared Binance feed lost: {error}")); break; }
                    }
                }
            }
            Err(error) => log(&shared, format!("Shared Binance feed unavailable: {error}")),
        }
        thread::sleep(Duration::from_secs(1));
    });
}

fn spawn_binance_feed(shared: Shared, id: u64) {
    thread::spawn(move || {
        loop {
            let active = shared.lock().unwrap().market_maker.as_ref().is_some_and(|mm| mm.id == id && mm.active);
            if !active { break; }
            match tungstenite::connect(STREAM_URL) {
                Ok((mut socket, _)) => {
                    use tungstenite::stream::MaybeTlsStream;
                    let timeout = Some(Duration::from_secs(2));
                    match socket.get_mut() {
                        MaybeTlsStream::Plain(stream) => { let _ = stream.set_read_timeout(timeout); }
                        MaybeTlsStream::NativeTls(stream) => { let _ = stream.get_mut().set_read_timeout(timeout); }
                        _ => {}
                    }
                    loop {
                        if !shared.lock().unwrap().market_maker.as_ref().is_some_and(|mm| mm.id == id && mm.active) { return; }
                        match socket.read() {
                            Ok(tungstenite::Message::Text(text)) => {
                                if let Ok(depth) = serde_json::from_str::<PartialDepth>(&text) {
                                    let mut state = shared.lock().unwrap();
                                    if let Some(mm) = state.market_maker.as_mut().filter(|mm| mm.id == id) {
                                        mm.feed_at = Some(Instant::now());
                                        if mm.latest_depth.as_ref().is_none_or(|old| depth.last_update_id >= old.last_update_id) {
                                            mm.latest_depth = Some(depth);
                                        }
                                    }
                                }
                            }
                            Ok(tungstenite::Message::Ping(_)) => { let _ = socket.flush(); }
                            Ok(tungstenite::Message::Close(_)) => break,
                            Ok(_) => {},
                            Err(tungstenite::Error::Io(error)) if error.kind() == io::ErrorKind::WouldBlock
                                || error.kind() == io::ErrorKind::TimedOut => {},
                            Err(error) => {
                                report_feed_error(&shared, id, format!("Binance depth connection lost: {error}"));
                                break;
                            }
                        }
                    }
                }
                Err(error) => report_feed_error(&shared, id, format!("Binance depth connection failed: {error}")),
            }
            thread::sleep(Duration::from_secs(1));
        }
    });
}

fn report_feed_error(shared: &Shared, id: u64, message: String) {
    let mut state = shared.lock().unwrap();
    if let Some(mm) = state.market_maker.as_mut().filter(|mm| mm.id == id) {
        mm.last_error = Some(message.clone());
    }
    state.log(message);
}

fn spawn_market_maker_loop(shared: Shared, id: u64) {
    thread::spawn(move || {
        loop {
            if !market_maker_step(&shared, id) { break; }
            thread::sleep(Duration::from_millis(100));
        }
    });
}

fn market_maker_step(shared: &Shared, id: u64) -> bool {
    let dispatch = {
        let mut state = shared.lock().unwrap();
        if !state.running { return false; }
        let Some(status) = best_status(&state).cloned() else { return true; };
        let auth_key = state.auth_key;
        let Some(account_address) = state.market_maker.as_ref().and_then(|mm| slot_address(&state, mm.config.account_id)) else { return true; };
        let Some(mm) = state.market_maker.as_mut().filter(|mm| mm.id == id) else { return false; };
        if !mm.active && !mm.stopping { return false; }
        let Some(nonce) = status.account_nonces.iter().find(|account| account.account_id == account_address).map(|account| account.nonce) else { return true; };
        let now = Instant::now();
        for pending in &mm.pending {
            if let Some(outcome) = status.recent_transaction_outcomes.iter().find(|outcome| outcome.digest == pending.digest) {
                if !outcome.success {
                    mm.last_error = Some(format!("Quote transaction failed at nonce {}: {}. Check outstanding quotes before restarting.", pending.transaction.nonce(), outcome.error.as_deref().unwrap_or("unknown error")));
                    mm.active = false;
                    mm.stopping = false;
                    mm.pending.clear();
                    return false;
                }
            }
        }
        while mm.pending.front().is_some_and(|pending| pending.transaction.nonce() < nonce) {
            mm.pending.pop_front();
        }
        let mut transactions = Vec::new();
        for pending in &mut mm.pending {
            if now.duration_since(pending.last_sent) >= Duration::from_secs(3) {
                pending.last_sent = now;
                transactions.push(pending.transaction.clone());
            }
        }
        if mm.active {
            let max_age = Duration::from_millis(mm.config.refresh_ms.saturating_mul(3).max(3_000));
            if mm.feed_at.is_some_and(|at| now.duration_since(at) <= max_age) {
                if mm.last_applied.is_none_or(|at| now.duration_since(at) >= Duration::from_millis(mm.config.refresh_ms)) {
                    if let Some(depth) = &mm.latest_depth { mm.desired = desired_book(depth, mm.config.levels); }
                    mm.last_applied = Some(now);
                    mm.last_error = None;
                }
            } else {
                mm.desired = DesiredBook::default();
                if mm.feed_at.is_some() { mm.last_error = Some("Binance depth feed is stale; quotes are being cancelled".into()); }
            }
        } else {
            mm.desired = DesiredBook::default();
        }
        let pending_actions = mm.pending.iter().map(|pending| pending.action).collect::<Vec<_>>();
        let available = 20usize.saturating_sub(mm.pending.len());
        let actions = plan_actions(account_address, &status.open_orders, &mm.desired, mm.active, &pending_actions);
        let next_nonce = mm.pending.back().map_or(nonce, |pending| pending.transaction.nonce().saturating_add(1)).max(nonce);
        for (index, action) in actions.into_iter().take(available).enumerate() {
            let tx_nonce = next_nonce + index as u64;
            let tx = match action {
                QuoteAction::Cancel { order_id } => Transaction::CancelOrder {
                    account_id: account_address, market_id: 0, order_id, nonce: tx_nonce,
                },
                QuoteAction::Place { side, price } => Transaction::PlaceOrder {
                    account_id: account_address, market_id: 0,
                    order_type: light_dex::exchange::order::OrderType::LO,
                    side, price, quantity: 1, nonce: tx_nonce,
                },
            };
            mm.pending.push_back(PendingQuote {
                action, digest: hex_encode(&tx.to_digest()), transaction: tx.clone(), last_sent: now,
            });
            transactions.push(tx);
        }
        if mm.stopping && mm.pending.is_empty() && transactions.is_empty() &&
            !status.open_orders.iter().any(|order| order.account_id == account_address && order.market_id == 0) {
            mm.stopping = false;
            state.log("Market maker stopped; all visible quotes cleared");
            return false;
        }
        (transactions, mm.config.target.clone(), auth_key)
    };
    let (transactions, target, auth_key) = dispatch;
    for tx in transactions {
        let target_address = { resolve_target(&shared.lock().unwrap(), &target) };
        let result = match target_address {
            Some((_, address)) => send_signed_transaction(&shared, address, tx, &auth_key),
            None => Err(io::Error::new(io::ErrorKind::NotConnected, "no available target node")),
        };
        let mut state = shared.lock().unwrap();
        if state.market_maker.as_ref().is_some_and(|mm| mm.id == id) {
            match result {
                Ok(()) => {
                    state.submitted += 1;
                    state.submit_times.push_back(Instant::now());
                }
                Err(error) => {
                    if let Some(mm) = state.market_maker.as_mut() {
                        mm.last_error = Some(format!("Quote submission failed; retrying: {error}"));
                    }
                }
            }
        }
    }
    true
}

fn snapshot(shared: &Shared) -> ControlSnapshot {
    let mut state = shared.lock().unwrap();
    let now = Instant::now();
    for node in state.nodes.values_mut() {
        if matches!(node.child.try_wait(), Ok(Some(_))) {
            node.state = "dead".into();
        }
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
        if node.state != "ok" {
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
    let market_maker = match state.market_maker.as_ref() {
        Some(mm) => MarketMakerSnapshot {
            running: mm.active, stopping: mm.stopping,
            account_id: Some(mm.config.account_id), refresh_ms: Some(mm.config.refresh_ms),
            last_update_id: mm.latest_depth.as_ref().map(|depth| depth.last_update_id),
            feed_age_ms: mm.feed_at.map(|at| now.duration_since(at).as_millis().min(u64::MAX as u128) as u64),
            desired_bids: mm.desired.bids.clone(), desired_asks: mm.desired.asks.clone(),
            own_open_orders: best_status(&state).map_or(0, |status| status.open_orders.iter()
                .filter(|order| Some(order.account_id) == slot_address(&state, mm.config.account_id) && order.market_id == 0).count()),
            pending: !mm.pending.is_empty(), last_error: mm.last_error.clone(),
        },
        None => MarketMakerSnapshot {
            running: false, stopping: false, account_id: None, refresh_ms: None,
            last_update_id: None, feed_age_ms: None,
            desired_bids: vec![], desired_asks: vec![], own_open_orders: 0,
            pending: false, last_error: None,
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
            chain_consistency: if responsive == 0 {
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
            kind: state.load.as_ref().map(|load| match load.config.preset {
                LoadPreset::RandomTransfers { .. } => "random transfers",
                LoadPreset::RoundRobinTransfers { .. } => "rotating transfers",
                LoadPreset::BookTaker { .. } => "book taker",
            }.to_string()),
            tps: state.load.as_ref().map(|load| load.config.tps),
            pending_accounts: state.load.as_ref().map_or(0, |load| load.pending.len()),
            failed_accounts: state.load.as_ref().map_or_else(Vec::new, |load| {
                let mut accounts = load.failed.keys().copied().collect::<Vec<_>>();
                accounts.sort_unstable();
                accounts
            }),
        },
        market_maker,
        agents: state.agents.values().map(|agent| AgentSnapshot {
            agent_id: agent.config.agent_id,
            account_id: agent.config.account_id,
            strategy: agent.config.strategy.clone(),
            target: agent.config.target.clone(),
            state: if agent.stopping { "stopping" } else if agent.active { "active" } else { "stopped" }.into(),
            pending: usize::from(agent.pending.is_some()),
            pending_action: agent.pending.as_ref().and_then(|pending| pending.action.map(|action| format!("{action:?}"))),
            own_open_orders: best_status(&state).map_or(0, |status| status.open_orders.iter()
                .filter(|order| Some(order.account_id) == slot_address(&state, agent.config.account_id) && order.market_id == agent.config.market_id).count()),
            last_error: agent.last_error.clone(),
            confirmed_nonce: best_status(&state).and_then(|status| status.account_nonces.iter()
                .find(|account| Some(account.account_id) == slot_address(&state, agent.config.account_id)).map(|account| account.nonce)),
            balance: best_status(&state).and_then(|status| status.agent_balances.iter()
                .find(|balance| Some(balance.account_id) == slot_address(&state, agent.config.account_id)).cloned()),
            feed_age_ms: agent.config.strategy.needs_binance().then(|| state.feed_at
                .map(|at| now.duration_since(at).as_millis().min(u64::MAX as u128) as u64)).flatten(),
            group_id: agent.group_id,
        }).collect(),
        wallet: state.wallet.iter().map(|(&label, &address)| WalletAccount { label, address }).collect(),
        generated_wallets: state.wallet.iter()
            .filter(|(_, address)| state.signing_manifest.as_ref().is_some_and(|manifest| !manifest.accounts.contains(address)))
            .map(|(&label, &address)| WalletAccount { label, address }).collect(),
        claim_progress: state.claim_progress.clone(),
        logs: state.logs.iter().cloned().collect(),
        active_data_dir: state.active_data_dir.clone(),
    }
}

fn node_snapshot(id: NodeId, node: &RuntimeNode) -> NodeRuntimeSnapshot {
    NodeRuntimeSnapshot {
        node_id: id,
        pid: node.child.id(),
        state: node.state.clone(),
        address: node.address.map(|address| address.to_string()),
        tx_rate: node.tx_rate,
        block_rate: node.block_rate,
        status: node.status.clone(),
        last_status_at_ms: node.last_status_at_ms,
    }
}

fn median(mut values: Vec<f64>) -> f64 {
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

fn stop_cluster(shared: Shared, force: bool) {
    let (addresses, auth_key) = {
        let mut state = shared.lock().unwrap();
        if force && state.stopping { return; }
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
        (addresses, state.auth_key)
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
        thread::sleep(Duration::from_millis(250));
    }
    let mut state = shared.lock().unwrap();
    for node in state.nodes.values_mut() {
        if !matches!(node.child.try_wait(), Ok(Some(_))) {
            let _ = node.child.kill();
            let _ = node.child.wait();
        }
        node.state = "dead".into();
    }
    state.account_keys.clear();
    state.pending_airdrops.clear();
    state.signing_manifest = None;
    state.auth_key = [0; 32];
    state.stopping = false;
    state.log("Simulation stopped");
}

fn resolve_target(state: &ControllerInner, target: &TargetNode) -> Option<(NodeId, SocketAddr)> {
    let id = resolve_target_id(state, target)?;
    Some((id, state.nodes.get(&id)?.address?))
}
fn resolve_target_id(state: &ControllerInner, target: &TargetNode) -> Option<NodeId> {
    let alive = state
        .nodes
        .iter()
        .filter(|(_, node)| node.state != "dead" && node.address.is_some())
        .map(|(id, _)| *id)
        .collect::<Vec<_>>();
    if alive.is_empty() {
        return None;
    }
    match target {
        TargetNode::Node { node_id } => alive.contains(node_id).then_some(*node_id),
        TargetNode::Random => Some(alive[rand::rng().random_range(0..alive.len())]),
        TargetNode::Primary => {
            let mut votes = HashMap::new();
            for node in state.nodes.values() {
                if node.state == "ok" {
                    if let Some(status) = &node.status {
                        *votes.entry(status.current_primary).or_insert(0usize) += 1;
                    }
                }
            }
            votes
                .into_iter()
                .max_by_key(|(_, count)| *count)
                .map(|(id, _)| id)
                .filter(|id| alive.contains(id))
        }
    }
}

fn send_signed_transaction(shared: &Shared, address: SocketAddr, transaction: Transaction, auth_key: &[u8; 32]) -> io::Result<()> {
    let signed = {
        let state = shared.lock().map_err(|_| io::Error::other("controller lock poisoned"))?;
        let manifest = state.signing_manifest.as_ref().ok_or_else(|| io::Error::other("no active signing session"))?;
        let slot = state.wallet.iter().find_map(|(slot, account)| (*account == transaction.account_id()).then_some(*slot))
            .ok_or_else(|| io::Error::other("sender address is not in the controller wallet"))?;
        let key = state.account_keys.get(&slot)
            .ok_or_else(|| io::Error::other("account has no signing key"))?;
        sign_transaction(transaction, manifest, key)?
    };
    send_message(address, &Message::SignedTransaction(signed), Duration::from_millis(500), auth_key)
}

fn send_message(address: SocketAddr, message: &Message, timeout: Duration, auth_key: &[u8; 32]) -> io::Result<()> {
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_write_timeout(Some(timeout))?;
    let data = sign_line(message, auth_key)?;
    stream.write_all(data.as_bytes())
}
fn request_status(address: SocketAddr, auth_key: &[u8; 32], timeout: Duration) -> io::Result<NodeStatus> {
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let request = sign_line(&Message::Control(ControlMessage::Status), auth_key)?;
    stream.write_all(request.as_bytes())?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    verify_line(response.trim(), auth_key)
}
fn read_line(stream: &TcpStream) -> io::Result<String> {
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    Ok(line)
}
fn spawn_output_reader<R: Read + Send + 'static>(node_id: NodeId, reader: R, shared: Shared) {
    thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(line) => log(&shared, format!("[node {node_id}] {line}")),
                Err(_) => break,
            }
        }
    });
}
fn log(shared: &Shared, message: impl Into<String>) {
    shared.lock().unwrap().log(message);
}

#[cfg(test)]
mod tests {
    use super::*;
    use light_dex::node::{AccountNonce, TransactionOutcome};

    #[test]
    fn polling_mode_selects_all_or_rotates_one_node() {
        let targets = (0..3)
            .map(|id| (id, SocketAddr::from(([127, 0, 0, 1], 9000 + id as u16))))
            .collect::<Vec<_>>();
        let mut next = 0;
        assert_eq!(select_poll_targets(targets.clone(), crate::types::PollMode::All, &mut next), targets);
        assert_eq!(next, 0);
        for expected in [0, 1, 2, 0] {
            let selected = select_poll_targets(targets.clone(), crate::types::PollMode::RoundRobin, &mut next);
            assert_eq!(selected.len(), 1);
            assert_eq!(selected[0].0, expected);
        }
        assert!(select_poll_targets(Vec::new(), crate::types::PollMode::RoundRobin, &mut next).is_empty());
    }

    #[test]
    fn benchmark_future_nonce_window_does_not_change_ui_rule() {
        assert!(nonce_in_window(5, 5, 0));
        assert!(!nonce_in_window(5, 6, 0));
        assert!(nonce_in_window(5, 6, 32));
        assert!(nonce_in_window(5, 37, 32));
        assert!(!nonce_in_window(5, 38, 32));
        assert!(!nonce_in_window(5, 4, 32));
    }

    fn status() -> NodeStatus {
        NodeStatus {
            node_id: 0,
            pid: 0,
            chain_id: String::new(),
            signing_enabled: false,
            authentication_rejections: 0,
            faucet_remaining_claims: None,
            network_ready: true,
            peer_count: 0,
            expected_peers: 0,
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
            transactions_failed: 0,
            trades_committed: 0,
            average_commit_latency_ms: 0.0,
            last_commit_latency_ms: None,
            recent_commit_latency_ms: vec![],
            catch_up_events: 0,
            blocks_caught_up: 0,
            recent_blocks: vec![],
            account_nonces: vec![
                AccountNonce {
                    account_id: 0.into(),
                    nonce: 0,
                },
                AccountNonce {
                    account_id: 1.into(),
                    nonce: 0,
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
        let first = next_load_transaction(state.load.as_mut().unwrap(), &status(), &state.wallet).unwrap();
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
        let second = next_load_transaction(state.load.as_mut().unwrap(), &after_failure, &state.wallet).unwrap();
        assert_eq!(second.account_id(), 1.into());
        assert_eq!(second.nonce(), 0);
    }

    #[test]
    fn taker_targets_best_opposite_price() {
        let mut status = status();
        status.order_books.push(light_dex::exchange::orderbook::OrderBookSnapshot {
            market_id: 0,
            bids: vec![light_dex::exchange::orderbook::BookLevel { price: 98, quantity: 2, order_count: 1 }],
            asks: vec![light_dex::exchange::orderbook::BookLevel { price: 102, quantity: 2, order_count: 1 }],
        });
        let mut load = LoadRuntime {
            id: 1,
            config: LoadConfig { preset: LoadPreset::BookTaker {
                account_id: 1, market_id: 0, side: light_dex::exchange::order::Side::Buy,
            }, target: TargetNode::Random, tps: 1.0 },
            cursor: 0, pending: HashMap::new(), failed: HashMap::new(),
        };
        let wallet = BTreeMap::from([(1, 1.into())]);
        assert!(matches!(next_load_transaction(&mut load, &status, &wallet), Some(Transaction::PlaceOrder {
            account_id, market_id: 0, price: 102, quantity: 1, nonce: 0,
            order_type: light_dex::exchange::order::OrderType::IOC, ..
        }) if account_id == Address::fixture(1)));
    }

    #[test]
    #[ignore = "spawns four node processes and requires a built light_dex binary"]
    fn fresh_signed_cluster_claims_accounts_and_replays() {
        let project = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let base_data = std::env::temp_dir().join(format!("light-dex-claim-smoke-{}", rand::random::<u64>()));
        let controller = ControllerState::default();
        controller.launch(LaunchConfig {
            project_dir: project.display().to_string(), binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(), recover_existing: false,
            node_count: 4, block_time_ms: 150, timeout_ms: 1_500, poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false, consensus: crate::types::ConsensusAlgorithm::Pbft,
            claim_limit: 3, claim_usd: 100, claim_btc: 2,
        }).unwrap();
        wait_for(&controller, |snapshot| snapshot.running && snapshot.metrics.responsive_nodes == 4 && snapshot.metrics.height_min == Some(1));
        let initial = controller.snapshot();
        assert!(initial.wallet.is_empty());
        assert!(initial.nodes.iter().filter_map(|node| node.status.as_ref())
            .all(|status| status.account_nonces.is_empty() && status.faucet_remaining_claims == Some(3)));
        let wallets = controller.claim_new_accounts(2).unwrap();
        assert_eq!(wallets.iter().map(|wallet| wallet.label).collect::<Vec<_>>(), vec![0, 1]);
        let claimed = wait_for(&controller, |snapshot| snapshot.metrics.height_min.unwrap_or_default() >= 2
            && snapshot.nodes.iter().filter_map(|node| node.status.as_ref()).all(|status|
                wallets.iter().all(|wallet| status.account_nonces.iter().any(|account| account.account_id == wallet.address && account.nonce == 1))));
        assert_eq!(claimed.nodes.iter().filter_map(|node| node.status.as_ref()).next().unwrap().faucet_remaining_claims, Some(1));
        assert!(controller.claim_new_accounts(2).is_err());
        controller.submit_transaction(Transaction::Transfer {
            from: wallets[0].address, to: wallets[1].address, asset: 0, amount: 10, nonce: 1,
        }, TargetNode::Primary).unwrap();
        let transferred = wait_for(&controller, |snapshot| snapshot.nodes.iter().filter_map(|node| node.status.as_ref())
            .any(|status| status.account_nonces.iter().any(|account| account.account_id == wallets[0].address && account.nonce == 2)));
        let session_dir = PathBuf::from(transferred.active_data_dir.as_ref().unwrap());
        controller.stop_all();
        wait_for(&controller, |snapshot| !snapshot.running && !snapshot.launching && !snapshot.stopping && snapshot.metrics.alive_nodes == 0);
        let manifest = Arc::new(SessionManifest::load(&session_dir.join("manifest.json")).unwrap());
        let replayed = light_dex::blockchain::BlockchainState::load_signed(session_dir.join("node-0.json"), manifest).unwrap();
        assert_eq!(replayed.exchange_state.accounts()[&wallets[0].address].nonce, 2);
        assert_eq!(replayed.exchange_state.accounts()[&wallets[0].address].balances[&0].available, 90);
        assert_eq!(replayed.exchange_state.accounts()[&wallets[1].address].balances[&0].available, 110);
        std::fs::remove_dir_all(base_data).unwrap();
    }

    #[test]
    #[ignore = "spawns four node processes and requires a built light_dex binary"]
    fn transfer_from_claimed_wallet_creates_unfunded_recipient() {
        let project = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let base_data = std::env::temp_dir().join(format!("light-dex-airdrop-{}", rand::random::<u64>()));
        let controller = ControllerState::default();
        controller.launch(LaunchConfig {
            project_dir: project.display().to_string(), binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(), recover_existing: false,
            node_count: 4, block_time_ms: 150, timeout_ms: 1_500, poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false, consensus: crate::types::ConsensusAlgorithm::Pbft,
            claim_limit: 1_000, claim_usd: 1_000_000, claim_btc: 1_000,
        }).unwrap();
        wait_for(&controller, |snapshot| snapshot.running && snapshot.metrics.responsive_nodes == 4 && snapshot.metrics.height_min == Some(1));
        let funded = controller.claim_new_accounts(1).unwrap().remove(0);
        let unfunded = controller.create_address().unwrap();
        assert_eq!(funded.label, 0);
        assert_eq!(unfunded.label, 1);
        controller.submit_transaction(Transaction::Transfer {
            from: funded.address, to: unfunded.address, asset: 0, amount: 5, nonce: 1,
        }, TargetNode::Primary).unwrap();
        let committed = wait_for(&controller, |snapshot| snapshot.metrics.height_min.unwrap_or_default() >= 3
            && snapshot.nodes.iter().filter(|node| node.state == "ok").filter_map(|node| node.status.as_ref())
                .all(|status| status.agent_balances.iter().any(|balance| balance.account_id == funded.address && balance.usd_available == 999_995)
                    && status.agent_balances.iter().any(|balance| balance.account_id == unfunded.address && balance.usd_available == 5)
                    && status.account_nonces.iter().any(|account| account.account_id == funded.address && account.nonce == 2)));
        let session_dir = PathBuf::from(committed.active_data_dir.as_ref().unwrap());
        assert!(session_dir.join("keys/account-0.key").exists());
        assert_eq!(committed.wallet.iter().find(|entry| entry.label == 0).unwrap().address, funded.address);
        controller.stop_all();
        wait_for(&controller, |snapshot| !snapshot.running && !snapshot.launching && !snapshot.stopping);
        let manifest = Arc::new(SessionManifest::load(&session_dir.join("manifest.json")).unwrap());
        let replayed = light_dex::blockchain::BlockchainState::load_signed(session_dir.join("node-0.json"), manifest).unwrap();
        assert_eq!(replayed.exchange_state.accounts()[&funded.address].balances[&0].available, 999_995);
        assert_eq!(replayed.exchange_state.accounts()[&unfunded.address].balances[&0].available, 5);
        std::fs::remove_dir_all(base_data).unwrap();
    }

    #[test]
    #[ignore = "spawns four node processes and requires a built light_dex binary"]
    fn cluster_commits_order_book_and_replays_signed_archive() {
        let project = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let base_data = std::env::temp_dir().join(format!("light-dex-smoke-{}", rand::random::<u64>()));
        let config = LaunchConfig {
            project_dir: project.display().to_string(),
            binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(),
            recover_existing: false,
            node_count: 4,
            block_time_ms: 100,
            timeout_ms: 800,
            poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus: crate::types::ConsensusAlgorithm::Pbft,
            claim_limit: 1_000, claim_usd: 1_000_000, claim_btc: 1_000,
        };
        let controller = ControllerState::default();
        controller.launch(config.clone()).unwrap();
        let ready = wait_for(&controller, |snapshot| snapshot.running && snapshot.metrics.responsive_nodes == 4 && snapshot.metrics.height_min == Some(1));
        let queried = controller.refresh_node_status(1).unwrap();
        assert_eq!(queried.node_id, 1);
        assert!(queried.last_status_at_ms.is_some());
        assert!(queried.status.is_some());
        let data_dir = ready.active_data_dir.clone().unwrap();
        controller.claim_new_accounts(3).unwrap();
        controller.submit_transaction(Transaction::PlaceOrder {
            account_id: wallet_addr(&controller, 0), market_id: 0, order_type: light_dex::exchange::order::OrderType::LO,
            side: light_dex::exchange::order::Side::Sell, price: 100_000, quantity: 1, nonce: 1,
        }, TargetNode::Random).unwrap();
        let committed = wait_for(&controller, |snapshot| snapshot.metrics.height_min.unwrap_or_default() >= 2
            && snapshot.nodes.iter().filter_map(|node| node.status.as_ref())
                .any(|status| status.order_books.iter().any(|book| book.market_id == 0 && book.asks.first().is_some_and(|level| level.price == 100_000 && level.quantity == 1))));
        assert_eq!(committed.metrics.chain_consistency, "consistent");
        controller.kill_node(TargetNode::Node { node_id: 0 }).unwrap();
        assert!(controller.refresh_node_status(0).is_err());
        controller.submit_transaction(Transaction::Transfer {
            from: wallet_addr(&controller, 1), to: wallet_addr(&controller, 2), asset: 0, amount: 5, nonce: 1,
        }, TargetNode::Node { node_id: 2 }).unwrap();
        let after_failover = wait_for(&controller, |snapshot| snapshot.metrics.responsive_nodes == 3
            && snapshot.metrics.views.contains(&1)
            && snapshot.metrics.height_min.unwrap_or_default() >= 3
            && snapshot.nodes.iter().filter(|node| node.state == "ok").filter_map(|node| node.status.as_ref())
                .all(|status| status.account_nonces.iter().any(|account| account.account_id == wallet_addr(&controller, 1) && account.nonce == 2)));
        controller.stop_all();
        wait_for(&controller, |snapshot| !snapshot.running && !snapshot.launching && !snapshot.stopping && snapshot.metrics.alive_nodes == 0);
        assert_eq!(after_failover.metrics.chain_consistency, "consistent");
        let manifest = Arc::new(SessionManifest::load(&PathBuf::from(&data_dir).join("manifest.json")).unwrap());
        let replayed = light_dex::blockchain::BlockchainState::load_signed(
            PathBuf::from(&data_dir).join("node-1.json"), manifest).unwrap();
        assert!(replayed.blocks.len() >= 3);
        assert!(replayed.exchange_state.order_books().get(&0).is_some());
        let mut recovery = config;
        recovery.data_dir = data_dir;
        recovery.recover_existing = true;
        controller.launch(recovery).unwrap();
        wait_for(&controller, |snapshot| snapshot.logs.iter().any(|line| line.contains("live recovery is unavailable")));
        std::fs::remove_dir_all(base_data).unwrap();
    }

    #[test]
    #[ignore = "spawns node processes and requires a built light_dex binary"]
    fn five_tps_across_accounts_can_batch_transactions() {
        let project = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let base_data = std::env::temp_dir().join(format!("light-dex-batch-{}", rand::random::<u64>()));
        let controller = ControllerState::default();
        controller.launch(LaunchConfig {
            project_dir: project.display().to_string(), binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(), recover_existing: false,
            node_count: 4, block_time_ms: 500, timeout_ms: 1500, poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus: crate::types::ConsensusAlgorithm::Pbft,
            claim_limit: 1_000, claim_usd: 1_000_000, claim_btc: 1_000,
        }).unwrap();
        wait_for(&controller, |snapshot| snapshot.running && snapshot.metrics.responsive_nodes == 4 && snapshot.metrics.height_min == Some(1));
        controller.claim_new_accounts(10).unwrap();
        controller.start_load(LoadConfig {
            preset: LoadPreset::RoundRobinTransfers { first_account: 0, account_count: 10, asset: 0, amount: 1 },
            target: TargetNode::Node { node_id: 0 }, tps: 5.0,
        }).unwrap();
        let observed = wait_for(&controller, |snapshot| snapshot.nodes.iter().filter(|node| node.state == "ok")
            .filter_map(|node| node.status.as_ref()).any(|status| status.recent_blocks.iter().any(|block| block.transaction_count >= 2)));
        controller.stop_all();
        wait_for(&controller, |snapshot| !snapshot.running && !snapshot.launching && !snapshot.stopping && snapshot.metrics.alive_nodes == 0);
        assert!(observed.metrics.submitted >= 2);
        std::fs::remove_dir_all(base_data).unwrap();
    }

    #[test]
    #[ignore = "spawns node processes and requires a built light_dex binary"]
    fn invalid_account_signature_is_rejected_before_mempool() {
        let project = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let base_data = std::env::temp_dir().join(format!("light-dex-invalid-signature-{}", rand::random::<u64>()));
        let controller = ControllerState::default();
        controller.launch(LaunchConfig {
            project_dir: project.display().to_string(), binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(), recover_existing: false,
            node_count: 4, block_time_ms: 200, timeout_ms: 1_500, poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false, consensus: crate::types::ConsensusAlgorithm::Pbft,
            claim_limit: 1_000, claim_usd: 1_000_000, claim_btc: 1_000,
        }).unwrap();
        wait_for(&controller, |snapshot| snapshot.running && snapshot.metrics.height_min == Some(1)
            && snapshot.metrics.responsive_nodes == 4);
        controller.claim_new_accounts(2).unwrap();
        let (address, auth_key, mut signed) = {
            let state = controller.inner.lock().unwrap();
            let transaction = Transaction::Transfer { from: state.wallet[&0], to: state.wallet[&1], asset: 0, amount: 1, nonce: 1 };
            let signed = sign_transaction(transaction, state.signing_manifest.as_ref().unwrap(),
                state.account_keys.get(&0).unwrap()).unwrap();
            (state.nodes[&0].address.unwrap(), state.auth_key, signed)
        };
        let changed = if &signed.signature[..2] == "ff" { "fe" } else { "ff" };
        signed.signature.replace_range(..2, changed);
        send_message(address, &Message::SignedTransaction(signed), Duration::from_millis(500), &auth_key).unwrap();
        let observed = wait_for(&controller, |snapshot| snapshot.nodes.iter().filter_map(|node| node.status.as_ref())
            .any(|status| status.node_id == 0 && status.authentication_rejections > 0));
        assert!(observed.metrics.height_min >= Some(2));
        assert!(observed.nodes.iter().filter_map(|node| node.status.as_ref()).all(|status|
            status.account_nonces.iter().any(|nonce| nonce.account_id == wallet_addr(&controller, 0) && nonce.nonce == 1)));
        controller.stop_all();
        wait_for(&controller, |snapshot| !snapshot.running && !snapshot.launching && !snapshot.stopping);
        std::fs::remove_dir_all(base_data).unwrap();
    }

    #[test]
    #[ignore = "spawns seven node processes and requires a built light_dex binary"]
    fn seven_nodes_batch_quotes_and_commit_after_primary_kill() {
        use light_dex::exchange::order::{OrderType, Side};
        let project = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let base_data = std::env::temp_dir().join(format!("light-dex-failover-{}", rand::random::<u64>()));
        let controller = ControllerState::default();
        controller.launch(LaunchConfig {
            project_dir: project.display().to_string(), binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(), recover_existing: false,
            node_count: 7, block_time_ms: 500, timeout_ms: 1200, poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus: crate::types::ConsensusAlgorithm::Pbft,
            claim_limit: 1_000, claim_usd: 1_000_000, claim_btc: 1_000,
        }).unwrap();
        wait_for(&controller, |s| s.running && s.metrics.responsive_nodes == 7 && s.metrics.height_min == Some(1));
        controller.claim_new_accounts(1).unwrap();
        let (primary_address, auth_key) = {
            let state = controller.inner.lock().unwrap();
            (state.nodes[&0].address.unwrap(), state.auth_key)
        };
        for (nonce, side, price) in [(1, Side::Buy, 100_000), (2, Side::Sell, 100_010),
            (3, Side::Buy, 99_999), (4, Side::Sell, 100_011)] {
            send_signed_transaction(&controller.inner, primary_address, Transaction::PlaceOrder {
                account_id: wallet_addr(&controller, 0), market_id: 0, order_type: OrderType::LO, side,
                price, quantity: 1, nonce,
            }, &auth_key).unwrap();
        }
        let batched = wait_for(&controller, |s| s.metrics.height_min.unwrap_or_default() >= 2 &&
            s.nodes.iter().filter_map(|n| n.status.as_ref()).any(|status|
            status.recent_blocks.iter().any(|b| b.transaction_count >= 2)) &&
            s.nodes.iter().filter_map(|n| n.status.as_ref()).any(|status|
                status.account_nonces.iter().any(|a| a.account_id == wallet_addr(&controller, 0) && a.nonce == 5)));
        assert!(batched.metrics.height_min.unwrap_or_default() >= 2);
        controller.kill_node(TargetNode::Node { node_id: 0 }).unwrap();
        wait_for(&controller, |s| s.metrics.responsive_nodes == 6 &&
            s.nodes.iter().filter(|n| n.state == "ok").filter_map(|n| n.status.as_ref())
                .all(|status| status.current_view >= 1 && status.current_primary == 1));
        // Stale traffic must not occupy the successor's mempool or prevent
        // the next valid nonce from being proposed.
        for _ in 0..4 {
            let follower_address = controller.inner.lock().unwrap().nodes[&2].address.unwrap();
            send_signed_transaction(&controller.inner, follower_address, Transaction::PlaceOrder {
                account_id: wallet_addr(&controller, 0), market_id: 0, order_type: OrderType::LO,
                side: Side::Buy, price: 100_000, quantity: 1, nonce: 0,
            }, &auth_key).unwrap();
        }
        controller.submit_transaction(Transaction::PlaceOrder {
            account_id: wallet_addr(&controller, 0), market_id: 0, order_type: OrderType::LO,
            side: Side::Buy, price: 99_998, quantity: 1, nonce: 5,
        }, TargetNode::Node { node_id: 2 }).unwrap();
        let recovered = wait_for(&controller, |s| s.metrics.responsive_nodes == 6 &&
            s.nodes.iter().filter(|n| n.state == "ok").filter_map(|n| n.status.as_ref()).all(|status|
                status.current_view >= 1 && status.account_nonces.iter().any(|a| a.account_id == wallet_addr(&controller, 0) && a.nonce == 6)));
        assert_eq!(recovered.metrics.chain_consistency, "consistent");
        controller.stop_all();
        wait_for(&controller, |s| !s.running && !s.launching && !s.stopping && s.metrics.alive_nodes == 0);
        std::fs::remove_dir_all(base_data).unwrap();
    }

    #[test]
    #[ignore = "spawns four node processes and requires a built light_dex binary"]
    fn market_maker_sends_both_sides_in_one_block() {
        let project = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let base_data = std::env::temp_dir().join(format!("light-dex-maker-batch-{}", rand::random::<u64>()));
        let controller = ControllerState::default();
        controller.launch(LaunchConfig {
            project_dir: project.display().to_string(), binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(), recover_existing: false,
            node_count: 4, block_time_ms: 500, timeout_ms: 1500, poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus: crate::types::ConsensusAlgorithm::Pbft,
            claim_limit: 1_000, claim_usd: 1_000_000, claim_btc: 1_000,
        }).unwrap();
        wait_for(&controller, |s| s.running && s.metrics.responsive_nodes == 4 && s.metrics.height_min == Some(1));
        controller.claim_new_accounts(1).unwrap();
        let id = 1;
        {
            let mut state = controller.inner.lock().unwrap();
            state.market_maker = Some(MarketMakerRuntime {
                id, config: MarketMakerConfig { account_id: 0, levels: 2, refresh_ms: 1_000,
                    target: TargetNode::Node { node_id: 0 } },
                active: true, stopping: false, latest_depth: Some(PartialDepth {
                    last_update_id: 1,
                    bids: vec![["100000".into(), "1".into()], ["99999".into(), "1".into()]],
                    asks: vec![["100010".into(), "1".into()], ["100011".into(), "1".into()]],
                }), feed_at: Some(Instant::now()), desired: DesiredBook::default(),
                last_applied: None, pending: VecDeque::new(), last_error: None,
            });
        }
        assert!(market_maker_step(&controller.inner, id));
        let committed = wait_for(&controller, |s| s.nodes.iter().filter_map(|n| n.status.as_ref()).any(|status|
            status.recent_blocks.iter().any(|b| b.transaction_count >= 4) &&
            status.account_nonces.iter().any(|a| a.account_id == wallet_addr(&controller, 0) && a.nonce == 5)));
        assert_eq!(committed.market_maker.own_open_orders, 4);
        controller.stop_all();
        wait_for(&controller, |s| !s.running && !s.launching && !s.stopping && s.metrics.alive_nodes == 0);
        std::fs::remove_dir_all(base_data).unwrap();
    }

    #[test]
    #[ignore = "spawns node processes and requires a built light_dex binary"]
    fn hotstuff_commits_and_changes_leader_after_primary_kill() {
        let project = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let base_data = std::env::temp_dir().join(format!("light-dex-hotstuff-{}", rand::random::<u64>()));
        let controller = ControllerState::default();
        controller.launch(LaunchConfig {
            project_dir: project.display().to_string(),
            binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(),
            recover_existing: false,
            node_count: 4,
            block_time_ms: 200,
            timeout_ms: 1500,
            poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus: crate::types::ConsensusAlgorithm::Hotstuff,
            claim_limit: 1_000, claim_usd: 1_000_000, claim_btc: 1_000,
        }).unwrap();
        wait_for(&controller, |s| s.running && s.metrics.responsive_nodes == 4 && s.metrics.height_min == Some(1));
        controller.claim_new_accounts(3).unwrap();
        controller.submit_transaction(Transaction::Transfer {
            from: wallet_addr(&controller, 0), to: wallet_addr(&controller, 1), asset: 0, amount: 1, nonce: 1,
        }, TargetNode::Random).unwrap();
        wait_for(&controller, |s| s.nodes.iter().filter_map(|node| node.status.as_ref()).all(|status|
            status.account_nonces.iter().any(|account| account.account_id == wallet_addr(&controller, 0) && account.nonce == 2)));
        controller.kill_node(TargetNode::Node { node_id: 0 }).unwrap();
        controller.submit_transaction(Transaction::Transfer {
            from: wallet_addr(&controller, 1), to: wallet_addr(&controller, 2), asset: 0, amount: 1, nonce: 1,
        }, TargetNode::Node { node_id: 2 }).unwrap();
        let recovered = wait_for(&controller, |s| s.metrics.responsive_nodes == 3
            && s.nodes.iter().filter(|node| node.state == "ok").filter_map(|node| node.status.as_ref())
                .all(|status| status.chain_height >= 4 && status.current_view > 0));
        assert_eq!(recovered.metrics.chain_consistency, "consistent");
        controller.stop_all();
        wait_for(&controller, |s| !s.running && !s.launching && !s.stopping && s.metrics.alive_nodes == 0);
        std::fs::remove_dir_all(base_data).unwrap();
    }

    #[test]
    #[ignore = "spawns seven node processes and requires a built light_dex binary"]
    fn hotstuff_two_paused_replicas_rejoin() {
        let project = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let base_data = std::env::temp_dir().join(format!("light-dex-hotstuff-sync-{}", rand::random::<u64>()));
        let controller = ControllerState::default();
        controller.launch(LaunchConfig {
            project_dir: project.display().to_string(), binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(), recover_existing: false,
            node_count: 7, block_time_ms: 150, timeout_ms: 3000, poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false, consensus: crate::types::ConsensusAlgorithm::Hotstuff,
            claim_limit: 1_000, claim_usd: 1_000_000, claim_btc: 1_000,
        }).unwrap();
        wait_for(&controller, |s| s.running && s.metrics.responsive_nodes == 7 && s.metrics.height_min == Some(1));
        controller.claim_new_accounts(14).unwrap();
        for node_id in [5, 6] {
            controller.pause_node(TargetNode::Node { node_id }, 8_000).unwrap();
        }
        wait_for(&controller, |s| [5, 6].iter().all(|id| s.nodes.iter().any(|node|
            node.node_id == *id && node.state == "paused")));
        for account in 0..4 {
            controller.submit_transaction(Transaction::Transfer {
                from: wallet_addr(&controller, account), to: wallet_addr(&controller, account + 10), asset: 0, amount: 1, nonce: 1,
            }, TargetNode::Node { node_id: 0 }).unwrap();
            let height = account as usize + 2;
            wait_for(&controller, |s| s.nodes.iter().filter(|node| node.node_id < 5)
                .all(|node| node.status.as_ref().is_some_and(|status| status.chain_height >= height)));
        }
        let caught_up = wait_for(&controller, |s| s.metrics.responsive_nodes == 7
            && s.nodes.iter().all(|node| node.status.as_ref().is_some_and(|status| status.chain_height >= 5)));
        assert_eq!(caught_up.metrics.chain_consistency, "consistent");
        controller.stop_all();
        wait_for(&controller, |s| !s.running && !s.launching && !s.stopping && s.metrics.alive_nodes == 0);
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
                    if !snapshot.running && !snapshot.launching && !snapshot.stopping && snapshot.metrics.alive_nodes == 0 { break; }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
        let project = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let node_count = std::env::var("LIGHT_DEX_FLEET_TEST_NODES").ok().and_then(|value| value.parse().ok()).unwrap_or(7);
        let binary_path = std::env::var("LIGHT_DEX_FLEET_TEST_BINARY").unwrap_or_else(|_| "target/debug/light_dex".into());
        let base_data = std::env::temp_dir().join(format!("light-dex-hotstuff-fleet-{}", rand::random::<u64>()));
        let controller = ControllerState::default();
        let _guard = StopOnDrop(controller.clone());
        controller.launch(LaunchConfig {
            project_dir: project.display().to_string(), binary_path,
            data_dir: base_data.display().to_string(), recover_existing: false,
            node_count, block_time_ms: 500, timeout_ms: 1_500, poll_ms: 200,
            poll_mode: crate::types::PollMode::RoundRobin,
            build_before_launch: false, consensus: crate::types::ConsensusAlgorithm::Hotstuff,
            claim_limit: 1_000, claim_usd: 1_000_000, claim_btc: 1_000,
        }).unwrap();
        wait_for(&controller, |s| s.running && s.metrics.responsive_nodes == node_count as usize && s.metrics.height_min == Some(1));
        {
            let mut state = controller.inner.lock().unwrap();
            state.feed_started = true;
            state.feed_depth = Some(PartialDepth {
                last_update_id: 1,
                bids: vec![["100000".into(), "1".into()], ["99999".into(), "1".into()], ["99998".into(), "1".into()]],
                asks: vec![["100010".into(), "1".into()], ["100011".into(), "1".into()], ["100012".into(), "1".into()]],
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
            assert_eq!(snapshot.metrics.alive_nodes, node_count as usize, "logs: {:?}", snapshot.logs.iter().rev().take(20).collect::<Vec<_>>());
            assert_eq!(snapshot.agents.len(), 13);
            thread::sleep(Duration::from_millis(200));
        }
        let snapshot = controller.snapshot();
        eprintln!("HotStuff fleet under load: height {:?}-{:?}, responsive {}, submitted {}, committed {:?}",
            snapshot.metrics.height_min, snapshot.metrics.height_max, snapshot.metrics.responsive_nodes,
            snapshot.metrics.submitted, snapshot.nodes.iter().filter_map(|node| node.status.as_ref().map(|status| status.transactions_committed)).max());
        controller.stop_agents((1..=13).collect()).unwrap();
        let settled = wait_for(&controller, |s| s.agents.is_empty() && s.metrics.responsive_nodes == node_count as usize
            && s.metrics.height_min == s.metrics.height_max && s.metrics.chain_consistency == "consistent");
        eprintln!("HotStuff fleet settled: height {:?}, all {} nodes consistent", settled.metrics.height_min, settled.metrics.responsive_nodes);
        controller.stop_all();
        wait_for(&controller, |s| !s.running && !s.launching && !s.stopping && s.metrics.alive_nodes == 0);
        std::fs::remove_dir_all(base_data).unwrap();
        assert!(snapshot.metrics.submitted > 0);
        assert!(snapshot.metrics.height_min.unwrap_or_default() > 1);
        assert_eq!(snapshot.metrics.responsive_nodes, node_count as usize);
    }

    #[test]
    #[ignore = "requires live Binance feed and spawns node processes"]
    fn market_maker_quotes_live_depth_and_cancels_on_stop() {
        let project = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let base_data = std::env::temp_dir().join(format!("light-dex-maker-{}", rand::random::<u64>()));
        let controller = ControllerState::default();
        controller.launch(LaunchConfig {
            project_dir: project.display().to_string(), binary_path: "target/debug/light_dex".into(),
            data_dir: base_data.display().to_string(), recover_existing: false,
            node_count: 4, block_time_ms: 100, timeout_ms: 1500, poll_ms: 100,
            poll_mode: crate::types::PollMode::All,
            build_before_launch: false,
            consensus: crate::types::ConsensusAlgorithm::Pbft,
            claim_limit: 1_000, claim_usd: 1_000_000, claim_btc: 1_000,
        }).unwrap();
        wait_for(&controller, |snapshot| snapshot.running && snapshot.metrics.responsive_nodes == 4 && snapshot.metrics.height_min == Some(1));
        controller.claim_new_accounts(2).unwrap();
        controller.start_market_maker(MarketMakerConfig {
            account_id: 0, levels: 1, refresh_ms: 500, target: TargetNode::Node { node_id: 0 },
        }).unwrap();
        let quoted = wait_for(&controller, |snapshot| snapshot.market_maker.last_update_id.is_some()
            && snapshot.market_maker.own_open_orders >= 2);
        assert!(quoted.market_maker.desired_bids.len() == 1 && quoted.market_maker.desired_asks.len() == 1);
        controller.start_market_maker(MarketMakerConfig {
            account_id: 0, levels: 2, refresh_ms: 750, target: TargetNode::Node { node_id: 0 },
        }).unwrap();
        wait_for(&controller, |snapshot| snapshot.market_maker.desired_bids.len() == 2
            && snapshot.market_maker.desired_asks.len() == 2 && snapshot.market_maker.own_open_orders >= 4);
        controller.start_load(LoadConfig {
            preset: LoadPreset::BookTaker { account_id: 1, market_id: 0,
                side: light_dex::exchange::order::Side::Buy },
            target: TargetNode::Node { node_id: 0 }, tps: 1.0,
        }).unwrap();
        wait_for(&controller, |snapshot| snapshot.nodes.iter().filter(|node| node.state == "ok")
            .filter_map(|node| node.status.as_ref()).any(|status| status.account_nonces.iter()
                .any(|account| account.account_id == wallet_addr(&controller, 1) && account.nonce > 1)));
        controller.stop_load();
        controller.stop_market_maker();
        let cleared = wait_for(&controller, |snapshot| !snapshot.market_maker.running
            && !snapshot.market_maker.stopping && snapshot.market_maker.own_open_orders == 0);
        controller.stop_all();
        wait_for(&controller, |snapshot| !snapshot.running && !snapshot.launching && !snapshot.stopping && snapshot.metrics.alive_nodes == 0);
        assert!(cleared.metrics.height_min.unwrap_or_default() >= 3);
        std::fs::remove_dir_all(base_data).unwrap();
    }

    fn wallet_addr(controller: &ControllerState, label: u64) -> Address {
        controller.inner.lock().unwrap().wallet[&label]
    }

    fn wait_for(controller: &ControllerState, predicate: impl Fn(&ControlSnapshot) -> bool) -> ControlSnapshot {
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            let snapshot = controller.snapshot();
            if predicate(&snapshot) { return snapshot; }
            assert!(Instant::now() < deadline, "timed out waiting for cluster; logs: {:?}; statuses: {:?}", snapshot.logs.iter().rev().take(30).collect::<Vec<_>>(), snapshot.nodes.iter().map(|node| (node.node_id, node.state.clone(), node.status.as_ref().map(|status| (status.chain_height, status.current_view, status.account_nonces.iter().find(|account| Some(account.account_id) == snapshot.wallet.iter().find(|entry| entry.label == 1).map(|entry| entry.address)).map(|account| account.nonce), status.recent_transaction_outcomes.clone())))).collect::<Vec<_>>());
            thread::sleep(Duration::from_millis(100));
        }
    }
}
