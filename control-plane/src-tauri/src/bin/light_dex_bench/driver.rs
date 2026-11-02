use super::*;

impl ExtraDriver {
    pub(super) fn new(workload: Workload, window: usize, accounts: Vec<u64>) -> Self {
        let driver_base = accounts.iter().copied().min().unwrap_or(0);
        let now = Instant::now();
        Self {
            first_submissions: HashMap::new(),
            scheduled_submissions: HashMap::new(),
            submissions: None,
            offered: 0,
            scheduled_slots: 0,
            blocked_slots: 0,
            rejected: 0,
            rejected_reasons: BTreeMap::new(),
            admission_latency_ms: Vec::new(),
            observed_confirmation_ms: Vec::new(),
            pending: HashMap::new(),
            confirmed: HashMap::new(),
            workload,
            accounts,
            driver_base,
            wallet: BTreeMap::new(),
            window,
            cursor: 0,
            attempted: 0,
            repair_submissions: 0,
            retry_submissions: 0,
            retry_after: Duration::from_secs(2),
            fatal_error: None,
            schedule_origin: now,
            next_at: now,
        }
    }

    #[cfg(test)]
    pub(super) fn observe_account(
        &mut self,
        account: u64,
        observed_nonce: u64,
        outcomes: &[TransactionOutcome],
    ) {
        let failures = outcomes
            .iter()
            .filter(|outcome| !outcome.success)
            .map(|outcome| outcome.digest.as_str())
            .collect();
        self.observe_account_indexed(
            account,
            &AccountNonce {
                account_id: account.into(),
                nonce: observed_nonce,
                used_nonces: [u64::MAX; 2],
            },
            &failures,
        );
    }

    pub(super) fn observe_account_indexed(
        &mut self,
        account: u64,
        observed: &AccountNonce,
        failures: &std::collections::HashSet<&str>,
    ) {
        let confirmed = self.confirmed.entry(account).or_insert(observed.nonce);
        *confirmed = (*confirmed).max(observed.nonce);
        if let Some(queue) = self.pending.get_mut(&account) {
            queue.retain(|nonce, pending| {
                if observed.consumed(*nonce) {
                    if !pending.failed {
                        self.observed_confirmation_ms.push(
                            pending
                                .first_sent
                                .elapsed()
                                .as_millis()
                                .min(u64::MAX as u128) as u64,
                        );
                    }
                    return false;
                }
                if failures.contains(pending.digest.as_str()) {
                    pending.failed = true;
                }
                true
            });
        }
    }

    pub(super) fn next_nonce(&self, account: u64) -> Option<u64> {
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

    pub(super) fn pending_count(&self) -> usize {
        self.pending.values().map(BTreeMap::len).sum()
    }

    pub(super) fn remember(&mut self, transaction: Transaction, now: Instant) {
        let Some(account) = self
            .wallet
            .iter()
            .find_map(|(slot, address)| (*address == transaction.account_id()).then_some(*slot))
        else {
            return;
        };
        let nonce = transaction.nonce();
        self.pending.entry(account).or_default().insert(
            nonce,
            PendingTx {
                digest: hex_encode(&transaction.to_digest()),
                transaction,
                last_sent: now,
                failed: false,
                repair_attempts: 0,
                first_sent: now,
                sending: false,
            },
        );
    }

    pub(super) fn maintenance_action(
        &self,
        account: u64,
        now: Instant,
    ) -> Option<MaintenanceAction> {
        let (&nonce, pending) = self.pending.get(&account)?.first_key_value()?;
        if pending.sending {
            return None;
        }
        if pending.failed {
            Some(MaintenanceAction::Repair(repair_transfer(
                self.wallet[&account],
                nonce,
            )))
        } else if now.duration_since(pending.last_sent) >= self.retry_after {
            Some(MaintenanceAction::Retry(pending.transaction.clone()))
        } else {
            None
        }
    }

    pub(super) fn step(
        &mut self,
        controller: &ControllerState,
        snapshot: &ControlSnapshot,
        tps: u64,
    ) {
        let Some(status) = best_status(snapshot) else {
            return;
        };
        self.step_status(controller, status, tps, None);
    }

    pub(super) fn step_status(
        &mut self,
        controller: &ControllerState,
        status: &NodeStatus,
        tps: u64,
        cutoff: Option<Instant>,
    ) {
        if self.submissions.is_none() {
            self.submissions = Some(submissions::Submissions::new(controller));
        }
        for result in self.submissions.as_ref().unwrap().drain() {
            if result.first {
                self.attempted += u64::from(result.accepted);
                self.rejected += u64::from(!result.accepted);
                self.admission_latency_ms.push(result.elapsed_ms);
            }
            if let Some(reason) = result.error_category {
                *self.rejected_reasons.entry(reason.to_string()).or_default() += 1;
            }
            if let Some(pending) = self
                .pending
                .get_mut(&result.account)
                .and_then(|queue| queue.get_mut(&result.nonce))
            {
                pending.sending = false;
                if !result.accepted {
                    pending.last_sent = Instant::now() - Duration::from_secs(2);
                }
            }
        }
        let reverse: HashMap<_, _> = self
            .accounts
            .iter()
            .filter_map(|slot| self.wallet.get(slot).map(|address| (*address, *slot)))
            .collect();
        let failures = status
            .recent_transaction_outcomes
            .iter()
            .filter(|outcome| !outcome.success)
            .map(|outcome| outcome.digest.as_str())
            .collect();
        for entry in &status.account_nonces {
            if let Some(&slot) = reverse.get(&entry.account_id) {
                self.observe_account_indexed(slot, entry, &failures);
            }
        }
        self.maintain(controller);
        if tps == 0 {
            return;
        }
        // The outer measurement loop polls every 20 ms; catch up scheduled
        // slots so the requested rate is not silently capped at 50 TPS.
        for _ in 0..256 {
            let now = Instant::now();
            if now < self.next_at || cutoff.is_some_and(|deadline| self.next_at >= deadline) {
                break;
            }
            let scheduled_at = self.next_at;
            self.scheduled_slots += 1;
            self.next_at = self.schedule_origin + scheduled_offset(self.scheduled_slots, tps);
            let now_unix_us = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_micros();
            let scheduled_unix_us =
                now_unix_us.saturating_sub(now.duration_since(scheduled_at).as_micros());
            if !self.send_one(controller, status, scheduled_unix_us) {
                self.blocked_slots += 1;
            }
        }
    }

    pub(super) fn maintain(&mut self, _controller: &ControllerState) {
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
                let nonce = *self.pending[&account].first_key_value().unwrap().0;
                if self.pending[&account][&nonce].repair_attempts >= 3 {
                    self.fatal_error.get_or_insert_with(|| {
                        format!(
                            "account {account} nonce {nonce} failed after three repair submissions"
                        )
                    });
                    continue;
                }
            }
            let transaction = match &action {
                MaintenanceAction::Repair(transaction) | MaintenanceAction::Retry(transaction) => {
                    transaction.clone()
                }
            };
            if self
                .submissions
                .as_ref()
                .is_some_and(|pool| pool.send(account, transaction.clone(), false))
            {
                let nonce = *self.pending[&account].first_key_value().unwrap().0;
                let pending = self
                    .pending
                    .get_mut(&account)
                    .unwrap()
                    .get_mut(&nonce)
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
                pending.sending = true;
                sent += 1;
            }
        }
    }

    pub(super) fn send_one(
        &mut self,
        _controller: &ControllerState,
        status: &NodeStatus,
        scheduled_unix_us: u128,
    ) -> bool {
        for _ in 0..self.accounts.len() {
            let account = self.accounts[self.cursor as usize % self.accounts.len()];
            self.cursor += 1;
            let Some(nonce) = self.next_nonce(account) else {
                continue;
            };
            let Some(tx) = build_transaction(
                self.workload,
                self.driver_base,
                account,
                nonce,
                status,
                &self.wallet,
            ) else {
                continue;
            };
            let submitted_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_micros();
            if self
                .submissions
                .as_ref()
                .is_some_and(|pool| pool.send(account, tx.clone(), true))
            {
                self.first_submissions
                    .entry(hex_encode(&tx.to_digest()))
                    .or_insert(submitted_at);
                self.scheduled_submissions
                    .entry(hex_encode(&tx.to_digest()))
                    .or_insert(scheduled_unix_us);
                self.remember(tx, Instant::now());
                self.pending
                    .get_mut(&account)
                    .unwrap()
                    .get_mut(&nonce)
                    .unwrap()
                    .sending = true;
                self.offered += 1;
            } else {
                return false;
            }
            return true;
        }
        false
    }
}

pub(super) fn scheduled_offset(slot: u64, tps: u64) -> Duration {
    let offset_ns = (slot as u128 * 1_000_000_000 / tps as u128).min(u64::MAX as u128) as u64;
    Duration::from_nanos(offset_ns)
}

pub(super) fn participant_fleet(count: usize) -> Vec<AgentConfig> {
    if count >= 100 {
        let template = hundred_participant_fleet_template();
        return (0..count)
            .map(|index| {
                let mut config = template[index % template.len()].clone();
                config.agent_id = index as u64 + 1;
                config.account_id = index as u64;
                config
            })
            .collect();
    }
    let mut fleet = participant_fleet_template();
    fleet.truncate(count);
    for index in fleet.len()..count {
        let account_id = if index < 30 {
            70 + index as u64
        } else {
            (index - 30) as u64
        };
        let order_type = if index % 3 == 0 {
            OrderType::LO
        } else {
            OrderType::IOC
        };
        fleet.push(AgentConfig {
            agent_id: index as u64 + 1,
            account_id,
            market_id: 0,
            target: TargetNode::Random,
            wait_for_confirmation: false,
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

pub(super) fn repair_transfer(
    account: light_dex::exchange::address::Address,
    nonce: u64,
) -> Transaction {
    Transaction::Transfer {
        from: account,
        to: account,
        asset: 0,
        amount: 1,
        nonce,
    }
}

pub(super) fn build_transaction(
    workload: Workload,
    driver_base: u64,
    account: u64,
    nonce: u64,
    status: &NodeStatus,
    wallet: &std::collections::BTreeMap<u64, light_dex::exchange::address::Address>,
) -> Option<Transaction> {
    let address = *wallet.get(&account)?;
    let next_address = *wallet.iter().find(|(label, _)| **label != account)?.1;
    // Dedicated matching workload: independent maker/taker accounts avoid
    // self trades; small lots sustain long runs without exhausting the faucet.
    // No transfer fallback is allowed in this workload.
    if workload == Workload::Orderflow {
        return Some(Transaction::PlaceOrder {
            account_id: address,
            market_id: 0,
            order_type: if account % 2 == 0 {
                OrderType::LO
            } else {
                OrderType::IOC
            },
            side: if account % 2 == 0 {
                Side::Sell
            } else {
                Side::Buy
            },
            price: 1_000_000,
            quantity: 100,
            nonce,
        });
    }
    if matches!(workload, Workload::Multimarket | Workload::BalancedSix) {
        // The legacy workload drives spot books only. BalancedSix assigns
        // independent maker/taker pairs to all six spot and perp books.
        let relative = account.checked_sub(driver_base)?;
        let market_id = if workload == Workload::BalancedSix {
            balanced_market(relative)
        } else {
            [0, 2, 8][((relative / 2) % 3) as usize]
        };
        let market = status
            .markets
            .iter()
            .find(|market| market.id == market_id)?;
        let reference = status
            .oracle_prices
            .iter()
            .find(|oracle| {
                oracle.pair.base_asset == market.config.base_asset
                    && oracle.pair.quote_asset == market.config.quote_asset
            })
            .and_then(|oracle| oracle.committed_mark_usd_e8)
            .and_then(|price| market.config.price.round_e8_to_tick(price).ok())?;
        let maker = relative % 2 == 0;
        let price = if maker {
            reference.saturating_add(market.config.price.step * 10)
        } else {
            reference.saturating_add(market.config.price.step * 20)
        };
        let order_type = if maker { OrderType::LO } else { OrderType::IOC };
        let side = if maker { Side::Sell } else { Side::Buy };
        let quantity = market.config.quantity.scale();
        return Some(
            if workload == Workload::BalancedSix && matches!(market_id, 1 | 3 | 9) {
                Transaction::PlacePerpOrder {
                    account_id: address,
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
                    account_id: address,
                    market_id,
                    order_type,
                    side,
                    price,
                    quantity,
                    nonce,
                }
            },
        );
    }
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
            quantity: light_dex::exchange::market::BTC_ATOMS,
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
                .map_or(1_000_100, |level| level.price.saturating_add(2)),
            quantity: light_dex::exchange::market::BTC_ATOMS,
            nonce,
        },
        2 => Transaction::PlaceOrder {
            account_id: address,
            market_id: 0,
            order_type: OrderType::LO,
            side: Side::Buy,
            price: book
                .and_then(|b| b.bids.first())
                .map_or(999_900, |level| level.price.saturating_sub(2).max(1)),
            quantity: light_dex::exchange::market::BTC_ATOMS,
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
            quantity: light_dex::exchange::market::BTC_ATOMS,
            nonce,
        },
        _ => Transaction::PlaceOrder {
            account_id: address,
            market_id: 0,
            order_type: OrderType::FOK,
            side: Side::Sell,
            price: book?.bids.first()?.price,
            quantity: light_dex::exchange::market::BTC_ATOMS,
            nonce,
        },
    })
}

pub(super) fn balanced_market(relative: u64) -> u64 {
    [0, 1, 2, 3, 8, 9][((relative / 2) % 6) as usize]
}
