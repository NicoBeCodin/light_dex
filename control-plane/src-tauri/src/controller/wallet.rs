use super::*;

impl ControllerState {
    pub fn create_address(&self) -> Result<WalletAccount, String> {
        let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
        if !state.running {
            return Err("start the simulation before creating an address".into());
        }
        let dir = PathBuf::from(
            state
                .active_data_dir
                .as_deref()
                .ok_or("no active session directory")?,
        );
        let label = state
            .wallet
            .keys()
            .next_back()
            .map_or(0, |last| last.saturating_add(1));
        if state.wallet.contains_key(&label) {
            return Err("wallet label space exhausted".into());
        }
        let key = new_key().map_err(|error| error.to_string())?;
        let address = Address::from_public_key(&key.verifying_key());
        write_secret(&dir.join("keys").join(format!("account-{label}.key")), &key)
            .map_err(|error| error.to_string())?;
        let mut wallet = state.wallet.clone();
        wallet.insert(label, address);
        let temporary = dir.join(format!("wallet.json.tmp-{:016x}", rand::random::<u64>()));
        let result = (|| -> io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&serde_json::to_vec_pretty(&wallet).map_err(io::Error::other)?)?;
            file.sync_all()?;
            std::fs::rename(&temporary, dir.join("wallet.json"))?;
            #[cfg(unix)]
            std::fs::File::open(&dir)?.sync_all()?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = std::fs::remove_file(&temporary);
            return Err(error.to_string());
        }
        state.wallet = wallet;
        state.wallet_slots.insert(address, label);
        state.account_keys.insert(label, key);
        state.log(format!("Created wallet #{label}: {address} (unfunded)"));
        Ok(WalletAccount { label, address })
    }

    /// Create independent participant identities, submit their signed claims,
    /// and return only after every account is funded on-chain.
    pub fn claim_new_accounts(&self, count: u64) -> Result<Vec<WalletAccount>, String> {
        if count == 0 || count > 100 {
            return Err("claim batch must contain 1–100 accounts".into());
        }
        let run_id = {
            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            if !state.running {
                return Err("launch the cluster before claiming accounts".into());
            }
            if state
                .claim_progress
                .as_ref()
                .is_some_and(|progress| progress.active)
            {
                return Err("another claim batch is in progress".into());
            }
            if state.pending_claims.len() > count as usize {
                return Err(format!(
                    "{} incomplete claims must be retried first",
                    state.pending_claims.len()
                ));
            }
            let status = best_status(&state).ok_or("wait for a responsive node")?;
            let remaining = status
                .faucet_remaining_claims
                .ok_or("claim allowance unavailable")?;
            let already_claimed = state
                .pending_claims
                .iter()
                .filter(|wallet| {
                    status
                        .account_nonces
                        .iter()
                        .any(|account| account.account_id == wallet.address && account.nonce >= 1)
                })
                .count() as u64;
            if remaining < count - already_claimed {
                return Err(format!(
                    "faucet has {remaining} claims remaining; this batch needs {}",
                    count - already_claimed
                ));
            }
            state.claim_progress = Some(ClaimProgress {
                active: true,
                requested: count as usize,
                confirmed: already_claimed as usize,
                error: None,
            });
            state.run_id
        };
        let result = self.claim_new_accounts_inner(count as usize, run_id);
        if let Ok(mut state) = self.inner.lock() {
            if state.run_id == run_id {
                if let Some(progress) = state.claim_progress.as_mut() {
                    progress.active = false;
                    progress.error = result.as_ref().err().cloned();
                }
                if let Err(error) = &result {
                    state.log(format!("Claim batch stopped: {error}"));
                }
            }
        }
        result
    }

    fn claim_new_accounts_inner(
        &self,
        count: usize,
        run_id: u64,
    ) -> Result<Vec<WalletAccount>, String> {
        while self
            .inner
            .lock()
            .map_err(|_| "controller lock poisoned")?
            .pending_claims
            .len()
            < count
        {
            let wallet = self.create_address()?;
            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            if !state.running || state.run_id != run_id {
                return Err("cluster stopped during account creation".into());
            }
            state.pending_claims.push(wallet);
        }
        let wallets = self
            .inner
            .lock()
            .map_err(|_| "controller lock poisoned")?
            .pending_claims
            .clone();
        let (auth_key, faucet) = {
            let state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            (
                state.auth_key,
                state
                    .signing_manifest
                    .as_ref()
                    .ok_or("no active signed session")?
                    .faucet,
            )
        };
        let mut last_sent = HashMap::<Address, Instant>::new();
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut poll_cursor = 0usize;
        loop {
            let (targets, destination, status) = {
                let state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
                if !state.running || state.run_id != run_id {
                    return Err("cluster stopped while waiting for claims".into());
                }
                (
                    state
                        .nodes
                        .iter()
                        .filter_map(|(&id, node)| {
                            (node.state != "dead").then_some((id, node.address?))
                        })
                        .collect::<Vec<_>>(),
                    resolve_target(&state, &TargetNode::Primary),
                    best_status(&state).cloned(),
                )
            };
            let confirmed = |status: &NodeStatus, wallet: &WalletAccount| {
                status
                    .account_nonces
                    .iter()
                    .any(|account| account.account_id == wallet.address && account.nonce >= 1)
                    && status.agent_balances.iter().any(|balance| {
                        balance.account_id == wallet.address
                            && balance.usd_available >= faucet.usd_per_claim
                            && balance.btc_available >= faucet.btc_per_claim
                    })
            };
            if let Some(status) = &status {
                let done = wallets
                    .iter()
                    .filter(|wallet| confirmed(status, wallet))
                    .count();
                if let Ok(mut state) = self.inner.lock() {
                    if let Some(progress) = state.claim_progress.as_mut() {
                        progress.confirmed = done;
                    }
                }
                if done == wallets.len() {
                    let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
                    state.pending_claims.clear();
                    state.log(format!("{} participant claims committed", wallets.len()));
                    return Ok(wallets);
                }
                if let Some(outcome) = status.recent_transaction_outcomes.iter().find(|outcome| {
                    !outcome.success
                        && wallets
                            .iter()
                            .any(|wallet| wallet.address == outcome.account_id)
                }) {
                    return Err(format!(
                        "claim for {} failed: {}",
                        outcome.account_id,
                        outcome.error.as_deref().unwrap_or("unknown error")
                    ));
                }
            }
            if let Some((_, address)) = destination {
                for wallet in &wallets {
                    if status
                        .as_ref()
                        .is_some_and(|status| confirmed(status, wallet))
                    {
                        continue;
                    }
                    if last_sent
                        .get(&wallet.address)
                        .is_some_and(|at| at.elapsed() < Duration::from_secs(2))
                    {
                        continue;
                    }
                    let tx = Transaction::ClaimAirdrop {
                        account_id: wallet.address,
                        nonce: 0,
                    };
                    match send_signed_transaction(&self.inner, address, tx, &auth_key) {
                        Ok(()) => {
                            last_sent.insert(wallet.address, Instant::now());
                            let mut state =
                                self.inner.lock().map_err(|_| "controller lock poisoned")?;
                            state.submitted += 1;
                            state.submit_times.push_back(Instant::now());
                        }
                        Err(error) => log(
                            &self.inner,
                            format!(
                                "Claim submission for wallet #{} failed; retrying: {error}",
                                wallet.label
                            ),
                        ),
                    }
                }
            }
            for offset in 0..targets.len() {
                let (node_id, address) = targets[(poll_cursor + offset) % targets.len()];
                let started = Instant::now();
                let Ok(status) = request_status(address, &auth_key, Duration::from_millis(400))
                else {
                    continue;
                };
                update_status(&self.inner, run_id, node_id, started, Ok(status));
                poll_cursor = poll_cursor.wrapping_add(offset + 1);
                break;
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "claim confirmation timed out; {} wallets remain retryable",
                    wallets.len()
                ));
            }
            thread::sleep(Duration::from_millis(200));
        }
    }

    pub fn airdrop_new_address(
        &self,
        from_label: u64,
        asset: u64,
        amount: u64,
        target: TargetNode,
    ) -> Result<WalletAccount, String> {
        if amount == 0 {
            return Err("airdrop amount must be positive".into());
        }
        let (from, nonce, destination, auth_key) = {
            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            if !state.running {
                return Err("start the simulation before airdropping".into());
            }
            let from = slot_address(&state, from_label).ok_or("unknown funding wallet label")?;
            if state
                .market_maker
                .as_ref()
                .is_some_and(|mm| (mm.active || mm.stopping) && mm.config.account_id == from_label)
                || state
                    .agents
                    .values()
                    .any(|agent| agent.config.account_id == from_label)
                || state
                    .load
                    .as_ref()
                    .is_some_and(|load| load_uses_account(&load.config.preset, from_label))
            {
                return Err(
                    "funding wallet is controlled by an active participant or load sender".into(),
                );
            }
            let status = best_status(&state).ok_or("wait for a responsive node")?;
            let nonce = status
                .account_nonces
                .iter()
                .find(|account| account.account_id == from)
                .map(|account| account.nonce)
                .ok_or("funding address is not on chain")?;
            let available = status
                .agent_balances
                .iter()
                .find(|balance| balance.account_id == from)
                .map(|balance| {
                    if asset == 0 {
                        balance.usd_available
                    } else if asset == 1 {
                        balance.btc_available
                    } else {
                        0
                    }
                })
                .ok_or("funding balance unavailable")?;
            if available < amount {
                return Err("funding address has insufficient available balance".into());
            }
            let destination = resolve_target(&state, &target).ok_or("no available target node")?;
            if let Some((previous_nonce, digest)) = state.pending_airdrops.get(&from) {
                let failed = digest.as_ref().is_some_and(|digest| {
                    status
                        .recent_transaction_outcomes
                        .iter()
                        .any(|outcome| outcome.digest == *digest && !outcome.success)
                });
                if nonce <= *previous_nonce && !failed {
                    return Err(
                        "an earlier airdrop from this wallet is still awaiting confirmation".into(),
                    );
                }
            }
            state.pending_airdrops.insert(from, (nonce, None));
            (from, nonce, destination, state.auth_key)
        };
        let created = match self.create_address() {
            Ok(created) => created,
            Err(error) => {
                self.inner.lock().unwrap().pending_airdrops.remove(&from);
                return Err(error);
            }
        };
        let transaction = Transaction::Transfer {
            from,
            to: created.address,
            asset,
            amount,
            nonce,
        };
        if let Err(error) =
            send_signed_transaction(&self.inner, destination.1, transaction.clone(), &auth_key)
        {
            self.inner.lock().unwrap().pending_airdrops.remove(&from);
            return Err(format!(
                "wallet #{} was created, but funding submission failed: {error}",
                created.label
            ));
        }
        let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
        state
            .pending_airdrops
            .insert(from, (nonce, Some(hex_encode(&transaction.to_digest()))));
        state.submitted += 1;
        state.submit_times.push_back(Instant::now());
        state.log(format!("Airdrop submitted: wallet #{from_label} -> #{} ({}) amount {amount} asset {asset}, nonce {nonce}, tx {}", created.label, created.address, hex_encode(&transaction.to_digest())));
        Ok(created)
    }

    /// Fund a controller-owned address and wait for the transfer to commit.
    /// Serial confirmation lets a batch reuse a donor without guessing nonces.
    pub fn fund_address(
        &self,
        from_label: u64,
        to: Address,
        asset: u64,
        amount: u64,
        target: TargetNode,
    ) -> Result<(), String> {
        if amount == 0 || asset > 1 {
            return Err("funding requires a positive USD or BTC amount".into());
        }
        let (from, nonce, expected_received, destination, auth_key, run_id) = {
            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            if !state.running {
                return Err("start the simulation before funding addresses".into());
            }
            if !state.wallet.values().any(|address| *address == to) {
                return Err("recipient is not in the controller wallet".into());
            }
            let from = slot_address(&state, from_label).ok_or("unknown funding wallet label")?;
            if from == to {
                return Err("funding wallet and recipient must differ".into());
            }
            if state
                .market_maker
                .as_ref()
                .is_some_and(|mm| (mm.active || mm.stopping) && mm.config.account_id == from_label)
                || state
                    .agents
                    .values()
                    .any(|agent| agent.config.account_id == from_label)
                || state
                    .load
                    .as_ref()
                    .is_some_and(|load| load_uses_account(&load.config.preset, from_label))
            {
                return Err(
                    "funding wallet is controlled by an active participant or load sender".into(),
                );
            }
            let status = best_status(&state).ok_or("wait for a responsive node")?;
            let nonce = status
                .account_nonces
                .iter()
                .find(|account| account.account_id == from)
                .map(|account| account.nonce)
                .ok_or("funding address is not on chain")?;
            let available = status
                .agent_balances
                .iter()
                .find(|balance| balance.account_id == from)
                .map(|balance| {
                    if asset == 0 {
                        balance.usd_available
                    } else {
                        balance.btc_available
                    }
                })
                .ok_or("funding balance unavailable")?;
            if available < amount {
                return Err(format!(
                    "wallet #{from_label} has insufficient {}",
                    if asset == 0 { "USD" } else { "BTC" }
                ));
            }
            let recipient_before = status
                .agent_balances
                .iter()
                .find(|balance| balance.account_id == to)
                .map_or(0, |balance| {
                    if asset == 0 {
                        balance.usd_available
                    } else {
                        balance.btc_available
                    }
                });
            let expected_received = recipient_before
                .checked_add(amount)
                .ok_or("recipient balance would overflow")?;
            if let Some((previous_nonce, digest)) = state.pending_airdrops.get(&from) {
                let failed = digest.as_ref().is_some_and(|digest| {
                    status
                        .recent_transaction_outcomes
                        .iter()
                        .any(|outcome| outcome.digest == *digest && !outcome.success)
                });
                if nonce <= *previous_nonce && !failed {
                    return Err("an earlier funding transfer from this wallet is still awaiting confirmation".into());
                }
            }
            let destination = resolve_target(&state, &target).ok_or("no available target node")?;
            state.pending_airdrops.insert(from, (nonce, None));
            (
                from,
                nonce,
                expected_received,
                destination,
                state.auth_key,
                state.run_id,
            )
        };
        let transaction = Transaction::Transfer {
            from,
            to,
            asset,
            amount,
            nonce,
        };
        let digest = hex_encode(&transaction.to_digest());
        if let Err(error) =
            send_signed_transaction(&self.inner, destination.1, transaction, &auth_key)
        {
            self.inner.lock().unwrap().pending_airdrops.remove(&from);
            return Err(format!("funding submission failed: {error}"));
        }
        {
            let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
            state
                .pending_airdrops
                .insert(from, (nonce, Some(digest.clone())));
            state.submitted += 1;
            state.submit_times.push_back(Instant::now());
            state.log(format!("Funding submitted: wallet #{from_label} -> {to}, amount {amount} asset {asset}, nonce {nonce}, tx {digest}"));
        }
        let deadline = Instant::now() + Duration::from_secs(45);
        let mut poll_cursor = 0usize;
        loop {
            let targets = {
                let state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
                if !state.running || state.run_id != run_id {
                    return Err("cluster stopped while waiting for funding".into());
                }
                state
                    .nodes
                    .iter()
                    .filter_map(|(&id, node)| (node.state != "dead").then_some((id, node.address?)))
                    .collect::<Vec<_>>()
            };
            for offset in 0..targets.len() {
                let (node_id, address) = targets[(poll_cursor + offset) % targets.len()];
                let started = Instant::now();
                let Ok(status) = request_status(address, &auth_key, Duration::from_millis(400))
                else {
                    continue;
                };
                let outcome = status
                    .recent_transaction_outcomes
                    .iter()
                    .find(|outcome| outcome.digest == digest);
                let confirmed = outcome.is_some_and(|outcome| outcome.success)
                    || (status
                        .account_nonces
                        .iter()
                        .any(|account| account.account_id == from && account.consumed(nonce))
                        && status.agent_balances.iter().any(|balance| {
                            balance.account_id == to
                                && if asset == 0 {
                                    balance.usd_available >= expected_received
                                } else {
                                    balance.btc_available >= expected_received
                                }
                        }));
                let failure = outcome.filter(|outcome| !outcome.success).map(|outcome| {
                    outcome
                        .error
                        .clone()
                        .unwrap_or_else(|| "transaction failed".into())
                });
                update_status(&self.inner, run_id, node_id, started, Ok(status));
                if let Some(error) = failure {
                    self.inner.lock().unwrap().pending_airdrops.remove(&from);
                    return Err(format!("funding transfer failed: {error}"));
                }
                if confirmed {
                    let mut state = self.inner.lock().map_err(|_| "controller lock poisoned")?;
                    if state.run_id != run_id {
                        return Err("cluster stopped while waiting for funding".into());
                    }
                    state.pending_airdrops.remove(&from);
                    state.log(format!("Funding committed: wallet #{from_label} -> {to}, amount {amount} asset {asset}"));
                    return Ok(());
                }
                poll_cursor = poll_cursor.wrapping_add(offset + 1);
                break;
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "funding confirmation timed out for {to}; do not retry this donor until the transfer outcome is known"
                ));
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
}
