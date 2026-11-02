use super::*;
use std::time::Instant;

mod pool;
pub use pool::Mempool;

impl BlockchainState {
    /// Accepted pending or successfully committed; failed receipts are retryable.
    pub fn knows_transaction(&self, digest: &Digest) -> bool {
        self.seen_transactions.contains(digest)
    }
    pub fn has_pending_signed_transaction(&self, digest: &Digest) -> bool {
        self.signed_mempool.contains_key(digest)
    }
    pub const MAX_MEMPOOL_TRANSACTIONS: usize = 16_384;
    pub const MAX_PENDING_PER_ACCOUNT: usize = 128;
    pub const MAX_MEMPOOL_BYTES: usize = 32 * 1024 * 1024;
    pub const MAX_MEMPOOL_ACTIONS: usize = 65_536;
    pub const MAX_PENDING_ACTIONS_PER_ACCOUNT: usize = 512;
    /// Local proposal budget, not a consensus validity rule. Bound execution
    /// work per proposal so a burst cannot monopolize the node owner thread.
    pub const MAX_PROPOSAL_TRANSACTIONS: usize = 512;

    pub fn remove_included_txs_from_mempool(&mut self, block: &Block) {
        let mut affected = HashSet::new();
        for tx in &block.transactions {
            self.remove_pending(&tx.to_digest());
            affected.insert(tx.account_id());
            if let Transaction::Transfer { to, .. } = tx {
                affected.insert(*to);
            }
        }
        // Only accounts touched by this block can have newly stale nonces.
        // A failed included transaction is removed but can be resubmitted.
        for account in affected {
            if let Some(wallet) = self.exchange_state.accounts().get(&account) {
                for digest in self.mempool.stale_for(wallet) {
                    self.remove_pending(&digest);
                }
            }
        }
    }

    fn remove_pending(&mut self, digest: &Digest) {
        self.mempool.remove(digest);
        self.signed_mempool.remove(digest);
        if !self.committed_transactions.contains(digest) {
            self.seen_transactions.remove(digest);
        }
    }

    pub fn add_transaction(&mut self, tx: &Transaction) -> bool {
        if self.manifest.is_some() {
            return false;
        }
        self.add_transaction_verified(tx)
    }

    pub fn add_signed_transaction(&mut self, signed: &SignedTransaction) -> bool {
        let Some(manifest) = &self.manifest else {
            return false;
        };
        if verify_transaction(signed, manifest).is_err() {
            return false;
        }
        if !self.add_transaction_verified(&signed.transaction) {
            return false;
        }
        self.signed_mempool
            .insert(signed.transaction.to_digest(), signed.signature.clone());
        true
    }

    pub fn signed_transaction(&self, tx: &Transaction) -> Option<SignedTransaction> {
        self.signed_mempool
            .get(&tx.to_digest())
            .map(|signature| SignedTransaction {
                transaction: tx.clone(),
                signature: signature.clone(),
            })
    }

    pub fn pending_signed_transaction(&self, digest: &Digest) -> Option<SignedTransaction> {
        Some(SignedTransaction {
            transaction: self.mempool.get(digest)?.clone(),
            signature: self.signed_mempool.get(digest)?.clone(),
        })
    }

    fn add_transaction_verified(&mut self, tx: &Transaction) -> bool {
        if tx.validate_shape().is_err() {
            return false;
        }
        let Ok(encoded) = serde_json::to_vec(tx) else {
            return false;
        };
        if encoded.len() > crate::exchange::limits::MAX_TRANSACTION_BYTES {
            return false;
        }
        let admissible = match tx {
            Transaction::ClaimAirdrop { account_id, nonce } => {
                *nonce == 0
                    && !self.exchange_state.accounts().contains_key(account_id)
                    && self
                        .exchange_state
                        .faucet_remaining_claims()
                        .is_some_and(|remaining| remaining > 0)
            }
            _ => self
                .exchange_state
                .accounts()
                .get(&tx.account_id())
                .is_some_and(|account| account.accepts_nonce(tx.nonce())),
        };
        if !admissible {
            return false;
        }
        if self.mempool.has_nonce(tx.account_id(), tx.nonce()) {
            return false;
        }
        let digest = tx.to_digest();
        if self.seen_transactions.contains(&digest) {
            return false;
        }
        let actions = Mempool::action_cost(tx);
        while self.mempool.len() >= Self::MAX_MEMPOOL_TRANSACTIONS
            || self.mempool.account_len(tx.account_id()) >= Self::MAX_PENDING_PER_ACCOUNT
            || self.mempool.queued_bytes() + encoded.len() > Self::MAX_MEMPOOL_BYTES
            || self.mempool.queued_actions() + actions > Self::MAX_MEMPOOL_ACTIONS
            || self.mempool.account_actions(tx.account_id()) + actions
                > Self::MAX_PENDING_ACTIONS_PER_ACCOUNT
        {
            // A full queue must still admit its missing earlier nonce. Evict
            // only this sender's latest future entry; it can be retried later.
            let Some(later) = self.mempool.later_entry(tx.account_id(), tx.nonce()) else {
                return false;
            };
            self.remove_pending(&later);
        }
        self.seen_transactions.insert(digest);
        self.mempool.insert(tx.clone(), digest, encoded.len());
        true
    }

    pub fn build_block(&mut self) -> Option<Block> {
        self.build_block_with_oracle(Vec::new())
    }

    pub fn build_block_with_oracle(&mut self, oracle_updates: Vec<OracleUpdate>) -> Option<Block> {
        let previous = self.blocks.last()?.clone();
        let exchange = self.exchange_state.fork();
        self.build_block_on(&previous, &exchange, oracle_updates, false)
    }

    /// Build on a certified, not-yet-committed parent without copying the
    /// whole chain or the mempool. The parent exchange is structurally shared.
    pub fn build_block_on(
        &mut self,
        previous_block: &Block,
        parent_exchange: &ExchangeState,
        oracle_updates: Vec<OracleUpdate>,
        allow_empty: bool,
    ) -> Option<Block> {
        let total_started = Instant::now();
        let select_started = Instant::now();
        // Scan beyond the block cap so a failed account does not crowd out
        // independent senders in the same proposal.
        let selected = self
            .mempool
            .select_with_bytes(parent_exchange, Self::MAX_PROPOSAL_TRANSACTIONS * 4);
        crate::profile::record(
            "proposal_select",
            select_started.elapsed(),
            selected.len() as u64,
        );
        if selected.is_empty() && oracle_updates.is_empty() && !allow_empty {
            return None;
        }
        if oracle_updates.len() > 32 || oracle_updates.windows(2).any(|w| w[0].pair >= w[1].pair) {
            return None;
        }

        let parent_root = parent_exchange.state_digest();
        let mut exchange = parent_exchange.fork();
        exchange.begin_state_batch();
        let oracle_started = Instant::now();
        for update in &oracle_updates {
            exchange.apply_oracle_update(update).ok()?;
        }
        crate::profile::record(
            "proposal_oracle",
            oracle_started.elapsed(),
            oracle_updates.len() as u64,
        );
        let mut transactions = Vec::with_capacity(selected.len());
        let mut outcomes = Vec::with_capacity(selected.len());
        let mut work = 0usize;
        let mut failed_accounts = HashSet::new();
        // Leave conservative room for metadata/oracle observations/signatures.
        let mut bytes = 64 * 1024usize;
        let prefetch_distance = crate::exchange::prefetch::distance();
        let execute_started = Instant::now();
        let timing = crate::profile::enabled();
        let mut fork_time = std::time::Duration::ZERO;
        let mut transaction_time = std::time::Duration::ZERO;
        let mut publish_time = std::time::Duration::ZERO;
        for (index, (tx, encoded)) in selected.iter().enumerate() {
            if transactions.len() >= Self::MAX_PROPOSAL_TRANSACTIONS {
                break;
            }
            if failed_accounts.contains(&tx.account_id()) {
                continue;
            }
            if prefetch_distance > 0 {
                if let Some((next, _)) = selected.get(index + prefetch_distance) {
                    exchange.prefetch_transaction(next);
                }
            }
            if tx.validate_shape().is_err() {
                continue;
            }
            if *encoded > crate::exchange::limits::MAX_TRANSACTION_BYTES {
                continue;
            }
            if bytes + *encoded + 256 > crate::exchange::limits::MAX_BLOCK_BYTES {
                break;
            }
            let stage_started = timing.then(Instant::now);
            let mut candidate = exchange.fork();
            if let Some(started) = stage_started {
                fork_time += started.elapsed();
            }
            let stage_started = timing.then(Instant::now);
            let outcome = candidate.execute_with_receipt(&tx);
            if outcome.is_err() {
                failed_accounts.insert(tx.account_id());
            }
            if let Some(started) = stage_started {
                transaction_time += started.elapsed();
            }
            let next_work = work.saturating_add(candidate.last_execution_work());
            if next_work > crate::exchange::limits::MAX_BLOCK_WORK {
                break;
            }
            work = next_work;
            bytes += *encoded + 256;
            let stage_started = timing.then(Instant::now);
            exchange = candidate;
            if let Some(started) = stage_started {
                publish_time += started.elapsed();
            }
            outcomes.push(outcome);
            transactions.push(tx.clone());
        }
        crate::profile::record("proposal_fork", fork_time, transactions.len() as u64);
        crate::profile::record(
            "proposal_transaction",
            transaction_time,
            transactions.len() as u64,
        );
        crate::profile::record("proposal_publish", publish_time, transactions.len() as u64);
        crate::profile::record(
            "proposal_execute",
            execute_started.elapsed(),
            transactions.len() as u64,
        );
        let materialize_started = Instant::now();
        exchange.finish_state_batch();
        let state_root = exchange.state_digest();
        crate::profile::record(
            "proposal_state_root",
            materialize_started.elapsed(),
            transactions.len() as u64,
        );
        let finalize_started = Instant::now();
        let authentication = self.manifest.as_ref().map(|manifest| BlockAuthentication {
            version: PROTOCOL_VERSION,
            chain_id: manifest.chain_id.clone(),
            signatures: transactions
                .iter()
                .map(|tx| {
                    self.signed_mempool
                        .get(&tx.to_digest())
                        .cloned()
                        .expect("verified mempool signature missing")
                })
                .collect(),
        });
        let block = Block {
            transactions,
            oracle_updates,
            sequence: previous_block.sequence + 1,
            previous_hash: previous_block.to_digest(),
            state_root,
            state_root_version: if self.manifest.is_some() {
                ADDRESS_STATE_ROOT_VERSION
            } else {
                MERKLE_STATE_ROOT_VERSION
            },
            authentication,
        };
        // The running estimate reserves room for normal oracle messages, but
        // only exact serialization defines the protocol limit.
        limits::validate(&block).ok()?;
        // Keep the exact speculative result: the proposer need not execute it
        // again at validation/commit. Commit still checks its certificate and
        // persists successfully before publishing this state.
        self.prepared_block = Some((
            block.to_digest(),
            parent_root,
            PreparedBlock { exchange, outcomes },
        ));
        crate::profile::record(
            "proposal_finalize",
            finalize_started.elapsed(),
            block.transactions.len() as u64,
        );
        crate::profile::record(
            "proposal_total",
            total_started.elapsed(),
            block.transactions.len() as u64,
        );
        Some(block)
    }

    pub fn build_executable_transactions(&self) -> Vec<Transaction> {
        self.mempool
            .select(&self.exchange_state, Self::MAX_PROPOSAL_TRANSACTIONS)
    }
}
