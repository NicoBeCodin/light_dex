//! Immutable, height-consistent state handed to read workers. Persistent
//! exchange maps share untouched paths; no JSON projection runs on the owner.
use super::*;
use std::collections::BTreeSet;

pub struct ReadSnapshot {
    pub node_id: NodeId,
    pub height: u64,
    pub digest: Option<String>,
    pub exchange: crate::exchange::ExchangeState,
    pub trades: Vec<Trade>,
    pub outcomes: Vec<TransactionOutcome>,
    pub view: u64,
    pub primary: NodeId,
    pub mempool: usize,
    pub peers: usize,
    pub transactions: u64,
    pub blocks: u64,
}

impl<C: Consensus> Node<C> {
    pub(in crate::node) fn read_snapshot(&mut self, fresh: bool) -> Arc<ReadSnapshot> {
        let height = self.state.blocks.len() as u64;
        if !fresh {
            if let Some(snapshot) = &self.published_read {
                if snapshot.height == height {
                    return snapshot.clone();
                }
            }
        }
        let snapshot = Arc::new(ReadSnapshot {
            node_id: self.node_id,
            height,
            digest: self
                .state
                .blocks
                .last()
                .map(|block| crate::message::hex_encode(&block.to_digest())),
            exchange: self.state.exchange_state.fork(),
            trades: self.state.recent_trades.iter().cloned().collect(),
            outcomes: self.recent_transaction_outcomes.iter().cloned().collect(),
            view: self.consensus.current_view(),
            primary: self.consensus.current_primary(),
            mempool: self.state.mempool.len(),
            peers: self.network.peers_address.len(),
            transactions: self.metrics.transactions_committed,
            blocks: self.metrics.blocks_committed,
        });
        self.published_read = Some(snapshot.clone());
        snapshot
    }
}

#[derive(Debug, Default)]
pub struct ReadUpdate {
    pub height: u64,
    pub markets: BTreeSet<u64>,
    pub accounts: BTreeSet<Address>,
    pub oracle_markets: BTreeSet<u64>,
}
impl ReadUpdate {
    pub fn new(block: &Block, exchange: &crate::exchange::ExchangeState) -> Self {
        let mut update = Self {
            height: block.sequence + 1,
            ..Self::default()
        };
        for oracle in &block.oracle_updates {
            for market in exchange.markets().values().filter(|market| {
                market.base_asset == oracle.pair.base_asset
                    && market.quote_asset == oracle.pair.quote_asset
            }) {
                update.markets.insert(market.id);
                update.oracle_markets.insert(market.id);
            }
        }
        update
    }
    pub fn observe(
        &mut self,
        tx: &Transaction,
        result: &Result<Vec<Trade>, crate::exchange::ExchangeError>,
        exchange: &crate::exchange::ExchangeState,
    ) {
        self.accounts.insert(tx.account_id());
        if let Transaction::Transfer { to, .. } = tx {
            self.accounts.insert(*to);
        }
        if let Some(market) = tx.market_id() {
            self.markets.insert(market);
        }
        if let Transaction::LiquidatePerp { target, .. } = tx {
            self.accounts.insert(*target);
            // Cross liquidation can cancel the target's quotes in other books.
            self.markets.extend(exchange.markets().keys());
        }
        if let Ok(trades) = result {
            for trade in trades {
                self.accounts.insert(trade.maker_account);
                self.accounts.insert(trade.taker_account);
                self.markets.insert(trade.market_id);
            }
        }
    }
    pub fn affects(&self, query: ReadQuery, dependencies: &BTreeSet<u64>) -> bool {
        match query {
            ReadQuery::Summary => true,
            ReadQuery::Market(id) => self.markets.contains(&id),
            ReadQuery::Account(owner) => {
                self.accounts.contains(&owner) || !self.oracle_markets.is_disjoint(dependencies)
            }
            // Cross-margin candidates depend on marks in other markets too.
            ReadQuery::Candidates(_) | ReadQuery::CandidatesAll => !self.markets.is_empty(),
        }
    }
}
