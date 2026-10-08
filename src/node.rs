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

use crate::{
    blockchain::{Block, BlockchainState, Digest, Trade, Transaction}, consensus::{CommitCertificate, Consensus, ConsensusAction, ConsensusEvent}, exchange::orderbook::OrderBookSnapshot, message::{Message, sign_line, verify_line}, network::{Network, NetworkEvent}
};
use crate::crypto::{SessionManifest, SignedConsensusMessage, SignedTransaction, sign_consensus, verify_consensus, verify_transaction};
use crate::exchange::address::Address;
use ed25519_dalek::SigningKey;
use std::sync::Arc;

const SYNC_BATCH_SIZE: usize = 32;

#[derive(Debug, Serialize, Deserialize)]
pub struct BlockSyncResponse {
    from_sequence: u64,
    chain_height: u64,
    blocks: Vec<Block>,
    #[serde(default)]
    certificates: Vec<Option<CommitCertificate>>,
}

pub type NodeId = u64;

#[derive(Debug, Default)]
struct NodeMetrics {
    authentication_rejections: u64,
    transactions_received: u64,

    blocks_committed: u64,
    transactions_committed: u64,
    transactions_failed: u64,
    trades_committed: u64,

    commit_latency_sum_ms: u128,
    commit_latency_samples: u64,

    last_commit_latency_ms: Option<u128>,

    transaction_received_at: HashMap<Digest, Instant>,
    recent_commit_latency_ms: VecDeque<u64>,
    catch_up_events: u64,
    blocks_caught_up: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone, TS)]
pub struct BlockSummary {
    pub sequence: u64,
    pub transaction_count: usize,
    pub digest: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, TS)]
pub struct AccountNonce {
    pub account_id: Address,
    pub nonce: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone, TS)]
pub struct AgentBalance {
    pub account_id: Address,
    pub usd_available: u64,
    pub usd_locked: u64,
    pub btc_available: u64,
    pub btc_locked: u64,
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

    pub network_ready: bool,
    pub peer_count: usize,
    pub expected_peers: usize,

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
    pub transactions_failed: u64,
    pub trades_committed: u64,

    pub average_commit_latency_ms: f64,
    pub last_commit_latency_ms: Option<u128>,
    pub recent_commit_latency_ms: Vec<u64>,
    pub catch_up_events: u64,
    pub blocks_caught_up: u64,

    pub recent_blocks: Vec<BlockSummary>,
    pub account_nonces: Vec<AccountNonce>,
    pub agent_balances: Vec<AgentBalance>,
    pub recent_transaction_outcomes: Vec<TransactionOutcome>,
    pub order_books: Vec<OrderBookSnapshot>,
    pub recent_trades: Vec<Trade>,
    pub open_orders: Vec<OpenOrderSnapshot>,
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

pub struct Node<C: Consensus> {
    node_id: NodeId,
    pid: u32,

    network: Network,
    state: BlockchainState,
    consensus: C,
    signing: Option<(Arc<SessionManifest>, SigningKey)>,

    block_time_ms: u64,
    last_primary_probe: Instant,

    metrics: NodeMetrics,
    recent_transaction_outcomes: VecDeque<TransactionOutcome>,
    sync_in_flight: bool,
    last_sync_probe: Instant,
    sync_peer_cursor: usize,
}

pub enum NodeEvent {
    Consensus(ConsensusEvent),
    SignedConsensus { from: NodeId, signed: SignedConsensusMessage },
    AuthenticationRejected,

    Control(ControlEvent),

    NewTransaction(Transaction),
    NewSignedTransaction(SignedTransaction),

    ProduceBlock,

    CatchUp { peer: NodeId, result: std::io::Result<BlockSyncResponse> },

    Shutdown,
}

pub enum ControlEvent {
    PeerConnected {
        node_id: NodeId,
        tcp_stream: TcpStream,
    },

    Pause {
        millis: u64,
    },

    Status {
        stream: TcpStream,
    },

    FetchBlocks { from_sequence: u64, limit: usize, stream: TcpStream },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ControlMessage {
    Hello { node_id: NodeId },

    Pause { millis: u64 },

    Status,

    FetchBlocks { from_sequence: u64, limit: usize },

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
        Ok(Node {
            node_id,
            pid,

            network,
            state,
            consensus,
            signing: None,

            block_time_ms,
            last_primary_probe: Instant::now(),

            metrics: NodeMetrics::default(),
            recent_transaction_outcomes: VecDeque::new(),
            sync_in_flight: false,
            last_sync_probe: Instant::now(),
            sync_peer_cursor: 0,
        })
    }

    pub fn attach_signing(&mut self, manifest: Arc<SessionManifest>, key: SigningKey) -> std::io::Result<()> {
        if self.state.signed_chain_id() != Some(manifest.chain_id.as_str()) {
            return Err(std::io::Error::other("node state and signing session differ"));
        }
        if key.verifying_key() != manifest.verifier(true, self.node_id)? {
            return Err(std::io::Error::other("validator signing key does not match session"));
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
            self.node_id,
            self.network.listener.local_addr()?
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

    fn apply_committed_block(&mut self, block: &Block, certificate: Option<CommitCertificate>) -> Result<(), std::io::Error> {
        let outcomes = match certificate {
            Some(certificate) => self.state.commit_block_with_certificate(block, certificate),
            None => self.state.commit_block_with_receipts(block),
        }.map_err(std::io::Error::other)?;
        let mut latency_sum = 0u128;
        let mut latency_samples = 0u64;
        let mut successful = 0u64;
        for (tx, result) in block.transactions.iter().zip(outcomes) {
            if let Some(start) = self.metrics.transaction_received_at.remove(&tx.to_digest()) {
                if result.is_ok() {
                    let elapsed_ms = start.elapsed().as_millis();
                    latency_sum += elapsed_ms;
                    latency_samples += 1;
                    self.metrics.recent_commit_latency_ms.push_back(elapsed_ms.min(u64::MAX as u128) as u64);
                    while self.metrics.recent_commit_latency_ms.len() > 1024 {
                        self.metrics.recent_commit_latency_ms.pop_front();
                    }
                }
            }
            successful += u64::from(result.is_ok());
            self.metrics.transactions_failed += u64::from(result.is_err());
            if let Ok(fills) = &result { self.metrics.trades_committed += fills.len() as u64; }
            self.recent_transaction_outcomes.push_back(TransactionOutcome {
                digest: tx.to_digest().iter().map(|byte| format!("{byte:02x}")).collect(),
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
        self.metrics.transactions_committed += successful;
        while self.recent_transaction_outcomes.len() > 512 { self.recent_transaction_outcomes.pop_front(); }
        self.state.remove_included_txs_from_mempool(block);
        println!("Node {} committed seq {} txs={}", self.node_id, block.sequence, block.transactions.len());
        Ok(())
    }

    fn start_sync(&mut self, peer: NodeId, node_tx: &mpsc::Sender<NodeEvent>) {
        if !self.consensus.supports_catch_up() || self.sync_in_flight { return; }
        let Some(&address) = self.network.peers_address.get(&peer) else { return; };
        self.sync_in_flight = true;
        self.last_sync_probe = Instant::now();
        // Include one already-committed anchor block so a peer with a
        // conflicting chain cannot appear healthy at the same height.
        let from_sequence = (self.state.blocks.len() as u64).saturating_sub(1);
        let auth_key = self.network.auth_key;
        let node_tx = node_tx.clone();
        thread::spawn(move || {
            let result = fetch_blocks(address, from_sequence, &auth_key);
            let _ = node_tx.send(NodeEvent::CatchUp { peer, result });
        });
    }

    fn probe_sync(&mut self, node_tx: &mpsc::Sender<NodeEvent>) {
        if !self.consensus.supports_catch_up() || self.sync_in_flight
            || self.last_sync_probe.elapsed() < Duration::from_secs(1) { return; }
        let mut peers = self.network.peers_address.keys().copied().collect::<Vec<_>>();
        peers.sort_unstable();
        if !peers.is_empty() {
            let peer = peers[self.sync_peer_cursor % peers.len()];
            self.sync_peer_cursor = self.sync_peer_cursor.wrapping_add(1);
            self.start_sync(peer, node_tx);
        }
    }

    fn sync_consensus_height(&mut self, node_tx: &mpsc::Sender<NodeEvent>) {
        let actions = self.consensus.catch_up_to_height(self.state.blocks.len() as u64);
        if let Err(error) = self.execute_consensus_actions(&actions, node_tx) {
            eprintln!("Node {} could not schedule catch-up actions: {error}", self.node_id);
        }
    }

    fn apply_sync_response(&mut self, peer: NodeId, response: BlockSyncResponse,
        node_tx: &mpsc::Sender<NodeEvent>) {
        let local_height = self.state.blocks.len() as u64;
        if response.blocks.len() > SYNC_BATCH_SIZE || response.from_sequence > local_height
            || response.certificates.len() != response.blocks.len()
            || response.from_sequence.checked_add(response.blocks.len() as u64).is_none_or(|end| end > response.chain_height)
            || response.blocks.iter().enumerate().any(|(index, block)|
                response.from_sequence.checked_add(index as u64) != Some(block.sequence)) {
            eprintln!("Node {} rejected malformed sync response from {peer}", self.node_id);
            return;
        }
        let mut applied = 0usize;
        for (block, certificate) in response.blocks.into_iter().zip(response.certificates) {
            let expected = self.state.blocks.len() as u64;
            if block.sequence < expected {
                let local = &self.state.blocks[block.sequence as usize];
                if local.to_digest() != block.to_digest() {
                    eprintln!("Node {} detected conflicting chain at sequence {} from node {peer}", self.node_id, block.sequence);
                    return;
                }
                continue;
            }
            if block.sequence != expected {
                if applied > 0 { self.sync_consensus_height(node_tx); }
                eprintln!("Node {} rejected noncontiguous sync block {} from {peer}; expected {expected}", self.node_id, block.sequence);
                return;
            }
            if let Err(error) = self.apply_committed_block(&block, certificate) {
                if applied > 0 { self.sync_consensus_height(node_tx); }
                eprintln!("Node {} rejected sync block {} from {peer}: {error}", self.node_id, block.sequence);
                return;
            }
            applied += 1;
        }
        if applied > 0 {
            let height = self.state.blocks.len() as u64;
            self.sync_consensus_height(node_tx);
            self.metrics.catch_up_events += 1;
            self.metrics.blocks_caught_up += applied as u64;
            println!("Node {} caught up {applied} blocks from node {peer}; next sequence {height}", self.node_id);
        }
        if (self.state.blocks.len() as u64) < response.chain_height && applied > 0 {
            self.start_sync(peer, node_tx);
        }
    }

    fn execute_action(
        &mut self,
        action: &ConsensusAction,
        node_tx: &mpsc::Sender<NodeEvent>,
    ) -> Result<(), std::io::Error> {
        match action {
            ConsensusAction::Broadcast(message) => {
                let message = match &self.signing {
                    Some((manifest, key)) => Message::SignedConsensus(sign_consensus(message.clone(), self.node_id, manifest, key)?),
                    None => Message::Consensus(message.clone()),
                };
                if let Message::SignedConsensus(signed) = &message { self.consensus.record_verified(signed.clone()); }
                let serialized = sign_line(&message, &self.network.auth_key)?;

                self.network.broadcast(serialized.as_bytes())?;
            }

            ConsensusAction::Send { peer, message } => {
                let message = match &self.signing {
                    Some((manifest, key)) => Message::SignedConsensus(sign_consensus(message.clone(), self.node_id, manifest, key)?),
                    None => Message::Consensus(message.clone()),
                };
                if let Message::SignedConsensus(signed) = &message { self.consensus.record_verified(signed.clone()); }
                let serialized = sign_line(&message, &self.network.auth_key)?;

                self.network.send(peer, serialized.as_bytes())?;
            }

            ConsensusAction::Commit(proposal) => {
                let block = proposal.get_block();
                let certificate = self.consensus.commit_certificate(&block);
                self.apply_committed_block(&block, certificate)?;
            }

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
        node_tx: &mpsc::Sender<NodeEvent>,
    ) -> Result<(), std::io::Error> {
        for action in actions {
            self.execute_action(action, node_tx)?;
        }

        Ok(())
    }

    fn forward_mempool_to_primary(&mut self) {
        let primary = self.consensus.current_primary();

        if primary == self.node_id {
            return;
        }

        let transactions = self.state.mempool.clone();

        for tx in transactions {
            let signed = self.state.signed_transaction(&tx);
            if let Err(error) = self.forward_transaction(primary, tx, signed) {
                eprintln!(
                    "Node {} failed retrying tx to primary {}: {}",
                    self.node_id, primary, error
                );
            }
        }
    }

    fn maybe_propose(&mut self, node_tx: &mpsc::Sender<NodeEvent>) -> Result<(), std::io::Error> {
        if !self.consensus.can_propose() {
            return Ok(());
        }

        println!(
            "Node {} is primary, mempool={}, trying to produce block",
            self.node_id,
            self.state.mempool.len()
        );

        let Some(block) = self.state.build_block() else {
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

        let actions = self.consensus.on_event(ConsensusEvent::Proposal(block));

        self.execute_consensus_actions(&actions, node_tx)?;

        Ok(())
    }

    fn maybe_start_consensus(
        &mut self,
        node_tx: &mpsc::Sender<NodeEvent>,
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
        self.network.send(&primary, serialized.as_bytes())
    }

    fn status(&mut self) -> NodeStatus {
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

        NodeStatus {
            node_id: self.node_id,
            pid: self.pid,
            chain_id: self.signing.as_ref().map(|(manifest, _)| manifest.chain_id.clone()).unwrap_or_default(),
            signing_enabled: self.signing.is_some(),
            authentication_rejections: self.metrics.authentication_rejections,

            network_ready: self.network.is_ready(),

            peer_count: self.network.peers_stream.len(),

            expected_peers: self.network.peers_address.len(),

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
            transactions_failed: self.metrics.transactions_failed,
            trades_committed: self.metrics.trades_committed,

            average_commit_latency_ms,

            last_commit_latency_ms: self.metrics.last_commit_latency_ms,
            recent_commit_latency_ms: self.metrics.recent_commit_latency_ms.iter().copied().collect(),
            catch_up_events: self.metrics.catch_up_events,
            blocks_caught_up: self.metrics.blocks_caught_up,

            recent_blocks,
            account_nonces: self.state.exchange_state.accounts().values().map(|account| AccountNonce {
                account_id: account.id,
                nonce: account.nonce,
            }).collect(),
            agent_balances: self.state.exchange_state.accounts().values().map(|account| {
                let usd = account.balances.get(&crate::exchange::market::USD);
                let btc = account.balances.get(&crate::exchange::market::BTC);
                AgentBalance {
                    account_id: account.id,
                    usd_available: usd.map_or(0, |balance| balance.available),
                    usd_locked: usd.map_or(0, |balance| balance.locked),
                    btc_available: btc.map_or(0, |balance| balance.available),
                    btc_locked: btc.map_or(0, |balance| balance.locked),
                }
            }).collect(),
            recent_transaction_outcomes: self.recent_transaction_outcomes.iter().cloned().collect(),
            order_books: {
                let mut books = self.state.exchange_state.order_books().iter()
                    .map(|(&market_id, book)| book.snapshot(market_id, 20)).collect::<Vec<_>>();
                books.sort_by_key(|book| book.market_id);
                books
            },
            recent_trades: self.state.recent_trades.iter().rev().cloned().collect(),
            open_orders: {
                let mut orders = self.state.exchange_state.order_books().iter()
                    .flat_map(|(&market_id, book)| book.bids.values().chain(book.asks.values())
                        .flat_map(move |level| level.iter().map(move |order| OpenOrderSnapshot {
                            order_id: order.id, account_id: order.account, market_id,
                            side: order.side, price: order.price,
                            remaining_quantity: order.remaining_quantity,
                        })))
                    .collect::<Vec<_>>();
                orders.sort_by_key(|order| order.order_id);
                orders
            },
        }
    }

    fn handle_control_event(&mut self, event: ControlEvent) -> Result<(), std::io::Error> {
        match event {
            ControlEvent::PeerConnected {
                node_id,
                tcp_stream,
            } => self.network.add_peer(node_id, tcp_stream),

            ControlEvent::Pause { millis } => {
                println!("Node {} PAUSED for {} ms", self.node_id, millis);

                thread::sleep(Duration::from_millis(millis));

                println!("Node {} RESUMED", self.node_id);

                Ok(())
            }

            ControlEvent::Status { mut stream } => {
                let status = self.status();

                let serialized = sign_line(&status, &self.network.auth_key)?;

                stream.write_all(serialized.as_bytes())?;

                Ok(())
            }

            ControlEvent::FetchBlocks { from_sequence, limit, mut stream } => {
                let start = usize::try_from(from_sequence).unwrap_or(usize::MAX).min(self.state.blocks.len());
                let blocks = self.state.blocks.iter().skip(start).take(limit.min(SYNC_BATCH_SIZE)).cloned().collect();
                let certificates = self.state.certificates.iter().skip(start).take(limit.min(SYNC_BATCH_SIZE)).cloned().collect();
                let response = BlockSyncResponse {
                    from_sequence,
                    chain_height: self.state.blocks.len() as u64,
                    blocks,
                    certificates,
                };
                let serialized = sign_line(&response, &self.network.auth_key)?;
                stream.write_all(serialized.as_bytes())
            }
        }
    }

    pub fn run(&mut self) -> Result<(), std::io::Error> {
        let (node_tx, node_rx) = mpsc::channel::<NodeEvent>();

        //
        // Block production timer
        //
        let block_tx = node_tx.clone();

        let block_time_ms = self.block_time_ms;

        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_millis(block_time_ms));

                if block_tx.send(NodeEvent::ProduceBlock).is_err() {
                    break;
                }
            }
        });

        //
        // Network
        //
        self.network.start()?;

        let network_rx = self
            .network
            .take_event_receiver()
            .expect("network receiver already taken");

        let network_node_tx = node_tx.clone();
        let signing_manifest = self.signing.as_ref().map(|(manifest, _)| manifest.clone());

        thread::spawn(move || {
            loop {
                let Ok(event) = network_rx.recv() else {
                    break;
                };

                match event {
                    NetworkEvent::AuthenticationRejected => {
                        let _ = network_node_tx.send(NodeEvent::AuthenticationRejected);
                    }
                    NetworkEvent::Message { from, message } => {
                        match message {
                            Message::Consensus(message) => {
                                if signing_manifest.is_some() {
                                    let _ = network_node_tx.send(NodeEvent::AuthenticationRejected);
                                    continue;
                                }
                                let _ = network_node_tx.send(NodeEvent::Consensus(
                                    ConsensusEvent::Message { from, message },
                                ));
                            }

                            // Peer forwarded a transaction.
                            Message::Transaction(transaction) => {
                                if signing_manifest.is_some() {
                                    let _ = network_node_tx.send(NodeEvent::AuthenticationRejected);
                                    continue;
                                }
                                let _ =
                                    network_node_tx.send(NodeEvent::NewTransaction(transaction));
                            }

                            Message::SignedConsensus(signed) => {
                                let Some(manifest) = &signing_manifest else { continue; };
                                if signed.signer != from || verify_consensus(&signed, manifest).is_err() {
                                    let _ = network_node_tx.send(NodeEvent::AuthenticationRejected);
                                    continue;
                                }
                                let _ = network_node_tx.send(NodeEvent::SignedConsensus { from, signed });
                            }

                            Message::SignedTransaction(transaction) => {
                                if signing_manifest.as_ref().is_some_and(|manifest| verify_transaction(&transaction, manifest).is_ok()) {
                                    let _ = network_node_tx.send(NodeEvent::NewSignedTransaction(transaction));
                                } else {
                                    let _ = network_node_tx.send(NodeEvent::AuthenticationRejected);
                                }
                            }

                            Message::Control(_) | Message::PeerHandshake(_) => {}
                        }
                    }

                    NetworkEvent::IncomingPeer { node_id, stream } => {
                        let _ =
                            network_node_tx.send(NodeEvent::Control(ControlEvent::PeerConnected {
                                node_id,
                                tcp_stream: stream,
                            }));
                    }

                    NetworkEvent::PeerConnected { .. } => {}

                    NetworkEvent::ClientTransaction(transaction) => {
                        if signing_manifest.is_none() { let _ = network_node_tx.send(NodeEvent::NewTransaction(transaction)); }
                        else { let _ = network_node_tx.send(NodeEvent::AuthenticationRejected); }
                    }

                    NetworkEvent::SignedClientTransaction(transaction) => {
                        if signing_manifest.as_ref().is_some_and(|manifest| verify_transaction(&transaction, manifest).is_ok()) {
                            let _ = network_node_tx.send(NodeEvent::NewSignedTransaction(transaction));
                        } else {
                            let _ = network_node_tx.send(NodeEvent::AuthenticationRejected);
                        }
                    }

                    NetworkEvent::ClientControl { message, stream } => match message {
                        ControlMessage::Pause { millis } => {
                            let _ = network_node_tx
                                .send(NodeEvent::Control(ControlEvent::Pause { millis }));
                        }

                        ControlMessage::Status => {
                            let _ = network_node_tx
                                .send(NodeEvent::Control(ControlEvent::Status { stream }));
                        }

                        ControlMessage::FetchBlocks { from_sequence, limit } => {
                            let _ = network_node_tx.send(NodeEvent::Control(ControlEvent::FetchBlocks {
                                from_sequence, limit, stream,
                            }));
                        }

                        ControlMessage::Shutdown => {
                            let _ = network_node_tx.send(NodeEvent::Shutdown);
                        }

                        ControlMessage::Hello { .. } => {}
                    },
                }
            }
        });

        self.maybe_start_consensus(&node_tx)?;

        //
        // Node event loop
        //
        loop {
            let event = node_rx.recv().map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "node event channel closed")
            })?;

            match event {
                NodeEvent::AuthenticationRejected => self.metrics.authentication_rejections += 1,
                NodeEvent::SignedConsensus { from, signed } => {
                    self.consensus.record_verified(signed.clone());
                    let _ = node_tx.send(NodeEvent::Consensus(ConsensusEvent::Message {
                        from, message: signed.message,
                    }));
                }
                NodeEvent::Consensus(event) => {
                    if let ConsensusEvent::Message {
                        from,
                        message,
                    } = &event {
                        if let Some(block) = message.proposed_block() {
                            if self.consensus.supports_catch_up()
                                && block.sequence > self.state.blocks.len() as u64 {
                                self.start_sync(*from, &node_tx);
                                continue;
                            }
                            if let Err(error) = self.state.validate_block(block) {
                                eprintln!("Node {} rejected proposal: {error}", self.node_id);
                                continue;
                            }
                        }
                    }
                    let old_primary = self.consensus.current_primary();

                    let actions = self.consensus.on_event(event);

                    self.execute_consensus_actions(&actions, &node_tx)?;
                    let new_primary = self.consensus.current_primary();

                    if new_primary != old_primary {
                        self.last_primary_probe = Instant::now();
                        println!(
                            "Node {} primary changed {} -> {}",
                            self.node_id, old_primary, new_primary
                        );

                        self.forward_mempool_to_primary();
                    }
                }

                NodeEvent::Control(event) => {
                    self.handle_control_event(event)?;
                }

                NodeEvent::NewTransaction(transaction) => {
                    if self.signing.is_some() { continue; }
                    let primary = self.consensus.current_primary();

                    let is_new = self.state.add_transaction(&transaction);

                    if is_new {
                        self.metrics.transactions_received += 1;

                        self.metrics
                            .transaction_received_at
                            .insert(transaction.to_digest(), Instant::now());
                        let actions = self.consensus.on_event(ConsensusEvent::TransactionAvailable);
                        self.execute_consensus_actions(&actions, &node_tx)?;
                    }

                    // Only forward toward the current leader.
                    if is_new && self.node_id != primary {
                        if let Err(error) = self.forward_transaction(primary, transaction, None) {
                            eprintln!(
                                "Node {} failed forwarding tx to primary {}: {}",
                                self.node_id, primary, error
                            );

                            let actions = self.consensus.on_event(ConsensusEvent::LeaderUnavailable);

                            self.execute_consensus_actions(&actions, &node_tx)?;
                        }
                    }
                }

                NodeEvent::NewSignedTransaction(signed) => {
                    let Some((manifest, _)) = &self.signing else { continue; };
                    if verify_transaction(&signed, manifest).is_err() { continue; }
                    let transaction = &signed.transaction;
                    let is_new = self.state.add_signed_transaction(&signed);
                    if is_new {
                        self.metrics.transactions_received += 1;
                        self.metrics.transaction_received_at.insert(transaction.to_digest(), Instant::now());
                        let actions = self.consensus.on_event(ConsensusEvent::TransactionAvailable);
                        self.execute_consensus_actions(&actions, &node_tx)?;
                    }
                    if is_new && self.node_id != self.consensus.current_primary() {
                        let primary = self.consensus.current_primary();
                        if let Err(error) = self.forward_transaction(primary, transaction.clone(), Some(signed)) {
                            eprintln!("Node {} failed forwarding signed tx to primary {primary}: {error}", self.node_id);
                            let actions = self.consensus.on_event(ConsensusEvent::LeaderUnavailable);
                            self.execute_consensus_actions(&actions, &node_tx)?;
                        }
                    }
                }

                NodeEvent::ProduceBlock => {
                    if !self.network.is_ready() { self.network.reconnect_missing(); }
                    self.probe_sync(&node_tx);
                    // Detect a dead leader even when no fresh transaction is
                    // admitted (for example, only stale nonces are replayed).
                    // A quiet cluster should still elect a live primary.
                    let primary = self.consensus.current_primary();
                    if self.consensus.consensus_started(false)
                        && primary != self.node_id
                        && self.last_primary_probe.elapsed()
                            >= Duration::from_millis(self.consensus.get_timeout())
                    {
                        self.last_primary_probe = Instant::now();
                        let unavailable = self.network.peers_address.get(&primary).is_some_and(|address| {
                            TcpStream::connect_timeout(address, Duration::from_millis(100)).is_err()
                        });
                        if unavailable {
                            let actions = self.consensus.on_event(ConsensusEvent::LeaderUnavailable);
                            self.execute_consensus_actions(&actions, &node_tx)?;
                        }
                    }
                    self.maybe_propose(&node_tx)?;
                }

                NodeEvent::Shutdown => {
                    println!("Node {} shutting down", self.node_id);
                    break;
                }

                NodeEvent::CatchUp { peer, result } => {
                    self.sync_in_flight = false;
                    match result {
                        Ok(response) => self.apply_sync_response(peer, response, &node_tx),
                        Err(error) => eprintln!("Node {} could not sync from node {peer}: {error}", self.node_id),
                    }
                }
            }

            self.maybe_start_consensus(&node_tx)?;
        }

        Ok(())
    }
}

fn fetch_blocks(address: SocketAddr, from_sequence: u64, auth_key: &[u8; 32]) -> std::io::Result<BlockSyncResponse> {
    let timeout = Duration::from_millis(500);
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let request = sign_line(&Message::Control(ControlMessage::FetchBlocks {
        from_sequence, limit: SYNC_BATCH_SIZE,
    }), auth_key)?;
    stream.write_all(request.as_bytes())?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    if line.is_empty() { return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "empty block sync response")); }
    verify_line(&line, auth_key)
}

#[cfg(test)]
mod sync_tests {
    use super::*;
    use crate::consensus::hotstuff::HotStuff;
    use std::net::TcpListener;

    #[test]
    fn lagging_hotstuff_replica_replays_a_verified_batch() {
        let mut source = BlockchainState::new();
        let genesis = Block::new_initial_block();
        source.commit_block(&genesis).unwrap();
        let tx = Transaction::Transfer { from: 0.into(), to: 1.into(), asset: 0, amount: 7, nonce: 0 };
        assert!(source.add_transaction(&tx));
        let next = source.build_block().unwrap();
        source.commit_block(&next).unwrap();
        let address = "127.0.0.1:0".parse().unwrap();
        let network = Network::new(2, address, [9; 32]).unwrap();
        let mut node = Node::new(2, 0, network, BlockchainState::new(), HotStuff::new(2, 4), 100).unwrap();
        let (tx, _rx) = mpsc::channel();
        node.apply_sync_response(0, BlockSyncResponse {
            from_sequence: 0, chain_height: 2, blocks: vec![genesis, next],
            certificates: vec![None, None],
        }, &tx);
        assert_eq!(node.state.blocks.len(), 2);
        assert_eq!(node.consensus.next_sequence(), 2);
        assert_eq!(node.state.exchange_state.get_nonce_for_account(0.into()).unwrap(), 1);
    }

    #[test]
    fn bad_sync_block_does_not_advance_the_chain() {
        let address = "127.0.0.1:0".parse().unwrap();
        let network = Network::new(2, address, [9; 32]).unwrap();
        let mut node = Node::new(2, 0, network, BlockchainState::new(), HotStuff::new(2, 4), 100).unwrap();
        let mut forged = Block::new_initial_block();
        forged.sequence = 1;
        let (tx, _rx) = mpsc::channel();
        node.apply_sync_response(0, BlockSyncResponse {
            from_sequence: 0, chain_height: 2, blocks: vec![forged],
            certificates: vec![None],
        }, &tx);
        assert_eq!(node.state.blocks.len(), 0);
        assert_eq!(node.consensus.next_sequence(), 0);
    }

    #[test]
    fn authenticated_block_fetch_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let key = [11; 32];
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
            let request: Message = verify_line(&line, &key).unwrap();
            assert!(matches!(request, Message::Control(ControlMessage::FetchBlocks {
                from_sequence: 0, limit: SYNC_BATCH_SIZE,
            })));
            let response = BlockSyncResponse {
                from_sequence: 0, chain_height: 1, blocks: vec![Block::new_initial_block()],
                certificates: vec![None],
            };
            stream.write_all(sign_line(&response, &key).unwrap().as_bytes()).unwrap();
        });
        let response = fetch_blocks(address, 0, &key).unwrap();
        assert_eq!(response.blocks.len(), 1);
        assert_eq!(response.chain_height, 1);
        server.join().unwrap();
    }

    #[test]
    fn conflicting_anchor_is_not_accepted() {
        let mut state = BlockchainState::new();
        let genesis = Block::new_initial_block();
        state.commit_block(&genesis).unwrap();
        let mut conflicting = genesis.clone();
        conflicting.state_root[0] ^= 1;
        let address = "127.0.0.1:0".parse().unwrap();
        let network = Network::new(2, address, [9; 32]).unwrap();
        let mut node = Node::new(2, 0, network, state, HotStuff::new(2, 4), 100).unwrap();
        let (tx, _rx) = mpsc::channel();
        node.apply_sync_response(0, BlockSyncResponse {
            from_sequence: 0, chain_height: 1, blocks: vec![conflicting],
            certificates: vec![None],
        }, &tx);
        assert_eq!(node.state.blocks[0].to_digest(), genesis.to_digest());
        assert_eq!(node.consensus.next_sequence(), 1);
    }
}
