use super::*;
impl ControllerState {
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
        if matches!(
            &config.preset,
            LoadPreset::RoundRobinTransfers { amount: 0, .. }
                | LoadPreset::RandomTransfers { amount: 0, .. }
        ) {
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
            if state.market_maker.as_ref().is_some_and(|mm| {
                (mm.active || mm.stopping) && (first..end).contains(&mm.config.account_id)
            }) {
                return Err("sender account range overlaps the market maker".into());
            }
            if state
                .agents
                .values()
                .any(|agent| (first..end).contains(&agent.config.account_id))
            {
                return Err("sender account range overlaps a participant".into());
            }
            if (first..end).any(|id| {
                slot_address(&state, id)
                    .is_some_and(|address| state.pending_airdrops.contains_key(&address))
            }) {
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
}

pub(super) fn reconcile_load(state: &mut ControllerInner, status: &NodeStatus) {
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
                Some(account.account_id) == wallet.get(&account_id).copied()
                    && account.consumed(pending.transaction.nonce())
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
            Some(account.account_id) == wallet.get(&account_id).copied()
                && account.consumed(pending.transaction.nonce())
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

pub(super) fn best_status(state: &ControllerInner) -> Option<&NodeStatus> {
    state
        .nodes
        .values()
        .filter(|node| node.state == "ok")
        .filter_map(|node| node.status.as_ref())
        .max_by_key(|status| (!status.account_nonces.is_empty(), status.chain_height))
}

pub(super) fn load_uses_account(preset: &LoadPreset, account_id: u64) -> bool {
    match *preset {
        LoadPreset::BookTaker {
            account_id: taker, ..
        } => taker == account_id,
        LoadPreset::RoundRobinTransfers {
            first_account,
            account_count,
            ..
        }
        | LoadPreset::RandomTransfers {
            first_account,
            account_count,
            ..
        } => {
            account_id >= first_account && account_id < first_account.saturating_add(account_count)
        }
    }
}

pub(super) fn spawn_load_loop(shared: Shared, load_id: u64) {
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

pub(super) fn next_load_transaction(
    load: &mut LoadRuntime,
    status: &NodeStatus,
    wallet: &BTreeMap<u64, Address>,
) -> Option<Transaction> {
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
        LoadPreset::BookTaker {
            account_id,
            market_id,
            side,
        } => {
            let book = status
                .order_books
                .iter()
                .find(|book| book.market_id == market_id)?;
            let price = match side {
                light_dex::exchange::order::Side::Buy => book.asks.first()?.price,
                light_dex::exchange::order::Side::Sell => book.bids.first()?.price,
            };
            Transaction::PlaceOrder {
                account_id: *wallet.get(&account_id)?,
                market_id,
                order_type: light_dex::exchange::order::OrderType::IOC,
                side,
                price,
                quantity: light_dex::exchange::market::BTC_ATOMS,
                nonce,
            }
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
