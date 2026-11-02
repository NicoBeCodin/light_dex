use super::*;
use std::collections::BTreeSet;
impl<C: Consensus> Node<C> {
    pub(super) fn status_metadata(&mut self) -> NodeStatus {
        let average_commit_latency_ms = if self.metrics.commit_latency_samples == 0 {
            0.0
        } else {
            self.metrics.commit_latency_sum_ms as f64 / self.metrics.commit_latency_samples as f64
        };

        let recent_blocks = self
            .state
            .blocks
            .iter()
            .rev()
            .take(10)
            .map(|block| BlockSummary {
                sequence: block.sequence,

                transaction_count: block.transactions.len(),

                digest: block
                    .to_digest()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            })
            .collect();

        let mut feed_keys = self.feed_urls.keys().copied().collect::<Vec<_>>();
        feed_keys.sort_unstable();
        let mut oracle_pairs = self
            .state
            .exchange_state
            .markets()
            .values()
            .map(|market| OraclePair::new(market.base_asset, market.quote_asset))
            .collect::<BTreeSet<_>>();
        oracle_pairs.extend(feed_keys.iter().map(|(pair, _)| *pair));

        NodeStatus {
            node_id: self.node_id,
            pid: self.pid,
            chain_id: self
                .signing
                .as_ref()
                .map(|(manifest, _)| manifest.chain_id.clone())
                .unwrap_or_default(),
            signing_enabled: self.signing.is_some(),
            authentication_rejections: self.metrics.authentication_rejections,
            faucet_remaining_claims: self.state.exchange_state.faucet_remaining_claims(),
            market_data: feed_keys
                .into_iter()
                .map(|(pair, venue)| {
                    let quote = self.market_data_quotes.get(&(pair, venue));
                    crate::oracle::VenueFeedStatus {
                        pair,
                        venue,
                        symbol: quote.map(|(value, _)| value.symbol.clone()),
                        bid: quote.map(|(value, _)| value.bid.clone()),
                        ask: quote.map(|(value, _)| value.ask.clone()),
                        age_ms: quote.map(|(_, received)| received.elapsed().as_millis() as u64),
                        source_sequence: quote.and_then(|(value, _)| value.source_sequence),
                    }
                })
                .collect(),
            provisional_index_usd_e8: crate::oracle::reference_index_e8(
                OraclePair::new(crate::exchange::market::BTC, crate::exchange::market::USD),
                &self.market_data_quotes,
            ),
            oracle_prices: oracle_pairs
                .into_iter()
                .map(|pair| {
                    let committed = self.state.exchange_state.oracle_mark(pair);
                    OraclePriceStatus {
                        pair,
                        provisional_index_usd_e8: crate::oracle::reference_index_e8(
                            pair,
                            &self.market_data_quotes,
                        ),
                        committed_mark_usd_e8: committed.map(|mark| mark.index_e8),
                        update_digest: committed
                            .map(|mark| crate::message::hex_encode(&mark.update_digest)),
                    }
                })
                .collect(),
            perp_risk: Vec::new(),
            perp_positions: Vec::new(),
            funding: self.state.exchange_state.funding_snapshots(),
            insurance_usd_atoms: self
                .state
                .exchange_state
                .insurance_balance(crate::exchange::market::USD),
            bad_debt_usd_atoms: self.state.exchange_state.bad_debt_usd(),

            network_ready: self.network.is_ready(),

            peer_count: self.network.peers_stream.len(),

            expected_peers: self.network.peers_address.len(),
            peer_queued_bytes: self
                .network
                .peers_stream
                .values()
                .chain(self.network.bulk_peers_stream.values())
                .map(crate::network_writer::PeerWriter::queued_bytes)
                .sum(),
            peer_write_timeouts: self
                .network
                .transport_stats
                .write_timeouts
                .load(std::sync::atomic::Ordering::Relaxed),
            peer_write_errors: self
                .network
                .transport_stats
                .write_errors
                .load(std::sync::atomic::Ordering::Relaxed),
            peer_queue_full: self
                .network
                .transport_stats
                .queue_full
                .load(std::sync::atomic::Ordering::Relaxed),
            peer_truncated_frames: self
                .network
                .transport_stats
                .truncated_frames
                .load(std::sync::atomic::Ordering::Relaxed),
            peer_reconnects: self
                .network
                .transport_stats
                .reconnects
                .load(std::sync::atomic::Ordering::Relaxed),

            chain_height: self.state.blocks.len(),

            latest_sequence: self.state.blocks.last().map(|b| b.sequence),

            mempool_size: self.state.mempool.len(),

            seen_transactions: self.state.seen_transactions.len(),

            current_view: self.consensus.current_view(),

            current_primary: self.consensus.current_primary(),

            next_sequence: self.consensus.next_sequence(),

            active_proposal: self.consensus.has_active_proposal(),

            consensus_started: self.consensus.consensus_started(false),

            transactions_received: self.metrics.transactions_received,

            blocks_committed: self.metrics.blocks_committed,

            transactions_committed: self.metrics.transactions_committed,
            order_actions_committed: self.metrics.order_actions_committed,
            transactions_failed: self.metrics.transactions_failed,
            trades_committed: self.metrics.trades_committed,

            average_commit_latency_ms,

            last_commit_latency_ms: self.metrics.last_commit_latency_ms,
            recent_commit_latency_ms: self
                .metrics
                .recent_commit_latency_ms
                .iter()
                .copied()
                .collect(),
            catch_up_events: self.metrics.catch_up_events,
            blocks_caught_up: self.metrics.blocks_caught_up,
            branch_sync_attempts: self.metrics.branch_sync_attempts,
            branch_sync_failures: self.metrics.branch_sync_failures,
            proposal_parent_misses: self.metrics.proposal_parent_misses,

            recent_blocks,
            markets: {
                let mut markets = self
                    .state
                    .exchange_state
                    .markets()
                    .values()
                    .map(|market| MarketSnapshot {
                        id: market.id,
                        config: market.config(),
                    })
                    .collect::<Vec<_>>();
                markets.sort_by_key(|market| market.id);
                markets
            },
            account_nonces: Vec::new(),
            agent_balances: Vec::new(),
            recent_transaction_outcomes: Vec::new(),
            order_books: Vec::new(),
            recent_trades: Vec::new(),
            open_orders: Vec::new(),
            total_open_orders: self.state.exchange_state.open_order_count(),
            open_orders_complete: true,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, TS)]
pub struct BlockSummary {
    pub sequence: u64,
    pub transaction_count: usize,
    pub digest: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, TS)]
pub struct MarketSnapshot {
    pub id: crate::exchange::market::MarketId,
    pub config: crate::exchange::market_config::MarketConfig,
}

#[derive(Debug, Serialize, Deserialize, Clone, TS)]
pub struct AccountNonce {
    pub account_id: Address,
    /// Suggested next nonce; lower unused values may still be valid.
    pub nonce: u64,
    pub used_nonces: [u64; 2],
}

impl AccountNonce {
    pub fn consumed(&self, value: u64) -> bool {
        if value >= self.nonce {
            return false;
        }
        let distance = self.nonce - 1 - value;
        distance >= 128
            || self.used_nonces[(distance / 64) as usize] & (1u64 << (distance % 64)) != 0
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, TS)]
pub struct AgentBalance {
    pub account_id: Address,
    pub assets: Vec<AssetBalanceSnapshot>,
    pub usd_available: u64,
    pub usd_locked: u64,
    pub btc_available: u64,
    pub btc_locked: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone, TS)]
pub struct AssetBalanceSnapshot {
    pub asset_id: u64,
    pub available: u64,
    pub locked: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone, TS)]
pub struct TransactionOutcome {
    pub digest: String,
    pub account_id: Address,
    pub nonce: u64,
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, TS)]
pub struct NodeStatus {
    pub node_id: NodeId,
    pub pid: u32,
    pub chain_id: String,
    pub signing_enabled: bool,
    pub authentication_rejections: u64,
    pub faucet_remaining_claims: Option<u64>,
    pub market_data: Vec<crate::oracle::VenueFeedStatus>,
    /// Locally observed only; this index is not yet committed by consensus.
    pub provisional_index_usd_e8: Option<u64>,
    pub oracle_prices: Vec<OraclePriceStatus>,
    pub perp_risk: Vec<crate::exchange::perp::PerpRiskSnapshot>,
    pub perp_positions: Vec<crate::exchange::perp::PerpPositionSnapshot>,
    pub funding: Vec<crate::exchange::perp::FundingSnapshot>,
    pub insurance_usd_atoms: u64,
    pub bad_debt_usd_atoms: u64,

    pub network_ready: bool,
    pub peer_count: usize,
    pub expected_peers: usize,
    pub peer_queued_bytes: usize,
    pub peer_write_timeouts: u64,
    pub peer_write_errors: u64,
    pub peer_queue_full: u64,
    pub peer_truncated_frames: u64,
    pub peer_reconnects: u64,

    pub chain_height: usize,
    pub latest_sequence: Option<u64>,

    pub mempool_size: usize,
    pub seen_transactions: usize,

    pub current_view: u64,
    pub current_primary: NodeId,
    pub next_sequence: u64,

    pub active_proposal: bool,
    pub consensus_started: bool,

    pub transactions_received: u64,
    pub blocks_committed: u64,
    pub transactions_committed: u64,
    /// Successful order instructions, including idempotent missing cancels.
    pub order_actions_committed: u64,
    pub transactions_failed: u64,
    pub trades_committed: u64,

    pub average_commit_latency_ms: f64,
    pub last_commit_latency_ms: Option<u128>,
    pub recent_commit_latency_ms: Vec<u64>,
    pub catch_up_events: u64,
    pub blocks_caught_up: u64,
    pub branch_sync_attempts: u64,
    pub branch_sync_failures: u64,
    pub proposal_parent_misses: u64,

    pub recent_blocks: Vec<BlockSummary>,
    pub markets: Vec<MarketSnapshot>,
    pub account_nonces: Vec<AccountNonce>,
    pub agent_balances: Vec<AgentBalance>,
    pub recent_transaction_outcomes: Vec<TransactionOutcome>,
    pub order_books: Vec<OrderBookSnapshot>,
    pub recent_trades: Vec<Trade>,
    pub open_orders: Vec<OpenOrderSnapshot>,
    pub total_open_orders: usize,
    pub open_orders_complete: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, TS)]
pub struct OraclePriceStatus {
    pub pair: OraclePair,
    pub provisional_index_usd_e8: Option<u64>,
    pub committed_mark_usd_e8: Option<u64>,
    pub update_digest: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, TS)]
pub struct OpenOrderSnapshot {
    pub order_id: u64,
    pub account_id: Address,
    pub market_id: u64,
    pub side: crate::exchange::order::Side,
    pub price: u64,
    pub remaining_quantity: u64,
}
