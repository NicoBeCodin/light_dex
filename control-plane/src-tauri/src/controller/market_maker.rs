use super::*;
impl ControllerState {
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
            if !state.running {
                return Err("start the simulation first".into());
            }
            let status = best_status(&state).ok_or("wait for a responsive node")?;
            if !status
                .account_nonces
                .iter()
                .any(|account| Some(account.account_id) == slot_address(&state, config.account_id))
            {
                return Err("market maker account does not exist".into());
            }
            if !state.market_maker.as_ref().is_some_and(|mm| mm.active)
                && status
                    .open_orders
                    .iter()
                    .any(|order| Some(order.account_id) == slot_address(&state, config.account_id))
            {
                return Err(
                    "market maker account already has open orders; use a clean account".into(),
                );
            }
            if state
                .load
                .as_ref()
                .is_some_and(|load| load_uses_account(&load.config.preset, config.account_id))
            {
                return Err(
                    "market maker account is already used by the transaction sender".into(),
                );
            }
            if state
                .agents
                .values()
                .any(|agent| agent.config.account_id == config.account_id)
            {
                return Err("market maker account is already used by a participant".into());
            }
            match state.market_maker.as_mut() {
                Some(mm) if mm.active => {
                    if mm.config.account_id != config.account_id {
                        return Err(
                            "stop and clear the old account's quotes before changing accounts"
                                .into(),
                        );
                    }
                    mm.config = config;
                    mm.last_applied = None;
                    mm.last_error = None;
                    state.log("Market maker settings updated");
                }
                Some(mm) if mm.stopping => {
                    return Err("wait for existing quotes to cancel first".into());
                }
                _ => {
                    state.next_market_maker_id += 1;
                    let id = state.next_market_maker_id;
                    state.market_maker = Some(MarketMakerRuntime {
                        id,
                        config,
                        active: true,
                        stopping: false,
                        latest_depth: None,
                        feed_at: None,
                        desired: DesiredBook::default(),
                        last_applied: None,
                        pending: VecDeque::new(),
                        last_error: None,
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
}

pub(super) fn spawn_market_maker_loop(shared: Shared, id: u64) {
    thread::spawn(move || {
        loop {
            if !market_maker_step(&shared, id) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
}

pub(super) fn market_maker_step(shared: &Shared, id: u64) -> bool {
    let dispatch = {
        let mut state = shared.lock().unwrap();
        if !state.running {
            return false;
        }
        let Some(status) = best_status(&state).cloned() else {
            return true;
        };
        let auth_key = state.auth_key;
        let Some(account_address) = state
            .market_maker
            .as_ref()
            .and_then(|mm| slot_address(&state, mm.config.account_id))
        else {
            return true;
        };
        let Some(mm) = state.market_maker.as_mut().filter(|mm| mm.id == id) else {
            return false;
        };
        if !mm.active && !mm.stopping {
            return false;
        }
        let Some(account_nonce) = status
            .account_nonces
            .iter()
            .find(|account| account.account_id == account_address)
        else {
            return true;
        };
        let nonce = account_nonce.nonce;
        let now = Instant::now();
        for pending in &mm.pending {
            if let Some(outcome) = status
                .recent_transaction_outcomes
                .iter()
                .find(|outcome| outcome.digest == pending.digest)
            {
                if !outcome.success {
                    mm.last_error = Some(format!(
                        "Quote transaction failed at nonce {}: {}. Check outstanding quotes before restarting.",
                        pending.transaction.nonce(),
                        outcome.error.as_deref().unwrap_or("unknown error")
                    ));
                    mm.active = false;
                    mm.stopping = false;
                    mm.pending.clear();
                    return false;
                }
            }
        }
        while mm
            .pending
            .front()
            .is_some_and(|pending| account_nonce.consumed(pending.transaction.nonce()))
        {
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
            if mm
                .feed_at
                .is_some_and(|at| now.duration_since(at) <= max_age)
            {
                if mm.last_applied.is_none_or(|at| {
                    now.duration_since(at) >= Duration::from_millis(mm.config.refresh_ms)
                }) {
                    if let Some(depth) = &mm.latest_depth {
                        mm.desired = market_tick(&status, 0)
                            .map(|tick| desired_book_for_market(depth, mm.config.levels, tick))
                            .unwrap_or_default();
                    }
                    mm.last_applied = Some(now);
                    mm.last_error = None;
                }
            } else {
                mm.desired = DesiredBook::default();
                if mm.feed_at.is_some() {
                    mm.last_error =
                        Some("Binance depth feed is stale; quotes are being cancelled".into());
                }
            }
        } else {
            mm.desired = DesiredBook::default();
        }
        let pending_actions = mm
            .pending
            .iter()
            .map(|pending| pending.action)
            .collect::<Vec<_>>();
        let available = 20usize.saturating_sub(mm.pending.len());
        let actions = plan_actions(
            account_address,
            0,
            light_dex::exchange::market::BTC_ATOMS,
            &status.open_orders,
            &mm.desired,
            mm.active,
            &pending_actions,
        );
        let next_nonce = mm
            .pending
            .back()
            .map_or(nonce, |pending| {
                pending.transaction.nonce().saturating_add(1)
            })
            .max(nonce);
        for (index, action) in actions.into_iter().take(available).enumerate() {
            let tx_nonce = next_nonce + index as u64;
            let tx = match action {
                QuoteAction::Reduce {
                    order_id,
                    remaining_quantity,
                } => Transaction::ReduceOrder {
                    account_id: account_address,
                    market_id: 0,
                    order_id,
                    remaining_quantity,
                    nonce: tx_nonce,
                },
                QuoteAction::Cancel { order_id } => Transaction::UpdateQuotes {
                    account_id: account_address,
                    market_id: 0,
                    cancel: vec![order_id],
                    reduce: vec![],
                    place: vec![],
                    nonce: tx_nonce,
                },
                QuoteAction::Place { side, price } => Transaction::PlaceOrder {
                    account_id: account_address,
                    market_id: 0,
                    order_type: light_dex::exchange::order::OrderType::PostOnly,
                    side,
                    price,
                    quantity: light_dex::exchange::market::BTC_ATOMS,
                    nonce: tx_nonce,
                },
            };
            mm.pending.push_back(PendingQuote {
                action,
                digest: hex_encode(&tx.to_digest()),
                transaction: tx.clone(),
                last_sent: now,
            });
            transactions.push(tx);
        }
        if mm.stopping
            && mm.pending.is_empty()
            && transactions.is_empty()
            && !status
                .open_orders
                .iter()
                .any(|order| order.account_id == account_address && order.market_id == 0)
        {
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
            None => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "no available target node",
            )),
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
