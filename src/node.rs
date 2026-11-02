use std::{
    collections::{HashMap, VecDeque},
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpStream},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::crypto::{
    SessionManifest, SignedConsensusMessage, SignedTransaction, sign_consensus, verify_consensus,
    verify_transaction,
};
use crate::exchange::{ExchangeState, address::Address};
use crate::oracle::{OraclePair, Venue, VenueQuote};
use crate::{
    blockchain::{Block, BlockchainState, Digest, Trade, Transaction},
    consensus::{CommitCertificate, Consensus, ConsensusAction, ConsensusEvent},
    exchange::orderbook::OrderBookSnapshot,
    message::{Message, sign_consensus_line, sign_line, verify_line},
    network::{Network, NetworkEvent},
};
use ed25519_dalek::SigningKey;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

mod events;
mod explorer;
mod read_api;
mod status;
mod status_worker;
mod sync;
mod sync_worker;
pub use explorer::ExplorerQuery;
mod batching;
mod gossip;
mod trace;

pub use status::{
    AccountNonce, AgentBalance, BlockSummary, MarketSnapshot, NodeStatus, OpenOrderSnapshot,
    OraclePriceStatus, TransactionOutcome,
};
#[cfg(test)]
use sync::fetch_blocks;

const SYNC_BATCH_SIZE: usize = 32;
// Four full proposal blocks per WAN fetch. The wire response is compressed and
// separately bounded by the 8 MiB authenticated payload limit.
const SYNC_BATCH_TRANSACTIONS: usize = 2048;

#[derive(Debug, Serialize, Deserialize)]
pub struct BlockSyncResponse {
    from_sequence: u64,
    chain_height: u64,
    anchor_digest: Option<Digest>,
    blocks: Vec<Block>,
    #[serde(default)]
    certificates: Vec<Option<CommitCertificate>>,
    #[serde(default)]
    speculative: Vec<crate::consensus::chained::CertifiedBranchEntry>,
}

pub type NodeId = u64;

struct PendingCompactData {
    from: NodeId,
    view: u64,
    height: u64,
    compact: crate::consensus::chained::CompactProposal,
    mode: CompactMode,
    first_seen: Instant,
    next_request: Instant,
}

#[derive(Clone)]
enum CompactMode {
    Chained,
    Hotstuff2 {
        justify: Option<crate::consensus::fast::FastQc>,
    },
}

#[derive(Debug, Default)]
struct NodeMetrics {
    authentication_rejections: u64,
    transactions_received: u64,

    blocks_committed: u64,
    transactions_committed: u64,
    order_actions_committed: u64,
    transactions_failed: u64,
    trades_committed: u64,

    commit_latency_sum_ms: u128,
    commit_latency_samples: u64,

    last_commit_latency_ms: Option<u128>,

    transaction_received_at: HashMap<Digest, Instant>,
    recent_commit_latency_ms: VecDeque<u64>,
    catch_up_events: u64,
    blocks_caught_up: u64,
    branch_sync_attempts: u64,
    branch_sync_failures: u64,
    proposal_parent_misses: u64,
    compact_received: u64,
    compact_reconstructed: u64,
    compact_body_requests: u64,
    adaptive_full_body_sends: u64,
}

pub struct Node<C: Consensus> {
    node_id: NodeId,
    pid: u32,

    network: Network,
    state: BlockchainState,
    consensus: C,
    signing: Option<(Arc<SessionManifest>, SigningKey)>,

    block_time_ms: u64,
    batch_window: Duration,
    proposal_ready_txs: usize,
    batch_deadline: Option<Instant>,
    batch_epoch: u64,
    last_primary_probe: Instant,
    primary_probe_timeout: Duration,
    forward_cursor: Option<u64>,
    last_forward_sweep: Instant,
    last_commit_at: Instant,
    gossip_pending: Vec<SignedTransaction>,
    gossip_epoch: u64,

    metrics: NodeMetrics,
    recent_transaction_outcomes: VecDeque<TransactionOutcome>,
    sync_in_flight: bool,
    pending_branch_peer: Option<NodeId>,
    sync_timeout: Duration,
    last_sync_probe: Instant,
    sync_peer_cursor: usize,
    feed_urls: HashMap<(OraclePair, Venue), String>,
    market_data_stop: Arc<AtomicBool>,
    market_data_quotes: HashMap<(OraclePair, Venue), (VenueQuote, Instant)>,
    read_updates: Option<tokio::sync::broadcast::Sender<Arc<read_api::ReadUpdate>>>,
    published_read: Option<Arc<read_api::ReadSnapshot>>,
    status_writer: Option<status_worker::StatusWriter>,
    sync_writer: Option<sync_worker::SyncWriter>,
    pending_commit: bool,
    deferred_actions: VecDeque<ConsensusAction>,
    journal_write_us: u128,
    journal_writes: u64,
    commit_trace: Option<trace::TraceWriter>,
    stage_trace: Option<trace::StageWriter>,
    speculative: HashMap<Digest, (Block, ExchangeState)>,
    pending_data: HashMap<Digest, (NodeId, crate::consensus::ConsensusMessage, Instant)>,
    pending_fast: VecDeque<(NodeId, crate::consensus::ConsensusMessage, Instant)>,
    pending_compact: HashMap<Digest, PendingCompactData>,
    full_body_peers: HashMap<NodeId, Instant>,
}

pub enum NodeEvent {
    DurableCommit {
        result: Result<crate::blockchain::DurableBlock, String>,
        committed_view: u64,
    },
    Consensus(ConsensusEvent),
    SignedConsensus {
        from: NodeId,
        signed: SignedConsensusMessage,
    },
    AuthenticationRejected,

    Control(ControlEvent),

    NewTransaction(Transaction),
    NewSignedTransaction(SignedTransaction),
    NewClientSignedTransaction(SignedTransaction),
    NewSignedTransactionBatch {
        transactions: Vec<SignedTransaction>,
        prewarm: bool,
    },
    GossipReady {
        epoch: u64,
    },
    SubmitTransaction {
        signed: SignedTransaction,
        stream: TcpStream,
        received: Instant,
    },

    ProduceBlock,
    DataRetry,
    BatchReady {
        epoch: u64,
    },
    MarketData {
        pair: OraclePair,
        quote: VenueQuote,
    },

    CatchUp {
        peer: NodeId,
        result: std::io::Result<BlockSyncResponse>,
    },
    ReadQuery {
        query: read_api::ReadQuery,
        reply: mpsc::Sender<Arc<read_api::ReadSnapshot>>,
    },
    ExplorerQuery {
        query: ExplorerQuery,
        reply: mpsc::Sender<serde_json::Value>,
    },

    Shutdown,
}

impl crate::priority::Prioritized for NodeEvent {
    fn priority(&self) -> bool {
        if matches!(self, Self::SignedConsensus { signed, .. } if signed.message.is_bulk()) {
            return false;
        }
        if matches!(self, Self::Consensus(crate::consensus::ConsensusEvent::Message { message, .. })
            if message.is_bulk())
        {
            return false;
        }
        matches!(
            self,
            Self::DurableCommit { .. }
                | Self::Consensus(_)
                | Self::SignedConsensus { .. }
                | Self::ProduceBlock
                | Self::DataRetry
                | Self::BatchReady { .. }
                | Self::CatchUp { .. }
                | Self::Shutdown
                | Self::Control(ControlEvent::PeerConnected { .. })
        )
    }
}

pub enum ControlEvent {
    PeerConnected {
        node_id: NodeId,
        lane: crate::network::PeerLane,
        tcp_stream: TcpStream,
    },

    Pause {
        millis: u64,
    },

    Status {
        stream: TcpStream,
        accounts: Option<Vec<Address>>,
    },
    Health {
        stream: TcpStream,
    },

    FetchBlocks {
        from_sequence: u64,
        limit: usize,
        want_branch: bool,
        stream: TcpStream,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ControlMessage {
    StatusAccounts {
        accounts: Vec<Address>,
    },
    Hello {
        node_id: NodeId,
    },
    BulkHello {
        node_id: NodeId,
    },

    Pause {
        millis: u64,
    },

    Status,
    Health,

    FetchBlocks {
        from_sequence: u64,
        limit: usize,
        #[serde(default)]
        want_branch: bool,
    },

    Shutdown,
}

impl<C: Consensus> Node<C> {
    pub fn new(
        node_id: NodeId,
        pid: u32,
        network: Network,
        state: BlockchainState,
        mut consensus: C,
        block_time_ms: u64,
    ) -> Result<Node<C>, std::io::Error> {
        consensus.restore_from_height(state.blocks.len() as u64);
        let commit_trace =
            if node_id == 0 && std::env::var("LIGHT_DEX_COMMIT_TRACE").as_deref() == Ok("1") {
                state
                    .trace_path()
                    .map(trace::TraceWriter::new)
                    .transpose()?
            } else {
                None
            };
        let stage_mod = std::env::var("LIGHT_DEX_TX_STAGE_SAMPLE_MOD")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let stage_trace = if stage_mod > 0 {
            state
                .stage_trace_path()
                .map(|path| trace::StageWriter::new(path, stage_mod))
                .transpose()?
        } else {
            None
        };
        Ok(Node {
            node_id,
            pid,

            network,
            state,
            consensus,
            signing: None,

            block_time_ms,
            batch_window: Duration::from_millis(
                std::env::var("LIGHT_DEX_TX_BATCH_WINDOW_MS")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(50)
                    .min(1_000),
            ),
            proposal_ready_txs: std::env::var("LIGHT_DEX_PROPOSAL_READY_TXS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(96)
                .clamp(32, 512),
            batch_deadline: None,
            batch_epoch: 0,
            last_primary_probe: Instant::now(),
            primary_probe_timeout: Duration::from_millis(100),
            forward_cursor: None,
            last_forward_sweep: Instant::now(),
            last_commit_at: Instant::now(),
            gossip_pending: Vec::new(),
            gossip_epoch: 0,

            metrics: NodeMetrics::default(),
            recent_transaction_outcomes: VecDeque::new(),
            sync_in_flight: false,
            pending_branch_peer: None,
            sync_timeout: Duration::from_millis(500),
            last_sync_probe: Instant::now(),
            sync_peer_cursor: 0,
            feed_urls: HashMap::new(),
            market_data_stop: Arc::new(AtomicBool::new(false)),
            market_data_quotes: HashMap::new(),
            read_updates: None,
            published_read: None,
            status_writer: None,
            sync_writer: None,
            pending_commit: false,
            deferred_actions: VecDeque::new(),
            journal_write_us: 0,
            journal_writes: 0,
            commit_trace,
            stage_trace,
            speculative: HashMap::new(),
            pending_data: HashMap::new(),
            pending_fast: VecDeque::new(),
            pending_compact: HashMap::new(),
            full_body_peers: HashMap::new(),
        })
    }

    /// A direct WebSocket URL overrides the shared hub for that venue.
    pub fn configure_market_data(&mut self, feed_urls: HashMap<(OraclePair, Venue), String>) {
        self.feed_urls = feed_urls;
    }

    pub fn attach_signing(
        &mut self,
        manifest: Arc<SessionManifest>,
        key: SigningKey,
    ) -> std::io::Result<()> {
        if self.state.signed_chain_id() != Some(manifest.chain_id.as_str()) {
            return Err(std::io::Error::other(
                "node state and signing session differ",
            ));
        }
        if key.verifying_key() != manifest.verifier(true, self.node_id)? {
            return Err(std::io::Error::other(
                "validator signing key does not match session",
            ));
        }
        self.consensus.set_chain_id(manifest.chain_id.clone());
        self.consensus.set_signer(manifest.clone(), key.clone());
        self.signing = Some((manifest, key));
        Ok(())
    }

    pub fn register_with_parent(&self) -> Result<(), std::io::Error> {
        let mut stream = TcpStream::connect(self.network.parent_address)?;

        let message = format!(
            "REGISTER {} {}\n",
            self.node_id, self.network.advertised_address
        );

        stream.write_all(message.as_bytes())?;

        Ok(())
    }

    pub fn discover_peers(&mut self) -> Result<(), std::io::Error> {
        let (stream, _) = self.network.listener.accept()?;

        let mut reader = BufReader::new(stream);

        loop {
            let mut line = String::new();

            let n = reader.read_line(&mut line)?;

            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "bootstrap connection closed before DONE",
                ));
            }

            let parts: Vec<&str> = line.split_whitespace().collect();

            match parts.first() {
                Some(&"REGISTER") if parts.len() >= 3 => {
                    let node_id: NodeId = parts[1].parse().map_err(|_| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid peer node id")
                    })?;

                    if node_id == self.node_id {
                        continue;
                    }

                    let address: SocketAddr = parts[2].parse().map_err(|_| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid peer address")
                    })?;

                    self.network.peers_address.insert(node_id, address);
                }

                Some(&"DONE") => break,

                _ => {}
            }
        }

        let peer_count = self.network.peers_address.len() as u64;

        self.consensus.set_node_count(peer_count + 1);

        println!("Node {} discovered {} peers", self.node_id, peer_count);

        Ok(())
    }

    pub fn wait_for_start(&self) -> Result<(), std::io::Error> {
        let mut stream = TcpStream::connect(self.network.parent_address)?;

        stream.write_all(format!("READY {}\n", self.node_id).as_bytes())?;

        let mut reader = BufReader::new(stream);

        let mut line = String::new();

        reader.read_line(&mut line)?;

        if line.trim() != "START" {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "expected START from controller",
            ));
        }

        Ok(())
    }

    fn apply_committed_block(
        &mut self,
        block: &Block,
        certificate: Option<CommitCertificate>,
    ) -> Result<(), std::io::Error> {
        let outcomes = match certificate {
            Some(certificate) => self.state.commit_block_with_certificate(block, certificate),
            None => self.state.commit_block_with_receipts(block),
        }
        .map_err(std::io::Error::other)?;
        self.record_committed_block(block, outcomes)
    }

    fn record_committed_block(
        &mut self,
        block: &Block,
        outcomes: Vec<Result<Vec<Trade>, crate::exchange::ExchangeError>>,
    ) -> std::io::Result<()> {
        let committed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        self.trace_block_stage(block, "durable_commit");
        let mut traced = Vec::new();
        let mut traced_outcomes = Vec::new();
        let mut latency_sum = 0u128;
        let mut latency_samples = 0u64;
        let mut successful = 0u64;
        let projection_started = std::time::Instant::now();
        let mut read_update = read_api::ReadUpdate::new(block, &self.state.exchange_state);
        for (tx, result) in block.transactions.iter().zip(outcomes) {
            read_update.observe(tx, &result, &self.state.exchange_state);
            if self.commit_trace.is_some() {
                let digest = crate::message::hex_encode(&tx.to_digest());
                if result.is_ok() {
                    traced.push(digest.clone());
                }
                traced_outcomes.push(trace::TraceOutcome {
                    digest,
                    market_id: tx.market_id(),
                    success: result.is_ok(),
                    fills: result.as_ref().map_or(0, Vec::len),
                    error: result.as_ref().err().map(|error| format!("{error:?}")),
                });
            }
            if let Some(start) = self.metrics.transaction_received_at.remove(&tx.to_digest()) {
                if result.is_ok() {
                    let elapsed_ms = start.elapsed().as_millis();
                    latency_sum += elapsed_ms;
                    latency_samples += 1;
                    self.metrics
                        .recent_commit_latency_ms
                        .push_back(elapsed_ms.min(u64::MAX as u128) as u64);
                    while self.metrics.recent_commit_latency_ms.len() > 1024 {
                        self.metrics.recent_commit_latency_ms.pop_front();
                    }
                }
            }
            successful += u64::from(result.is_ok());
            if result.is_ok() {
                self.metrics.order_actions_committed += match tx {
                    Transaction::UpdateQuotes {
                        cancel,
                        reduce,
                        place,
                        ..
                    } => (cancel.len() + reduce.len() + place.len()) as u64,
                    Transaction::ReduceOrder { .. }
                    | Transaction::PlaceOrder { .. }
                    | Transaction::PlacePerpOrder { .. }
                    | Transaction::CancelOrder { .. } => 1,
                    _ => 0,
                };
            }
            self.metrics.transactions_failed += u64::from(result.is_err());
            if let Ok(fills) = &result {
                self.metrics.trades_committed += fills.len() as u64;
            }
            self.recent_transaction_outcomes
                .push_back(TransactionOutcome {
                    digest: tx
                        .to_digest()
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect(),
                    account_id: tx.account_id(),
                    nonce: tx.nonce(),
                    success: result.is_ok(),
                    error: result.err().map(|error| format!("{error:?}")),
                });
        }
        if latency_samples > 0 {
            self.metrics.last_commit_latency_ms = Some(latency_sum / latency_samples as u128);
            self.metrics.commit_latency_sum_ms += latency_sum;
            self.metrics.commit_latency_samples += latency_samples;
        }
        self.metrics.blocks_committed += 1;
        self.last_commit_at = Instant::now();
        if self.signing.is_some() {
            self.forward_cursor = None;
        }
        self.metrics.transactions_committed += successful;
        while self.recent_transaction_outcomes.len() > 512 {
            self.recent_transaction_outcomes.pop_front();
        }
        self.state.remove_included_txs_from_mempool(block);
        self.speculative
            .retain(|_, (candidate, _)| candidate.sequence > block.sequence);
        crate::profile::record(
            "commit_projection",
            projection_started.elapsed(),
            block.transactions.len() as u64,
        );
        if let Some(trace) = &mut self.commit_trace {
            trace.send(block.sequence, traced, traced_outcomes, committed_at);
        }
        if std::env::var_os("LIGHT_DEX_HANDOFF_TRACE").is_some() {
            eprintln!(
                "HANDOFF node={} view={} event=durable_commit sequence={} unix_us={}",
                self.node_id,
                self.consensus.current_view(),
                block.sequence,
                committed_at
            );
        }
        self.published_read = None;
        if let Some(updates) = &self.read_updates {
            let _ = updates.send(Arc::new(read_update));
        }
        println!(
            "Node {} committed seq {} txs={}",
            self.node_id,
            block.sequence,
            block.transactions.len()
        );
        Ok(())
    }

    fn execute_action(
        &mut self,
        action: &ConsensusAction,
        node_tx: &crate::priority::Sender<NodeEvent>,
    ) -> Result<(), std::io::Error> {
        match action {
            ConsensusAction::Broadcast(message) => {
                let encode_started = Instant::now();
                let bulk = message.is_bulk();
                let fast_proposal_shape = match message {
                    crate::consensus::ConsensusMessage::Hotstuff2(
                        crate::consensus::fast::FastMessage::Proposal { proposal, .. },
                    ) => Some((
                        proposal.block.sequence,
                        proposal.block.transactions.len(),
                        false,
                    )),
                    crate::consensus::ConsensusMessage::Hotstuff2(
                        crate::consensus::fast::FastMessage::CompactProposal {
                            height,
                            compact,
                            ..
                        },
                    ) => Some((*height, compact.transaction_digests.len(), true)),
                    _ => None,
                };
                let handoff_shape = match message {
                    crate::consensus::ConsensusMessage::Chained(
                        crate::consensus::chained::ChainedMessage::NewView { branch, .. },
                    ) => Some(("new_view", branch.len(), 0)),
                    crate::consensus::ConsensusMessage::Chained(
                        crate::consensus::chained::ChainedMessage::StartView {
                            branch, proofs, ..
                        },
                    ) => Some(("start_view", branch.len(), proofs.len())),
                    _ => None,
                };
                if let crate::consensus::ConsensusMessage::Chained(
                    crate::consensus::chained::ChainedMessage::Certified { qc },
                ) = message
                {
                    if let Some(block) = self
                        .speculative
                        .get(&qc.digest)
                        .map(|(block, _)| block.clone())
                    {
                        self.trace_block_stage(&block, "certified");
                    }
                }
                if self.consensus.is_chained() {
                    if let Some(block) = message.proposed_block() {
                        self.validate_speculative_proposal(block)
                            .map_err(std::io::Error::other)?;
                    }
                }
                let message = match &self.signing {
                    Some((manifest, key)) => Message::SignedConsensus(sign_consensus(
                        message.clone(),
                        self.node_id,
                        manifest,
                        key,
                    )?),
                    None => Message::Consensus(message.clone()),
                };
                if let Message::SignedConsensus(signed) = &message {
                    self.consensus.record_verified(signed.clone());
                }
                let serialized = sign_consensus_line(&message, &self.network.auth_key)?;
                crate::profile::record(
                    "consensus_broadcast_encode",
                    encode_started.elapsed(),
                    serialized.len() as u64,
                );
                if std::env::var_os("LIGHT_DEX_HANDOFF_TRACE").is_some() {
                    if let Some((height, txs, compact)) = fast_proposal_shape {
                        eprintln!(
                            "FAST_SEND node={} height={} txs={} bytes={} encode_us={} compact={compact}",
                            self.node_id,
                            height,
                            txs,
                            serialized.len(),
                            encode_started.elapsed().as_micros()
                        );
                    }
                }

                if std::env::var_os("LIGHT_DEX_HANDOFF_TRACE").is_some() {
                    if let Some((kind, branch_len, proof_count)) = handoff_shape {
                        eprintln!(
                            "HANDOFF node={} view={} event=wire_message kind={} bytes={} branch={} proofs={}",
                            self.node_id,
                            self.consensus.current_view(),
                            kind,
                            serialized.len(),
                            branch_len,
                            proof_count
                        );
                    }
                }

                if bulk {
                    if let ConsensusAction::Broadcast(
                        crate::consensus::ConsensusMessage::Chained(
                            crate::consensus::chained::ChainedMessage::CompactData {
                                view,
                                height,
                                digest,
                                ..
                            },
                        ),
                    ) = action
                    {
                        let now = Instant::now();
                        self.full_body_peers.retain(|_, until| *until > now);
                        if !self.full_body_peers.is_empty() {
                            if let Some((block, _)) = self.speculative.get(digest) {
                                let full = crate::consensus::ConsensusMessage::Chained(
                                    crate::consensus::chained::ChainedMessage::Data {
                                        view: *view,
                                        height: *height,
                                        digest: *digest,
                                        proposal: crate::blockchain::Proposal {
                                            block: block.clone(),
                                        },
                                    },
                                );
                                let full = match &self.signing {
                                    Some((manifest, key)) => Message::SignedConsensus(
                                        sign_consensus(full, self.node_id, manifest, key)?,
                                    ),
                                    None => Message::Consensus(full),
                                };
                                let full_line = sign_consensus_line(&full, &self.network.auth_key)?;
                                let peers = self
                                    .network
                                    .peers_address
                                    .keys()
                                    .copied()
                                    .collect::<Vec<_>>();
                                for peer in peers {
                                    let use_full = self.full_body_peers.contains_key(&peer);
                                    let line = if use_full {
                                        full_line.as_bytes()
                                    } else {
                                        serialized.as_bytes()
                                    };
                                    let _ = self.network.send_bulk(&peer, line);
                                    if use_full {
                                        self.metrics.adaptive_full_body_sends += 1;
                                    }
                                }
                                return Ok(());
                            }
                        }
                    }
                    self.network.broadcast_bulk(serialized.as_bytes())?;
                } else {
                    self.network.broadcast(serialized.as_bytes())?;
                }
            }

            ConsensusAction::Send { peer, message } => {
                let encode_started = Instant::now();
                let bulk = message.is_bulk();
                let handoff_branch = match message {
                    crate::consensus::ConsensusMessage::Chained(
                        crate::consensus::chained::ChainedMessage::NewView { branch, .. },
                    ) => Some(branch.len()),
                    _ => None,
                };
                let message = match &self.signing {
                    Some((manifest, key)) => Message::SignedConsensus(sign_consensus(
                        message.clone(),
                        self.node_id,
                        manifest,
                        key,
                    )?),
                    None => Message::Consensus(message.clone()),
                };
                if let Message::SignedConsensus(signed) = &message {
                    self.consensus.record_verified(signed.clone());
                }
                let serialized = sign_consensus_line(&message, &self.network.auth_key)?;
                crate::profile::record(
                    "consensus_send_encode",
                    encode_started.elapsed(),
                    serialized.len() as u64,
                );

                if std::env::var_os("LIGHT_DEX_HANDOFF_TRACE").is_some() {
                    if let Some(branch_len) = handoff_branch {
                        eprintln!(
                            "HANDOFF node={} view={} event=wire_message kind=new_view_unicast bytes={} branch={} peer={}",
                            self.node_id,
                            self.consensus.current_view(),
                            serialized.len(),
                            branch_len,
                            peer
                        );
                    }
                }

                let result = if bulk {
                    self.network.send_bulk(peer, serialized.as_bytes())
                } else {
                    self.network.send(peer, serialized.as_bytes())
                };
                if let Err(error) = result {
                    eprintln!(
                        "Node {} could not send consensus message to peer {}: {}",
                        self.node_id, peer, error
                    );
                }
            }

            ConsensusAction::Commit(proposal) => {
                let block = proposal.get_block();
                self.trace_block_stage(&block, "commit_decided");
                let certificate = self.consensus.commit_certificate(&block);
                let committed_view = match &certificate {
                    Some(CommitCertificate::Pbft { view, .. }) => *view,
                    Some(CommitCertificate::Hotstuff { qc }) => qc.view,
                    Some(CommitCertificate::Chained { qcs }) => qcs[0].view,
                    Some(CommitCertificate::Hotstuff2 { finality, .. })
                    | Some(CommitCertificate::Votor { finality, .. }) => finality.view,
                    None => self.consensus.current_view(),
                };
                let completed = self
                    .state
                    .persist_commit_async(block, certificate)
                    .map_err(std::io::Error::other)?;
                self.pending_commit = true;
                let tx = node_tx.clone();
                thread::spawn(move || {
                    let result = completed
                        .recv()
                        .unwrap_or_else(|_| Err("journal completion closed".into()));
                    let _ = tx.send(NodeEvent::DurableCommit {
                        result,
                        committed_view,
                    });
                });
            }

            ConsensusAction::TryPropose => self.request_proposal(node_tx),
            ConsensusAction::PrewarmNextLeader => self.prewarm_next_primary(),

            ConsensusAction::ScheduleTimeout {
                view,
                sequence,
                duration,
            } => {
                let tx = node_tx.clone();

                let view = *view;
                let sequence = *sequence;
                let duration = *duration;

                thread::spawn(move || {
                    thread::sleep(duration);

                    let _ = tx.send(NodeEvent::Consensus(ConsensusEvent::Timeout {
                        view,
                        sequence,
                    }));
                });
            }
        }

        Ok(())
    }

    fn execute_consensus_actions(
        &mut self,
        actions: &[ConsensusAction],
        node_tx: &crate::priority::Sender<NodeEvent>,
    ) -> Result<(), std::io::Error> {
        let previous = self.consensus.current_primary();
        for action in actions {
            if self.pending_commit {
                self.deferred_actions.push_back(action.clone());
            } else {
                self.execute_action(action, node_tx)?;
            }
        }
        if self.consensus.current_primary() != previous && self.signing.is_none() {
            self.forward_cursor = Some(0);
            self.forward_mempool_to_primary();
        }

        Ok(())
    }

    fn forward_mempool_to_primary(&mut self) {
        self.last_forward_sweep = Instant::now();
        let primary = self.consensus.current_primary();

        if primary == self.node_id {
            self.forward_cursor = None;
            return;
        }
        let start = self.forward_cursor.unwrap_or(0);
        let mut next = start;
        // With four-block rotation a leader can step down after ~400 ms.
        // 64 entries/tick only hands off ~256 transactions per tenure, below
        // even one 512-tx proposal. Size the bounded handoff to the proposal
        // budget so a tail is not repeatedly stranded until its holder leads
        // again. Outbound per-peer frame/byte limits still apply.
        let handoff_budget = BlockchainState::MAX_PROPOSAL_TRANSACTIONS / 2;
        let pending = self.state.mempool.forward_batch(start, handoff_budget);
        for chunk in pending.chunks(32) {
            if let Err(error) = self.forward_transaction_batch(primary, chunk, false) {
                eprintln!(
                    "Node {} failed retrying tx batch to primary {}: {}",
                    self.node_id, primary, error
                );
                // A dead leader will trigger view change; avoid hammering it
                // with every pending transaction on this timer tick.
                break;
            }
            next = chunk.last().expect("nonempty chunk").0 + 1;
        }
        self.forward_cursor = self.state.mempool.has_arrival_from(next).then_some(next);
    }

    fn prewarm_next_primary(&mut self) {
        if self.consensus.current_primary() != self.node_id {
            return;
        }
        let next = self
            .consensus
            .leader_for_view(self.consensus.current_view() + 1);
        if next == self.node_id {
            return;
        }
        let batch = self
            .state
            .mempool
            .forward_batch(0, BlockchainState::MAX_PROPOSAL_TRANSACTIONS / 2);
        for chunk in batch.chunks(32) {
            if self.forward_transaction_batch(next, chunk, true).is_err() {
                break;
            }
        }
    }

    fn validate_speculative_proposal(&mut self, block: &Block) -> Result<(), String> {
        let digest = block.to_digest();
        if self.speculative.contains_key(&digest) {
            return Ok(());
        }
        if (block.sequence as usize) < self.state.blocks.len() {
            return self
                .state
                .blocks
                .get(block.sequence as usize)
                .filter(|known| known.to_digest() == digest)
                .map(|_| ())
                .ok_or_else(|| "conflicting committed block".into());
        }
        if block.sequence == 0 {
            self.state.validate_block(block)?;
            self.speculative
                .insert(digest, (block.clone(), self.state.exchange_state.fork()));
            return Ok(());
        }
        let parent = if let Some(parent) = self
            .state
            .blocks
            .last()
            .filter(|parent| parent.to_digest() == block.previous_hash)
        {
            (parent, &self.state.exchange_state)
        } else {
            let (parent, exchange) =
                self.speculative.get(&block.previous_hash).ok_or_else(|| {
                    self.metrics.proposal_parent_misses += 1;
                    "missing speculative parent"
                })?;
            (parent, exchange)
        };
        // Failed receipts may be retried at the same nonce after a preceding
        // block changes the account state. State-root validation decides the
        // outcome against this exact speculative parent.
        let exchange = self
            .state
            .validate_speculative_block(block, parent.0, parent.1)?;
        self.speculative.insert(digest, (block.clone(), exchange));
        Ok(())
    }

    /// A bulk body may overtake the ancestor sent on another connection.
    /// Retry buffered signed bodies after a branch or sync supplies parents.
    fn retry_pending_data(&mut self, local_events: &mut VecDeque<NodeEvent>) {
        loop {
            let mut pending = self
                .pending_data
                .iter()
                .filter_map(|(digest, (from, message, _))| {
                    message
                        .proposed_block()
                        .map(|block| (block.sequence, *digest, *from, message.clone()))
                })
                .collect::<Vec<_>>();
            pending.sort_unstable_by_key(|(sequence, ..)| *sequence);
            let mut progressed = false;
            for (_, digest, from, message) in pending {
                let Some(block) = message.proposed_block() else {
                    continue;
                };
                match self.validate_speculative_proposal(block) {
                    Ok(()) => {
                        self.pending_data.remove(&digest);
                        local_events.push_back(NodeEvent::Consensus(ConsensusEvent::Message {
                            from,
                            message,
                        }));
                        progressed = true;
                    }
                    Err(error) if error == "missing speculative parent" => {}
                    Err(error) => {
                        eprintln!("Node {} discarded buffered proposal: {error}", self.node_id);
                        self.pending_data.remove(&digest);
                    }
                }
            }
            if !progressed {
                break;
            }
        }
    }

    /// A WAN peer may announce the next view before this validator has
    /// durably caught up. Keep the already authenticated message so the
    /// successor can use it as soon as the missing committed block arrives.
    fn defer_fast_message(&mut self, from: NodeId, message: crate::consensus::ConsensusMessage) {
        const MAX_PENDING: usize = 256;
        const MAX_PER_PEER: usize = 48;
        let key = message.fast_pending_key();
        if let Some(slot) = self
            .pending_fast
            .iter_mut()
            .find(|(peer, existing, _)| *peer == from && existing.fast_pending_key() == key)
        {
            *slot = (from, message, Instant::now());
            return;
        }
        if self
            .pending_fast
            .iter()
            .filter(|(peer, _, _)| *peer == from)
            .count()
            >= MAX_PER_PEER
        {
            if let Some(index) = self
                .pending_fast
                .iter()
                .position(|(peer, _, _)| *peer == from)
            {
                self.pending_fast.remove(index);
            }
        }
        if self.pending_fast.len() == MAX_PENDING {
            self.pending_fast.pop_front();
        }
        self.pending_fast.push_back((from, message, Instant::now()));
    }

    fn retry_pending_fast(&mut self, local_events: &mut VecDeque<NodeEvent>) -> Option<NodeId> {
        let height = self.consensus.next_sequence();
        let durable_height = self.state.blocks.len() as u64;
        let mut remaining = VecDeque::new();
        let mut sync_peer = None;
        while let Some((from, message, received)) = self.pending_fast.pop_front() {
            let Some(target) = message.fast_height() else {
                continue;
            };
            if received.elapsed() > Duration::from_secs(30) || target < height {
                continue;
            }
            if target > height || target > durable_height {
                sync_peer.get_or_insert(from);
                remaining.push_back((from, message, received));
            } else {
                local_events.push_back(NodeEvent::Consensus(ConsensusEvent::Message {
                    from,
                    message,
                }));
            }
        }
        self.pending_fast = remaining;
        sync_peer
    }

    fn expand_compact_data(
        &self,
        compact: &crate::consensus::chained::CompactProposal,
        digest: Digest,
    ) -> Result<Option<crate::blockchain::Proposal>, &'static str> {
        compact.reconstruct(digest, |transaction_digest| {
            self.state
                .pending_signed_transaction(transaction_digest)
                .map(|signed| (signed.transaction, signed.signature))
        })
    }

    fn retry_compact_data(
        &mut self,
        local_events: &mut VecDeque<NodeEvent>,
        node_tx: &crate::priority::Sender<NodeEvent>,
    ) -> Result<(), std::io::Error> {
        use crate::consensus::{ConsensusMessage, chained::ChainedMessage, fast::FastMessage};
        let now = Instant::now();
        let mut ready = Vec::new();
        let mut requests = Vec::new();
        let mut discarded = Vec::new();
        for (&digest, pending) in &self.pending_compact {
            if pending.height < self.consensus.next_sequence() {
                discarded.push(digest);
                continue;
            }
            match self.expand_compact_data(&pending.compact, digest) {
                Ok(Some(proposal)) => ready.push((
                    digest,
                    pending.from,
                    pending.view,
                    pending.height,
                    proposal,
                    pending.mode.clone(),
                )),
                Ok(None) if now >= pending.next_request => requests.push((
                    digest,
                    pending.from,
                    pending.view,
                    pending.height,
                    pending.mode.clone(),
                )),
                Err(error) => {
                    eprintln!("Node {} discarded compact proposal: {error}", self.node_id);
                    discarded.push(digest);
                }
                _ => {}
            }
        }
        for digest in discarded {
            self.pending_compact.remove(&digest);
        }
        for (digest, from, view, height, proposal, mode) in ready {
            self.pending_compact.remove(&digest);
            self.metrics.compact_reconstructed += 1;
            local_events.push_back(NodeEvent::Consensus(ConsensusEvent::Message {
                from,
                message: match mode {
                    CompactMode::Chained => ConsensusMessage::Chained(ChainedMessage::Data {
                        view,
                        height,
                        digest,
                        proposal,
                    }),
                    CompactMode::Hotstuff2 { justify } => {
                        ConsensusMessage::Hotstuff2(FastMessage::Proposal {
                            view,
                            height,
                            proposal,
                            justify,
                        })
                    }
                },
            }));
        }
        let mut actions = Vec::new();
        for (digest, peer, view, height, mode) in requests {
            if let Some(pending) = self.pending_compact.get_mut(&digest) {
                if now.duration_since(pending.first_seen) > Duration::from_secs(5) {
                    self.pending_compact.remove(&digest);
                    continue;
                }
                pending.next_request = now + Duration::from_millis(500);
                self.metrics.compact_body_requests += 1;
                actions.push(ConsensusAction::Send {
                    peer,
                    message: match mode {
                        CompactMode::Chained => {
                            ConsensusMessage::Chained(ChainedMessage::DataRequest { view, digest })
                        }
                        CompactMode::Hotstuff2 { .. } => {
                            ConsensusMessage::Hotstuff2(FastMessage::ProposalRequest {
                                view,
                                height,
                                digest,
                            })
                        }
                    },
                });
            }
        }
        self.execute_consensus_actions(&actions, node_tx)
    }

    fn maybe_propose(
        &mut self,
        node_tx: &crate::priority::Sender<NodeEvent>,
    ) -> Result<(), std::io::Error> {
        if !self.consensus.can_propose() {
            return Ok(());
        }

        if std::env::var_os("LIGHT_DEX_HANDOFF_TRACE").is_some() {
            eprintln!(
                "HANDOFF node={} view={} event=proposal_attempt unix_us={}",
                self.node_id,
                self.consensus.current_view(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_micros()
            );
        }

        println!(
            "Node {} is primary, mempool={}, trying to produce block",
            self.node_id,
            self.state.mempool.len()
        );

        let mut pairs = self
            .feed_urls
            .keys()
            .map(|(pair, _)| *pair)
            .collect::<Vec<_>>();
        pairs.sort_unstable();
        pairs.dedup();
        let oracle_updates = pairs
            .into_iter()
            .filter_map(|pair| {
                if !self.state.exchange_state.markets().values().any(|market| {
                    market.base_asset == pair.base_asset && market.quote_asset == pair.quote_asset
                }) {
                    return None;
                }
                let candidate =
                    crate::oracle::candidate_from_cache(pair, &self.market_data_quotes)?;
                if self
                    .state
                    .exchange_state
                    .oracle_mark(pair)
                    .is_some_and(|mark| mark.update_digest == candidate.digest())
                {
                    None
                } else {
                    Some(candidate)
                }
            })
            .take(32)
            .collect();
        let block = if self.consensus.is_chained() {
            let Some(parent_digest) = self.consensus.speculative_parent() else {
                return Ok(());
            };
            let allow_empty = self.consensus.needs_empty_block();
            if let Some((parent, exchange)) = self.speculative.get(&parent_digest) {
                self.state
                    .build_block_on(parent, exchange, oracle_updates, allow_empty)
            } else if let Some(parent) = self
                .state
                .blocks
                .last()
                .filter(|block| block.to_digest() == parent_digest)
            {
                let exchange = self.state.exchange_state.fork();
                self.state
                    .build_block_on(&parent.clone(), &exchange, oracle_updates, allow_empty)
            } else {
                let previous_view = self.consensus.current_view().saturating_sub(1);
                let peer = self.consensus.leader_for_view(previous_view);
                if peer != self.node_id {
                    self.start_branch_sync(peer, node_tx);
                }
                None
            }
        } else {
            self.state.build_block_with_oracle(oracle_updates)
        };
        let Some(block) = block else {
            println!(
                "Node {}: no block built because mempool is empty",
                self.node_id
            );

            return Ok(());
        };

        println!(
            "Node {} proposing block {} with {} txs",
            self.node_id,
            block.sequence,
            block.transactions.len()
        );
        self.trace_block_stage(&block, "proposed");

        if self.consensus.is_chained() {
            let digest = block.to_digest();
            if let Some(exchange) = self.state.take_built_speculative(digest) {
                self.speculative.insert(digest, (block.clone(), exchange));
            } else {
                self.validate_speculative_proposal(&block)
                    .map_err(std::io::Error::other)?;
            }
        }
        let actions = self.consensus.on_event(ConsensusEvent::Proposal(block));

        self.execute_consensus_actions(&actions, node_tx)?;

        Ok(())
    }

    fn maybe_start_consensus(
        &mut self,
        node_tx: &crate::priority::Sender<NodeEvent>,
    ) -> Result<(), std::io::Error> {
        if self.consensus.consensus_started(false) {
            return Ok(());
        }

        if !self.network.is_ready() {
            return Ok(());
        }

        let actions = self.consensus.on_start();

        self.execute_consensus_actions(&actions, node_tx)?;

        self.consensus.consensus_started(true);

        println!("READY node {}", self.node_id);

        Ok(())
    }

    fn forward_transaction(
        &mut self,
        primary: NodeId,
        transaction: Transaction,
        signed: Option<SignedTransaction>,
    ) -> Result<(), std::io::Error> {
        let message = match signed {
            Some(signed) => Message::SignedTransaction(signed),
            None if self.signing.is_none() => Message::Transaction(transaction),
            None => return Err(std::io::Error::other("missing transaction signature")),
        };
        let serialized = sign_line(&message, &self.network.auth_key)?;
        self.network.send_bulk(&primary, serialized.as_bytes())
    }

    fn forward_transaction_batch(
        &mut self,
        primary: NodeId,
        batch: &[(u64, Transaction)],
        prewarm: bool,
    ) -> Result<(), std::io::Error> {
        if self.signing.is_none()
            || std::env::var_os("LIGHT_DEX_DISABLE_TX_BATCH_FORWARD").is_some()
        {
            for (_, transaction) in batch {
                let signed = self.state.signed_transaction(transaction);
                self.forward_transaction(primary, transaction.clone(), signed)?;
            }
            return Ok(());
        }
        let transactions = batch
            .iter()
            .map(|(_, transaction)| {
                self.state.signed_transaction(transaction).ok_or_else(|| {
                    std::io::Error::other("missing transaction signature during batch handoff")
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if transactions.is_empty() {
            return Ok(());
        }
        let message = Message::SignedTransactionBatch {
            transactions,
            prewarm,
        };
        let serialized = crate::message::sign_compressed_line(&message, &self.network.auth_key)?;
        self.network.send_bulk(&primary, serialized.as_bytes())
    }
}

#[cfg(test)]
mod sync_tests;
