use super::*;
impl ControllerState {
    pub fn start_agent(&self, config: AgentConfig) -> Result<(), String> {
        config.strategy.validate()?;
        let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
        if !state.running {
            return Err("start the cluster before an agent".into());
        }
        let status = best_status(&state)
            .cloned()
            .ok_or("wait for a responsive node")?;
        validate_agent_market(&config, &status)?;
        if !status
            .account_nonces
            .iter()
            .any(|account| Some(account.account_id) == slot_address(&state, config.account_id))
        {
            return Err("agent account does not exist".into());
        }
        let starting_new_keeper = !state.agents.contains_key(&config.agent_id)
            && matches!(config.strategy, AgentStrategy::LiquidationKeeper { .. });
        let started_agent_id = config.agent_id;
        if state.market_maker.as_ref().is_some_and(|mm| {
            (mm.active || mm.stopping) && mm.config.account_id == config.account_id
        }) || state
            .load
            .as_ref()
            .is_some_and(|load| load_uses_account(&load.config.preset, config.account_id))
            || slot_address(&state, config.account_id)
                .is_some_and(|address| state.pending_airdrops.contains_key(&address))
            || state.agents.values().any(|agent| {
                agent.config.agent_id != config.agent_id
                    && agent.config.account_id == config.account_id
            })
        {
            return Err("account already belongs to another participant".into());
        }
        if let Some(agent) = state.agents.get_mut(&config.agent_id) {
            if agent.config.account_id != config.account_id
                || agent.config.market_id != config.market_id
                || agent.stopping
            {
                return Err("stop and clear the old agent before changing its account".into());
            }
            if std::mem::discriminant(&agent.config.strategy)
                != std::mem::discriminant(&config.strategy)
            {
                return Err("stop and clear the agent before changing strategy presets".into());
            }
            agent.config = config;
            agent.last_error = None;
            agent.last_step = Instant::now() - Duration::from_secs(1);
            state.log("Participant settings updated");
        } else {
            if status
                .open_orders
                .iter()
                .any(|order| Some(order.account_id) == slot_address(&state, config.account_id))
            {
                return Err(
                    "agent account has existing open orders; choose a clean account".into(),
                );
            }
            state
                .agents
                .insert(config.agent_id, new_agent(config, None));
            state.log("Participant started");
        }
        let spawn_feed = state
            .agents
            .values()
            .any(|agent| agent.active && agent.config.strategy.needs_binance())
            && !state.feed_started;
        if spawn_feed {
            state.feed_started = true;
        }
        let run_id = state.run_id;
        let keeper_id = starting_new_keeper.then_some(started_agent_id);
        drop(state);
        if spawn_feed {
            spawn_shared_binance_feed(self.inner.clone(), run_id);
        }
        if let Some(id) = keeper_id {
            spawn_keeper(self.inner.clone(), id, run_id);
        }
        Ok(())
    }

    /// All-or-nothing activation of a fleet. Existing agent IDs are not
    /// overwritten by a group launch; update them individually instead.
    pub fn start_agents(&self, configs: Vec<AgentConfig>) -> Result<u64, String> {
        if configs.is_empty() || configs.len() > 100 {
            return Err("fleet must contain 1–100 agents".into());
        }
        let mut ids = HashSet::new();
        let mut accounts = HashSet::new();
        for config in &configs {
            config.strategy.validate()?;
            if !ids.insert(config.agent_id) || !accounts.insert(config.account_id) {
                return Err("fleet agent IDs and accounts must be unique".into());
            }
        }
        let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
        if !state.running {
            return Err("start the cluster before a fleet".into());
        }
        let status = best_status(&state)
            .cloned()
            .ok_or("wait for a responsive node")?;
        for config in &configs {
            validate_agent_market(config, &status)?;
            if state.agents.contains_key(&config.agent_id) {
                return Err(format!("agent ID {} is already active", config.agent_id));
            }
            if !status
                .account_nonces
                .iter()
                .any(|account| Some(account.account_id) == slot_address(&state, config.account_id))
            {
                return Err(format!("account {} does not exist", config.account_id));
            }
            if status
                .open_orders
                .iter()
                .any(|order| Some(order.account_id) == slot_address(&state, config.account_id))
                || slot_address(&state, config.account_id)
                    .is_some_and(|address| state.pending_airdrops.contains_key(&address))
                || state
                    .agents
                    .values()
                    .any(|agent| agent.config.account_id == config.account_id)
                || state.market_maker.as_ref().is_some_and(|mm| {
                    (mm.active || mm.stopping) && mm.config.account_id == config.account_id
                })
                || state
                    .load
                    .as_ref()
                    .is_some_and(|load| load_uses_account(&load.config.preset, config.account_id))
            {
                return Err(format!(
                    "account {} is already in use or has open orders",
                    config.account_id
                ));
            }
        }
        state.next_group_id += 1;
        let group_id = state.next_group_id;
        let keepers = configs
            .iter()
            .filter(|config| matches!(config.strategy, AgentStrategy::LiquidationKeeper { .. }))
            .map(|config| config.agent_id)
            .collect::<Vec<_>>();
        for config in configs {
            state
                .agents
                .insert(config.agent_id, new_agent(config, Some(group_id)));
        }
        let spawn_feed = state
            .agents
            .values()
            .any(|agent| agent.active && agent.config.strategy.needs_binance())
            && !state.feed_started;
        if spawn_feed {
            state.feed_started = true;
        }
        let run_id = state.run_id;
        state.log(format!("Participant fleet {group_id} started"));
        drop(state);
        if spawn_feed {
            spawn_shared_binance_feed(self.inner.clone(), run_id);
        }
        for id in keepers {
            spawn_keeper(self.inner.clone(), id, run_id);
        }
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
        if agent_ids.iter().any(|id| !state.agents.contains_key(id)) {
            return Err("agent not found".into());
        }
        for id in agent_ids {
            let agent = state.agents.get_mut(&id).expect("prevalidated agent ID");
            agent.active = false;
            agent.stopping = true;
            agent.desired = DesiredBook::default();
        }
        state.log("Participant group stopping");
        Ok(())
    }
}

fn validate_agent_market(config: &AgentConfig, status: &NodeStatus) -> Result<(), String> {
    let market = status
        .markets
        .iter()
        .find(|market| market.id == config.market_id)
        .ok_or_else(|| format!("market {} does not exist", config.market_id))?;
    if matches!(
        config.strategy,
        AgentStrategy::LiquidationKeeper { .. } | AgentStrategy::LeveragedTaker { .. }
    ) && !matches!(
        market.config.market_type,
        light_dex::exchange::market_config::MarketType::Perp { .. }
    ) {
        return Err("liquidation keeper and leveraged taker require a perp market".into());
    }
    if (config.strategy.needs_binance()
        || matches!(config.strategy, AgentStrategy::TrendFollower { .. }))
        && market.config.base_asset != light_dex::exchange::market::BTC
    {
        return Err("Binance makers and the current trend-history adapter require a BTC market; random agents support every market".into());
    }
    if matches!(
        config.strategy,
        AgentStrategy::AvellanedaStoikov { .. } | AgentStrategy::TrendFollower { .. }
    ) && matches!(
        market.config.market_type,
        light_dex::exchange::market_config::MarketType::Perp { .. }
    ) {
        return Err("this preset currently uses spot inventory/history; choose Binance mirror or random order for a perp market".into());
    }
    Ok(())
}

fn participant_order(
    account_id: light_dex::exchange::account::AccountId,
    market_id: u64,
    order_type: light_dex::exchange::order::OrderType,
    side: light_dex::exchange::order::Side,
    price: u64,
    quantity: u64,
    nonce: u64,
    is_perp: bool,
) -> Transaction {
    if is_perp {
        Transaction::PlacePerpOrder {
            account_id,
            market_id,
            order_type,
            side,
            price,
            quantity,
            reduce_only: false,
            nonce,
        }
    } else {
        Transaction::PlaceOrder {
            account_id,
            market_id,
            order_type,
            side,
            price,
            quantity,
            nonce,
        }
    }
}

/// Stress participants use part of their wallet for cross margin. The
/// leveraged-taker preset keeps its explicit, intentionally small deposit so
/// liquidation scenarios still exercise the risk engine.
fn participant_perp_deposit(available_usd_atoms: u64) -> u64 {
    use light_dex::exchange::market::USD_ATOMS;
    let minimum = 100_000 * USD_ATOMS;
    let maximum = 100_000_000 * USD_ATOMS;
    available_usd_atoms.min((available_usd_atoms / 10).clamp(minimum, maximum))
}

/// Add a cushion before existing positions and resting orders consume all
/// initial margin. This is a regular signed collateral deposit, not a mint.
fn participant_perp_top_up(
    available_usd_atoms: u64,
    collateral_atoms: i128,
    equity_atoms: i128,
    initial_required_atoms: i128,
) -> u64 {
    if available_usd_atoms < 1_000 * light_dex::exchange::market::USD_ATOMS {
        return 0;
    }
    let threshold = (10_000_i128 * i128::from(light_dex::exchange::market::USD_ATOMS))
        .max(collateral_atoms / 4)
        .max(initial_required_atoms.saturating_mul(2));
    if equity_atoms >= threshold {
        return 0;
    }
    let needed = threshold.saturating_mul(2).saturating_sub(equity_atoms);
    u64::try_from(needed)
        .unwrap_or(u64::MAX)
        .min(participant_perp_deposit(available_usd_atoms))
}

#[cfg(test)]
mod funding_tests {
    use super::*;
    use light_dex::exchange::market::USD_ATOMS;

    #[test]
    fn stress_collateral_scales_with_wallet_and_respects_the_cap() {
        assert_eq!(
            participant_perp_deposit(1_000_000 * USD_ATOMS),
            100_000 * USD_ATOMS
        );
        assert_eq!(
            participant_perp_deposit(1_000_000_000 * USD_ATOMS),
            100_000_000 * USD_ATOMS
        );
        assert_eq!(participant_perp_deposit(100 * USD_ATOMS), 100 * USD_ATOMS);
    }

    #[test]
    fn top_up_only_when_cross_equity_needs_margin_cushion() {
        let available = 900_000_000 * USD_ATOMS;
        let collateral = 100_000_000_i128 * i128::from(USD_ATOMS);
        assert_eq!(
            participant_perp_top_up(available, collateral, collateral, 0),
            0
        );
        assert_eq!(
            participant_perp_top_up(available, collateral, 20_000_000 * i128::from(USD_ATOMS), 0),
            30_000_000 * USD_ATOMS
        );
        assert_eq!(
            participant_perp_top_up(
                available,
                collateral,
                70_000_000 * i128::from(USD_ATOMS),
                40_000_000 * i128::from(USD_ATOMS)
            ),
            90_000_000 * USD_ATOMS
        );
        assert_eq!(participant_perp_top_up(0, collateral, 0, 0), 0);
        assert_eq!(
            participant_perp_top_up(999 * USD_ATOMS, collateral, 0, 0),
            0
        );
    }
}

fn leverage_order_quantity(
    status: &NodeStatus,
    account_id: light_dex::exchange::account::AccountId,
    market_id: u64,
    side: light_dex::exchange::order::Side,
    price: u64,
    collateral_usd: u64,
    target_leverage_bps: u64,
    available_at_best: u64,
) -> Option<u64> {
    use light_dex::exchange::market_config::MarketType;
    let market = status
        .markets
        .iter()
        .find(|market| market.id == market_id)?;
    let MarketType::Perp { max_leverage } = market.config.market_type else {
        return None;
    };
    let position = status
        .perp_positions
        .iter()
        .find(|position| position.account_id == account_id && position.market_id == market_id);
    let signed_size = position.map_or(0, |position| position.size_atoms);
    if (side == light_dex::exchange::order::Side::Buy && signed_size < 0)
        || (side == light_dex::exchange::order::Side::Sell && signed_size > 0)
    {
        return None;
    }
    let leverage = target_leverage_bps.min(u64::from(max_leverage) * 10_000);
    let notional = u128::from(collateral_usd)
        .checked_mul(u128::from(light_dex::exchange::market::USD_ATOMS))?
        .checked_mul(u128::from(leverage))?
        / 10_000;
    let desired = notional
        .checked_mul(u128::from(market.config.price.scale()))?
        .checked_mul(u128::from(market.config.quantity.scale()))?
        / u128::from(price).checked_mul(u128::from(light_dex::exchange::market::USD_ATOMS))?;
    let remaining = desired.saturating_sub(signed_size.unsigned_abs());
    let capped = remaining.min(u128::from(available_at_best));
    let lot = u128::from(market.config.quantity.step);
    u64::try_from((capped / lot) * lot)
        .ok()
        .filter(|quantity| *quantity > 0)
}

/// One immutable observation per fleet tick, not one full-state copy per agent.
/// Indices contain offsets into the owned status, so no self-referential borrows.
struct AgentObservation {
    run_id: u64,
    status: NodeStatus,
    nonces: HashMap<Address, u64>,
    balances: HashMap<Address, usize>,
    orders: HashMap<(Address, u64), Vec<usize>>,
    failures: HashMap<String, Option<String>>,
    feed: Option<PartialDepth>,
    feed_stale: bool,
    feed_history: VecDeque<(Instant, f64)>,
    local_history: Vec<f64>,
}
impl AgentObservation {
    fn capture(state: &ControllerInner) -> Option<Self> {
        let status = best_status(state)?.clone();
        let nonces = status
            .account_nonces
            .iter()
            .map(|a| (a.account_id, a.nonce))
            .collect();
        let balances = status
            .agent_balances
            .iter()
            .enumerate()
            .map(|(i, a)| (a.account_id, i))
            .collect();
        let mut orders: HashMap<_, Vec<_>> = HashMap::new();
        for (i, order) in status.open_orders.iter().enumerate() {
            orders
                .entry((order.account_id, order.market_id))
                .or_default()
                .push(i);
        }
        let failures = status
            .recent_transaction_outcomes
            .iter()
            .filter(|o| !o.success)
            .map(|o| (o.digest.clone(), o.error.clone()))
            .collect();
        Some(Self {
            run_id: state.run_id,
            status,
            nonces,
            balances,
            orders,
            failures,
            feed: state.feed_depth.clone(),
            feed_stale: state
                .feed_at
                .is_none_or(|at| at.elapsed() > Duration::from_secs(3)),
            feed_history: state.feed_history.clone(),
            local_history: state.local_mid_history.iter().copied().collect(),
        })
    }
}

pub(super) fn spawn_agent_loop(shared: Shared, run_id: u64) {
    // Admission acknowledgements include network RTT and node queueing. Keep
    // those waits off the planner thread while bounding sockets and queued
    // work on a laptop-sized cluster.
    const WORKERS: usize = 8;
    const QUEUED: usize = 32;
    let (sender, receiver) = mpsc::sync_channel::<AgentSubmission>(QUEUED);
    let receiver = Arc::new(Mutex::new(receiver));
    for _ in 0..WORKERS {
        let shared = shared.clone();
        let receiver = receiver.clone();
        thread::spawn(move || agent_submission_worker(shared, receiver, run_id));
    }
    thread::spawn(move || {
        let mut cursor = 0usize;
        loop {
            let (mut ids, observation) = {
                let state = shared.lock().unwrap();
                if !state.running || state.run_id != run_id {
                    break;
                }
                (
                    state.agents.keys().copied().collect::<Vec<_>>(),
                    AgentObservation::capture(&state),
                )
            };
            if let Some(observation) = observation {
                if !ids.is_empty() {
                    let offset = cursor % ids.len();
                    ids.rotate_left(offset);
                }
                let started = Instant::now();
                for id in ids {
                    participant_step(&shared, id, &observation, &sender);
                    cursor = cursor.wrapping_add(1);
                    // Refresh observations under heavy planning load. Rotate
                    // the starting agent to preserve fairness.
                    if started.elapsed() >= Duration::from_millis(200) {
                        break;
                    }
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
    });
}

struct AgentSubmission {
    agent_id: u64,
    digest: String,
    transaction: Transaction,
    target: TargetNode,
    auth_key: [u8; 32],
}

fn agent_submission_worker(
    shared: Shared,
    receiver: Arc<Mutex<mpsc::Receiver<AgentSubmission>>>,
    run_id: u64,
) {
    loop {
        // Release the receiver mutex before waiting for a validator's ACK;
        // otherwise all workers would still serialize behind this one.
        let received = { receiver.lock().unwrap().recv() };
        let Ok(job) = received else { break };
        let destination = {
            let state = shared.lock().unwrap();
            (state.running
                && state.run_id == run_id
                && state.agents.get(&job.agent_id).is_some_and(|agent| {
                    agent
                        .pending
                        .iter()
                        .any(|pending| pending.digest == job.digest)
                }))
            .then(|| resolve_target(&state, &job.target))
            .flatten()
        };
        let result = match destination {
            Some(first) => send_signed_transaction_with_failover(
                &shared,
                first,
                matches!(job.target, TargetNode::Random),
                job.transaction,
                &job.auth_key,
            )
            .map(|_| ()),
            None => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "no available target node or participant stopped",
            )),
        };
        let mut state = shared.lock().unwrap();
        if state.run_id != run_id {
            continue;
        }
        if let Some(agent) = state.agents.get_mut(&job.agent_id) {
            agent.dispatching.remove(&job.digest);
            if let Err(error) = &result
                && agent
                    .pending
                    .iter()
                    .any(|pending| pending.digest == job.digest)
            {
                agent.last_error = Some(format!("Submission failed; retrying: {error}"));
            }
        }
        if result.is_ok() {
            state.submitted += 1;
            state.submit_times.push_back(Instant::now());
        }
    }
}

fn participant_step(
    shared: &Shared,
    id: u64,
    observation: &AgentObservation,
    sender: &mpsc::SyncSender<AgentSubmission>,
) {
    if shared.lock().unwrap().agents.get(&id).is_some_and(|agent| {
        matches!(
            agent.config.strategy,
            AgentStrategy::LiquidationKeeper { .. }
        )
    }) {
        return;
    }
    let dispatch = {
        let mut state = shared.lock().unwrap();
        if !state.running || state.run_id != observation.run_id {
            return;
        }
        let status = &observation.status;
        let feed = &observation.feed;
        let feed_stale = observation.feed_stale;
        let feed_history = &observation.feed_history;
        let local_history = &observation.local_history;
        let auth_key = state.auth_key;
        let Some(account_address) = state
            .agents
            .get(&id)
            .and_then(|agent| slot_address(&state, agent.config.account_id))
        else {
            return;
        };
        let Some(agent) = state.agents.get_mut(&id) else {
            return;
        };
        let Some(confirmed_nonce) = observation.nonces.get(&account_address).copied() else {
            return;
        };
        let Some(balance) = observation
            .balances
            .get(&account_address)
            .map(|&i| &status.agent_balances[i])
        else {
            return;
        };
        let now = Instant::now();
        let own_orders = observation
            .orders
            .get(&(account_address, agent.config.market_id))
            .into_iter()
            .flatten()
            .map(|&i| &status.open_orders[i])
            .collect::<Vec<_>>();
        let own_ids = own_orders
            .iter()
            .map(|order| order.order_id)
            .collect::<HashSet<_>>();
        agent
            .resting_since
            .retain(|order_id, _| own_ids.contains(order_id));
        for order in &own_orders {
            agent.resting_since.entry(order.order_id).or_insert(now);
        }
        let account_nonce = status
            .account_nonces
            .iter()
            .find(|account| account.account_id == account_address)
            .expect("known account");
        agent
            .pending
            .retain(|pending| !account_nonce.consumed(pending.transaction.nonce()));
        if let Some((failed_nonce, error)) = agent.pending.iter().find_map(|pending| {
            observation
                .failures
                .get(&pending.digest)
                .map(|error| (pending.transaction.nonce(), error.clone()))
        }) {
            agent
                .pending
                .retain(|pending| pending.transaction.nonce() != failed_nonce);
            agent.last_error = Some(format!(
                "Transaction at nonce {failed_nonce} failed: {}; replanning from current state",
                error.as_deref().unwrap_or("unknown")
            ));
            agent.last_step = now - Duration::from_millis(agent.config.strategy.interval_ms());
        }
        let mut transaction = agent
            .pending
            .iter_mut()
            .find(|pending| {
                !agent.dispatching.contains(&pending.digest)
                    && now.duration_since(pending.last_sent) >= Duration::from_secs(3)
            })
            .map(|pending| {
                pending.last_sent = now;
                pending.transaction.clone()
            });
        let limit = if agent.config.wait_for_confirmation
            || matches!(agent.config.strategy, AgentStrategy::LeveragedTaker { .. })
        {
            1
        } else {
            MAX_AGENT_IN_FLIGHT
        };
        // Keep at most two quote generations in flight. Recompute the latest
        // desired book when capacity returns rather than accumulating obsolete
        // unsigned intentions. Signed nonces are retained/retried unchanged.
        let limit = if agent.config.strategy.is_maker() {
            limit.min(2)
        } else {
            limit
        };
        let limit = if status.mempool_size > 4096 {
            limit.min(2)
        } else {
            limit
        };
        if transaction.is_none()
            && agent.pending.len() < limit
            && (!agent.stopping || agent.pending.is_empty())
        {
            if agent.stopping && own_orders.is_empty() {
                state.agents.remove(&id);
                state.log(format!("Participant {id} stopped; resting orders cleared"));
                return;
            }
            let Some(nonce) = next_agent_nonce(confirmed_nonce, &agent.pending) else {
                return;
            };
            let interval = Duration::from_millis(agent.config.strategy.interval_ms());
            let refresh_due = now.duration_since(agent.last_step) >= interval;
            if refresh_due
                || agent.stopping
                || agent.config.strategy.is_maker()
                || !own_orders.is_empty()
            {
                if refresh_due {
                    agent.last_step = now;
                }
                let account_id = account_address;
                let market_id = agent.config.market_id;
                let Some(market_snapshot) =
                    status.markets.iter().find(|market| market.id == market_id)
                else {
                    return;
                };
                let tick = market_snapshot.config.price;
                let is_perp = matches!(
                    market_snapshot.config.market_type,
                    light_dex::exchange::market_config::MarketType::Perp { .. }
                );
                let quantity_per_level = market_snapshot.config.quantity.scale();
                let strategy = agent.config.strategy.clone();
                let book = status
                    .order_books
                    .iter()
                    .find(|book| book.market_id == market_id);
                let reference_price = status
                    .oracle_prices
                    .iter()
                    .find(|oracle| {
                        oracle.pair.base_asset == market_snapshot.config.base_asset
                            && oracle.pair.quote_asset == market_snapshot.config.quote_asset
                    })
                    .and_then(|oracle| oracle.committed_mark_usd_e8)
                    .and_then(|mark| tick.round_e8_to_tick(mark).ok());
                let pending_actions = agent
                    .pending
                    .iter()
                    .flat_map(|pending| match &pending.transaction {
                        Transaction::UpdateQuotes {
                            cancel,
                            reduce,
                            place,
                            ..
                        } => cancel
                            .iter()
                            .map(|&order_id| QuoteAction::Cancel { order_id })
                            .chain(reduce.iter().map(|r| QuoteAction::Reduce {
                                order_id: r.order_id,
                                remaining_quantity: r.remaining_quantity,
                            }))
                            .chain(place.iter().map(|order| QuoteAction::Place {
                                side: order.side,
                                price: order.price,
                            }))
                            .collect::<Vec<_>>(),
                        _ => pending.action.into_iter().collect(),
                    })
                    .collect::<Vec<_>>();
                let reserved_usd = agent
                    .pending
                    .iter()
                    .filter_map(|pending| match &pending.transaction {
                        Transaction::PlaceOrder {
                            side: light_dex::exchange::order::Side::Buy,
                            price,
                            quantity,
                            ..
                        } => status
                            .markets
                            .iter()
                            .find(|market| market.id == market_id)
                            .and_then(|snapshot| {
                                light_dex::exchange::market::Market::from_config(
                                    snapshot.id,
                                    snapshot.config,
                                )
                                .quote_atoms(*price, *quantity)
                            }),
                        Transaction::UpdateQuotes { place, .. } if !is_perp => Some(
                            place
                                .iter()
                                .filter(|order| order.side == light_dex::exchange::order::Side::Buy)
                                .filter_map(|order| {
                                    light_dex::exchange::market::Market::from_config(
                                        market_id,
                                        market_snapshot.config,
                                    )
                                    .quote_atoms(order.price, order.quantity)
                                })
                                .fold(0u64, u64::saturating_add),
                        ),
                        _ => None,
                    })
                    .fold(0u64, u64::saturating_add);
                let reserved_base = agent
                    .pending
                    .iter()
                    .filter_map(|pending| match &pending.transaction {
                        Transaction::PlaceOrder {
                            side: light_dex::exchange::order::Side::Sell,
                            quantity,
                            ..
                        } => Some(*quantity),
                        Transaction::UpdateQuotes { place, .. } if !is_perp => Some(
                            place
                                .iter()
                                .filter(|order| {
                                    order.side == light_dex::exchange::order::Side::Sell
                                })
                                .map(|order| order.quantity)
                                .fold(0u64, u64::saturating_add),
                        ),
                        _ => None,
                    })
                    .fold(0u64, u64::saturating_add);
                let available_usd = balance.usd_available.saturating_sub(reserved_usd);
                let available_base = balance
                    .assets
                    .iter()
                    .find(|asset| asset.asset_id == market_snapshot.config.base_asset)
                    .map_or(0, |asset| asset.available)
                    .saturating_sub(reserved_base);
                let mut next = None;
                if agent.stopping {
                    if let Some(order) = own_orders.first() {
                        next = Some((
                            // Quote updates treat an already-filled order as an
                            // idempotent cancel, unlike standalone CancelOrder.
                            Transaction::UpdateQuotes {
                                account_id,
                                market_id,
                                cancel: vec![order.order_id],
                                reduce: vec![],
                                place: vec![],
                                nonce,
                            },
                            Some(QuoteAction::Cancel {
                                order_id: order.order_id,
                            }),
                        ));
                    }
                } else if is_perp
                    && !status.perp_risk.iter().any(|risk| {
                        risk.account_id == account_id
                            && risk.mode == light_dex::exchange::perp::MarginMode::Cross
                    })
                {
                    if !agent.pending.iter().any(|pending| {
                        matches!(pending.transaction, Transaction::SetMarginMode { .. })
                    }) {
                        next = Some((
                            Transaction::SetMarginMode {
                                account_id,
                                mode: light_dex::exchange::perp::MarginMode::Cross,
                                nonce,
                            },
                            None,
                        ));
                    }
                } else if is_perp
                    && !status.perp_risk.iter().any(|risk| {
                        risk.account_id == account_id
                            && risk.collateral_usd_atoms > 0
                            && risk.market_id.is_none()
                    })
                {
                    if !agent.pending.iter().any(|pending| {
                        matches!(
                            pending.transaction,
                            Transaction::DepositPerpCollateral { .. }
                        )
                    }) {
                        let amount = match &strategy {
                            AgentStrategy::LeveragedTaker { collateral_usd, .. } => {
                                balance.usd_available.min(
                                    collateral_usd
                                        .saturating_mul(light_dex::exchange::market::USD_ATOMS),
                                )
                            }
                            _ => participant_perp_deposit(balance.usd_available),
                        };
                        if amount > 0 {
                            next = Some((
                                Transaction::DepositPerpCollateral {
                                    account_id,
                                    market_id,
                                    amount,
                                    nonce,
                                },
                                None,
                            ));
                        } else {
                            agent.last_error =
                                Some("No USD available to deposit as perp collateral".into());
                        }
                    }
                } else if is_perp
                    && !agent.stopping
                    && !matches!(&strategy, AgentStrategy::LeveragedTaker { .. })
                    && !agent.pending.iter().any(|pending| {
                        matches!(
                            pending.transaction,
                            Transaction::DepositPerpCollateral { .. }
                        )
                    })
                    && status.perp_risk.iter().any(|risk| {
                        risk.account_id == account_id
                            && risk.market_id.is_none()
                            && participant_perp_top_up(
                                balance.usd_available,
                                risk.collateral_usd_atoms,
                                risk.equity_usd_atoms,
                                risk.initial_required_usd_atoms,
                            ) > 0
                    })
                {
                    let risk = status
                        .perp_risk
                        .iter()
                        .find(|risk| risk.account_id == account_id && risk.market_id.is_none())
                        .expect("cross risk checked");
                    let amount = participant_perp_top_up(
                        balance.usd_available,
                        risk.collateral_usd_atoms,
                        risk.equity_usd_atoms,
                        risk.initial_required_usd_atoms,
                    );
                    next = Some((
                        Transaction::DepositPerpCollateral {
                            account_id,
                            market_id,
                            amount,
                            nonce,
                        },
                        None,
                    ));
                } else if let AgentStrategy::LeveragedTaker {
                    side,
                    collateral_usd,
                    target_leverage_bps,
                    ..
                } = strategy
                {
                    if refresh_due {
                        let best = book.and_then(|book| match side {
                            light_dex::exchange::order::Side::Buy => book.asks.first(),
                            light_dex::exchange::order::Side::Sell => book.bids.first(),
                        });
                        if let Some(level) = best {
                            if let Some(quantity) = leverage_order_quantity(
                                &status,
                                account_id,
                                market_id,
                                side,
                                level.price,
                                collateral_usd,
                                target_leverage_bps,
                                level.quantity,
                            ) {
                                next = Some((
                                    participant_order(
                                        account_id,
                                        market_id,
                                        light_dex::exchange::order::OrderType::IOC,
                                        side,
                                        level.price,
                                        quantity,
                                        nonce,
                                        true,
                                    ),
                                    None,
                                ));
                            }
                        }
                    }
                } else if strategy.is_maker() {
                    if refresh_due {
                        if !feed_stale
                            && agent.last_error.as_deref()
                                == Some("Binance feed stale; cancelling quotes")
                        {
                            agent.last_error = None;
                        }
                        agent.desired = if feed_stale {
                            DesiredBook::default()
                        } else {
                            match &strategy {
                                AgentStrategy::BinanceMirror { levels, .. } => feed
                                    .as_ref()
                                    .and_then(|depth| {
                                        market_tick(&status, market_id).map(|tick| {
                                            desired_book_for_market(depth, *levels, tick)
                                        })
                                    })
                                    .unwrap_or_default(),
                                AgentStrategy::AvellanedaStoikov {
                                    volatility_window_seconds,
                                    ..
                                } => {
                                    let inventory =
                                        balance.btc_available.saturating_add(balance.btc_locked)
                                            / light_dex::exchange::market::BTC_ATOMS;
                                    feed.as_ref()
                                        .and_then(mid_price)
                                        .zip(feed_variance(
                                            &feed_history,
                                            *volatility_window_seconds,
                                        ))
                                        .and_then(|(mid, variance)| {
                                            strategy
                                                .avellaneda_quotes(mid, variance, inventory, tick)
                                        })
                                        .unwrap_or_default()
                                }
                                _ => DesiredBook::default(),
                            }
                        };
                        if let Some(book) = book {
                            if let Some(ask) = book.asks.first() {
                                for bid in &mut agent.desired.bids {
                                    *bid = (*bid).min(ask.price.saturating_sub(tick.step));
                                }
                            }
                            if let Some(bid) = book.bids.first() {
                                for ask in &mut agent.desired.asks {
                                    *ask = (*ask).max(bid.price.saturating_add(tick.step));
                                }
                            }
                        }
                    }
                    if feed_stale {
                        agent.desired = DesiredBook::default();
                        agent.last_error = Some("Binance feed stale; cancelling quotes".into());
                    }
                    let actions = plan_actions(
                        account_id,
                        market_id,
                        quantity_per_level,
                        &own_orders
                            .iter()
                            .map(|order| (*order).clone())
                            .collect::<Vec<_>>(),
                        &agent.desired,
                        true,
                        &pending_actions,
                    );
                    let mut cancel = Vec::new();
                    let mut reduce = Vec::new();
                    let mut place = Vec::new();
                    let (mut quote_left, mut base_left) = (available_usd, available_base);
                    for action in actions
                        .into_iter()
                        .take(light_dex::exchange::limits::MAX_QUOTE_ACTIONS)
                    {
                        match action {
                            QuoteAction::Cancel { order_id } => cancel.push(order_id),
                            QuoteAction::Reduce {
                                order_id,
                                remaining_quantity,
                            } => reduce.push(light_dex::blockchain::QuoteReduction {
                                order_id,
                                remaining_quantity,
                            }),
                            QuoteAction::Place { side, price } => {
                                if !is_perp {
                                    match side {
                                        light_dex::exchange::order::Side::Buy => {
                                            let Some(cost) =
                                                light_dex::exchange::market::Market::from_config(
                                                    market_id,
                                                    market_snapshot.config,
                                                )
                                                .quote_atoms(price, quantity_per_level)
                                            else {
                                                continue;
                                            };
                                            if quote_left < cost {
                                                continue;
                                            }
                                            quote_left -= cost;
                                        }
                                        light_dex::exchange::order::Side::Sell => {
                                            if base_left < quantity_per_level {
                                                continue;
                                            }
                                            base_left -= quantity_per_level;
                                        }
                                    }
                                }
                                place.push(light_dex::blockchain::QuoteOrder {
                                    order_type: light_dex::exchange::order::OrderType::PostOnly,
                                    side,
                                    price,
                                    quantity: quantity_per_level,
                                    reduce_only: false,
                                });
                            }
                        }
                    }
                    if !cancel.is_empty() || !reduce.is_empty() || !place.is_empty() {
                        next = Some((
                            Transaction::UpdateQuotes {
                                account_id,
                                market_id,
                                cancel,
                                reduce,
                                place,
                                nonce,
                            },
                            None,
                        ));
                    }
                } else {
                    let expired = match &strategy {
                        AgentStrategy::RandomOrder {
                            order_type:
                                light_dex::exchange::order::OrderType::LO
                                | light_dex::exchange::order::OrderType::PostOnly,
                            resting_ttl_ms,
                            max_resting_orders,
                            ..
                        } => own_orders
                            .iter()
                            .find(|order| {
                                !pending_actions.contains(&QuoteAction::Cancel {
                                    order_id: order.order_id,
                                }) && agent.resting_since.get(&order.order_id).is_some_and(|at| {
                                    now.duration_since(*at)
                                        >= Duration::from_millis(*resting_ttl_ms)
                                })
                            })
                            .or_else(|| {
                                (own_orders.len()
                                    + pending_actions
                                        .iter()
                                        .filter(|action| {
                                            matches!(action, QuoteAction::Place { .. })
                                        })
                                        .count()
                                    >= *max_resting_orders)
                                    .then(|| {
                                        own_orders.iter().find(|order| {
                                            !pending_actions.contains(&QuoteAction::Cancel {
                                                order_id: order.order_id,
                                            })
                                        })
                                    })
                                    .flatten()
                            }),
                        _ => own_orders.iter().find(|order| {
                            !pending_actions.contains(&QuoteAction::Cancel {
                                order_id: order.order_id,
                            })
                        }),
                    };
                    if let Some(order) = expired {
                        next = Some((
                            Transaction::CancelOrder {
                                account_id,
                                market_id,
                                order_id: order.order_id,
                                nonce,
                            },
                            Some(QuoteAction::Cancel {
                                order_id: order.order_id,
                            }),
                        ));
                    } else if refresh_due
                        && agent.active
                        && !matches!(&strategy,
                        AgentStrategy::RandomOrder { order_type: light_dex::exchange::order::OrderType::LO | light_dex::exchange::order::OrderType::PostOnly, max_resting_orders, .. }
                            if own_orders.len() + pending_actions.iter().filter(|action| matches!(action, QuoteAction::Place { .. })).count() >= *max_resting_orders)
                    {
                        let side = match &strategy {
                            AgentStrategy::RandomOrder { .. } => {
                                strategy.random_side(&mut agent.rng)
                            }
                            AgentStrategy::TrendFollower { .. } => {
                                strategy.trend_side(&local_history, tick)
                            }
                            _ => None,
                        };
                        if let Some(side) = side {
                            let (order_type, passive_offset) = match strategy {
                                AgentStrategy::RandomOrder {
                                    order_type,
                                    passive_offset_ticks,
                                    ..
                                } => (order_type, passive_offset_ticks),
                                _ => (light_dex::exchange::order::OrderType::IOC, 0),
                            };
                            let passive_offset = passive_offset.saturating_mul(tick.step);
                            let price = match (order_type, side) {
                                (
                                    light_dex::exchange::order::OrderType::LO
                                    | light_dex::exchange::order::OrderType::PostOnly,
                                    light_dex::exchange::order::Side::Buy,
                                ) => book
                                    .and_then(|book| {
                                        book.bids.first().map(|level| {
                                            level.price.saturating_sub(passive_offset).max(1)
                                        })
                                    })
                                    .or_else(|| {
                                        reference_price.map(|price| {
                                            price.saturating_sub(passive_offset).max(tick.step)
                                        })
                                    }),
                                (
                                    light_dex::exchange::order::OrderType::LO
                                    | light_dex::exchange::order::OrderType::PostOnly,
                                    light_dex::exchange::order::Side::Sell,
                                ) => book
                                    .and_then(|book| {
                                        book.asks
                                            .first()
                                            .map(|level| level.price.saturating_add(passive_offset))
                                    })
                                    .or_else(|| {
                                        reference_price
                                            .map(|price| price.saturating_add(passive_offset))
                                    }),
                                (_, light_dex::exchange::order::Side::Buy) => {
                                    book.and_then(|book| book.asks.first().map(|level| level.price))
                                }
                                (_, light_dex::exchange::order::Side::Sell) => {
                                    book.and_then(|book| book.bids.first().map(|level| level.price))
                                }
                            };
                            let price = price.map(|price| {
                                if !order_type.rests() {
                                    return price;
                                }
                                match side {
                                    light_dex::exchange::order::Side::Buy => own_orders
                                        .iter()
                                        .filter(|order| {
                                            order.side == light_dex::exchange::order::Side::Sell
                                        })
                                        .map(|order| order.price)
                                        .min()
                                        .map_or(price, |own_ask| {
                                            price.min(own_ask.saturating_sub(tick.step))
                                        }),
                                    light_dex::exchange::order::Side::Sell => own_orders
                                        .iter()
                                        .filter(|order| {
                                            order.side == light_dex::exchange::order::Side::Buy
                                        })
                                        .map(|order| order.price)
                                        .max()
                                        .map_or(price, |own_bid| {
                                            price.max(own_bid.saturating_add(tick.step))
                                        }),
                                }
                            });
                            if let Some(price) = price.filter(|price| *price > 0) {
                                let quantity = quantity_per_level;
                                let fok_has_depth =
                                    if order_type == light_dex::exchange::order::OrderType::FOK {
                                        let displayed = book
                                            .map(|book| match side {
                                                light_dex::exchange::order::Side::Buy => book
                                                    .asks
                                                    .iter()
                                                    .filter(|level| level.price <= price)
                                                    .fold(0u64, |total, level| {
                                                        total.saturating_add(level.quantity)
                                                    }),
                                                light_dex::exchange::order::Side::Sell => book
                                                    .bids
                                                    .iter()
                                                    .filter(|level| level.price >= price)
                                                    .fold(0u64, |total, level| {
                                                        total.saturating_add(level.quantity)
                                                    }),
                                            })
                                            .unwrap_or(0);
                                        displayed >= quantity
                                    } else {
                                        true
                                    };
                                let quote = status
                                    .markets
                                    .iter()
                                    .find(|market| market.id == market_id)
                                    .and_then(|snapshot| {
                                        light_dex::exchange::market::Market::from_config(
                                            snapshot.id,
                                            snapshot.config,
                                        )
                                        .quote_atoms(price, quantity)
                                    });
                                if fok_has_depth
                                    && (is_perp
                                        || (side == light_dex::exchange::order::Side::Buy
                                            && quote.is_some_and(|amount| available_usd >= amount))
                                        || (side == light_dex::exchange::order::Side::Sell
                                            && available_base >= quantity))
                                {
                                    next = Some((
                                        participant_order(
                                            account_id, market_id, order_type, side, price,
                                            quantity, nonce, is_perp,
                                        ),
                                        order_type
                                            .rests()
                                            .then_some(QuoteAction::Place { side, price }),
                                    ));
                                }
                            }
                        }
                    }
                }
                if let Some((tx, action)) = next {
                    let position = agent
                        .pending
                        .iter()
                        .position(|pending| pending.transaction.nonce() > tx.nonce())
                        .unwrap_or(agent.pending.len());
                    agent.pending.insert(
                        position,
                        PendingAgentTransaction {
                            digest: hex_encode(&tx.to_digest()),
                            transaction: tx.clone(),
                            action,
                            last_sent: now,
                        },
                    );
                    transaction = Some(tx);
                }
            }
        }
        transaction.map(|tx| {
            let digest = hex_encode(&tx.to_digest());
            agent.dispatching.insert(digest.clone());
            AgentSubmission {
                agent_id: id,
                digest,
                transaction: tx,
                target: agent.config.target.clone(),
                auth_key,
            }
        })
    };
    if let Some(job) = dispatch {
        if let Err(error) = sender.try_send(job) {
            let job = match error {
                mpsc::TrySendError::Full(job) | mpsc::TrySendError::Disconnected(job) => job,
            };
            let mut state = shared.lock().unwrap();
            if state.run_id != observation.run_id {
                return;
            }
            if let Some(agent) = state.agents.get_mut(&id) {
                agent.dispatching.remove(&job.digest);
                if let Some(pending) = agent
                    .pending
                    .iter_mut()
                    .find(|pending| pending.digest == job.digest)
                {
                    pending.last_sent = Instant::now() - Duration::from_secs(3);
                }
            }
        }
    }
}
