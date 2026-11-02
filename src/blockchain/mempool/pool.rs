use super::*;
use std::collections::BTreeMap;

/// Admission assigns a stable arrival ID. Account indexes bound duplicate
/// nonces and make commit cleanup independent of unrelated transactions.
#[derive(Debug, Default)]
pub struct Mempool {
    arrivals: BTreeMap<u64, PendingTransaction>,
    digests: HashMap<Digest, u64>,
    accounts: HashMap<AccountId, BTreeMap<(u64, u64), Digest>>,
    next_arrival: u64,
    bytes: usize,
    actions: usize,
    account_actions: HashMap<AccountId, usize>,
}

#[derive(Debug)]
struct PendingTransaction {
    transaction: Transaction,
    digest: Digest,
    bytes: usize,
    actions: usize,
}

impl Mempool {
    pub fn queued_bytes(&self) -> usize {
        self.bytes
    }
    pub fn queued_actions(&self) -> usize {
        self.actions
    }
    pub(super) fn account_actions(&self, id: AccountId) -> usize {
        self.account_actions.get(&id).copied().unwrap_or(0)
    }
    pub(super) fn action_cost(tx: &Transaction) -> usize {
        match tx {
            Transaction::UpdateQuotes {
                cancel,
                reduce,
                place,
                ..
            } => 1 + cancel.len() + reduce.len() + place.len(),
            _ => 1,
        }
    }
    pub fn len(&self) -> usize {
        self.arrivals.len()
    }
    pub fn is_empty(&self) -> bool {
        self.arrivals.is_empty()
    }

    pub(super) fn get(&self, digest: &Digest) -> Option<&Transaction> {
        self.arrivals
            .get(self.digests.get(digest)?)
            .map(|pending| &pending.transaction)
    }

    pub(super) fn account_len(&self, account: AccountId) -> usize {
        self.accounts.get(&account).map_or(0, BTreeMap::len)
    }

    pub(super) fn has_nonce(&self, account: AccountId, nonce: u64) -> bool {
        self.accounts
            .get(&account)
            .is_some_and(|queue| queue.range((nonce, 0)..=(nonce, u64::MAX)).next().is_some())
    }

    pub(super) fn later_entry(&self, account: AccountId, nonce: u64) -> Option<Digest> {
        let (&(last_nonce, _), &digest) = self.accounts.get(&account)?.last_key_value()?;
        (last_nonce > nonce).then_some(digest)
    }

    pub(super) fn insert(&mut self, transaction: Transaction, digest: Digest, bytes: usize) {
        let actions = Self::action_cost(&transaction);
        self.bytes += bytes;
        self.actions += actions;
        *self
            .account_actions
            .entry(transaction.account_id())
            .or_default() += actions;
        let arrival = self.next_arrival;
        self.next_arrival = arrival
            .checked_add(1)
            .expect("mempool arrival ID exhausted");
        self.accounts
            .entry(transaction.account_id())
            .or_default()
            .insert((transaction.nonce(), arrival), digest);
        self.digests.insert(digest, arrival);
        self.arrivals.insert(
            arrival,
            PendingTransaction {
                transaction,
                digest,
                bytes,
                actions,
            },
        );
    }

    pub(super) fn remove(&mut self, digest: &Digest) -> bool {
        let Some(arrival) = self.digests.remove(digest) else {
            return false;
        };
        let pending = self
            .arrivals
            .remove(&arrival)
            .expect("arrival index agrees");
        debug_assert_eq!(&pending.digest, digest);
        let account = pending.transaction.account_id();
        self.bytes -= pending.bytes;
        self.actions -= pending.actions;
        let cost = self
            .account_actions
            .get_mut(&account)
            .expect("account work index");
        *cost -= pending.actions;
        if *cost == 0 {
            self.account_actions.remove(&account);
        }
        let queue = self
            .accounts
            .get_mut(&account)
            .expect("account index agrees");
        queue.remove(&(pending.transaction.nonce(), arrival));
        if queue.is_empty() {
            self.accounts.remove(&account);
        }
        true
    }

    pub(super) fn stale_for(&self, account: &crate::exchange::account::Account) -> Vec<Digest> {
        let account_id = account.id;
        let Some(queue) = self.accounts.get(&account_id) else {
            return Vec::new();
        };
        // Transfers can create an account with nonce zero, invalidating a
        // pending ClaimAirdrop even though its nonce is not below the new nonce.
        queue
            .iter()
            .filter_map(|(&(tx_nonce, arrival), digest)| {
                (!account.accepts_nonce(tx_nonce)
                    || matches!(
                        self.arrivals[&arrival].transaction,
                        Transaction::ClaimAirdrop { .. }
                    ))
                .then_some(*digest)
            })
            .collect()
    }

    fn ready_entries<'a>(
        &'a self,
        exchange: &'a ExchangeState,
    ) -> impl Iterator<Item = &'a PendingTransaction> + 'a {
        self.arrivals.values().filter(|pending| {
            let tx = &pending.transaction;
            match tx {
                Transaction::ClaimAirdrop { account_id, .. } => {
                    !exchange.accounts().contains_key(account_id)
                }
                _ => exchange
                    .accounts()
                    .get(&tx.account_id())
                    .is_some_and(|account| account.accepts_nonce(tx.nonce())),
            }
        })
    }

    fn ready<'a>(
        &'a self,
        exchange: &'a ExchangeState,
    ) -> impl Iterator<Item = &'a Transaction> + 'a {
        self.ready_entries(exchange)
            .map(|pending| &pending.transaction)
    }

    pub(crate) fn ready_count(&self, exchange: &ExchangeState, limit: usize) -> usize {
        self.ready(exchange).take(limit).count()
    }

    pub(super) fn select(&self, exchange: &ExchangeState, limit: usize) -> Vec<Transaction> {
        self.ready(exchange).take(limit).cloned().collect()
    }

    /// Reuse the exact serialized length checked at admission when assembling a
    /// proposal. The transaction itself is still serialized by block encoding.
    pub(super) fn select_with_bytes(
        &self,
        exchange: &ExchangeState,
        limit: usize,
    ) -> Vec<(Transaction, usize)> {
        self.ready_entries(exchange)
            .take(limit)
            .map(|pending| (pending.transaction.clone(), pending.bytes))
            .collect()
    }

    /// Stable cursors cannot skip transactions when earlier entries commit.
    pub(crate) fn forward_batch(&self, from: u64, limit: usize) -> Vec<(u64, Transaction)> {
        self.arrivals
            .range(from..)
            .take(limit)
            .map(|(&arrival, pending)| (arrival, pending.transaction.clone()))
            .collect()
    }

    pub(crate) fn has_arrival_from(&self, from: u64) -> bool {
        self.arrivals.range(from..).next().is_some()
    }

    #[cfg(test)]
    pub(crate) fn assert_indexes(&self) {
        assert_eq!(
            self.bytes,
            self.arrivals.values().map(|p| p.bytes).sum::<usize>()
        );
        assert_eq!(
            self.actions,
            self.arrivals.values().map(|p| p.actions).sum::<usize>()
        );
        assert_eq!(self.actions, self.account_actions.values().sum::<usize>());
        assert_eq!(self.arrivals.len(), self.digests.len());
        assert_eq!(
            self.arrivals.len(),
            self.accounts.values().map(BTreeMap::len).sum::<usize>()
        );
        for (&arrival, pending) in &self.arrivals {
            assert_eq!(
                pending.bytes,
                serde_json::to_vec(&pending.transaction).unwrap().len()
            );
            assert_eq!(self.digests[&pending.digest], arrival);
            assert_eq!(
                self.accounts[&pending.transaction.account_id()]
                    [&(pending.transaction.nonce(), arrival)],
                pending.digest
            );
        }
    }
}
