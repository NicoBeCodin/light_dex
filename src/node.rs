use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpStream},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{
    blockchain::{BlockchainState, Digest, Transaction},
    consensus::pbft::{Consensus, ConsensusAction, ConsensusEvent},
    message::Message,
    network::{Network, NetworkEvent},
};

pub type NodeId = u64;

#[derive(Debug, Default)]
struct NodeMetrics {
    transactions_received: u64,

    blocks_committed: u64,
    transactions_committed: u64,

    commit_latency_sum_ms: u128,
    commit_latency_samples: u64,

    last_commit_latency_ms: Option<u128>,

    transaction_received_at: HashMap<Digest, Instant>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BlockSummary {
    pub sequence: u64,
    pub transaction_count: usize,
    pub digest: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NodeStatus {
    pub node_id: NodeId,
    pub pid: u32,

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

    pub average_commit_latency_ms: f64,
    pub last_commit_latency_ms: Option<u128>,

    pub recent_blocks: Vec<BlockSummary>,
}

#[derive(Debug)]
pub struct Node<C: Consensus> {
    node_id: NodeId,
    pid: u32,

    network: Network,
    state: BlockchainState,
    consensus: C,

    block_time_ms: u64,

    metrics: NodeMetrics,
}

pub enum NodeEvent {
    Consensus(ConsensusEvent),

    Control(ControlEvent),

    NewTransaction(Transaction),

    ProduceBlock,

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
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ControlMessage {
    Hello { node_id: NodeId },

    Pause { millis: u64 },

    Status,

    Shutdown,
}

impl<C: Consensus> Node<C> {
    pub fn new(
        node_id: NodeId,
        pid: u32,
        network: Network,
        state: BlockchainState,
        consensus: C,
        block_time_ms: u64,
    ) -> Result<Node<C>, std::io::Error> {
        Ok(Node {
            node_id,
            pid,

            network,
            state,
            consensus,

            block_time_ms,

            metrics: NodeMetrics::default(),
        })
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

    fn execute_action(
        &mut self,
        action: &ConsensusAction,
        node_tx: &mpsc::Sender<NodeEvent>,
    ) -> Result<(), std::io::Error> {
        match action {
            ConsensusAction::Broadcast(message) => {
                let mut serialized = Message::Consensus(message.clone()).to_str()?;

                serialized.push('\n');

                self.network.broadcast(serialized.as_bytes())?;
            }

            ConsensusAction::Send { peer, message } => {
                let mut serialized = Message::Consensus(message.clone()).to_str()?;

                serialized.push('\n');

                self.network.send(peer, serialized.as_bytes())?;
            }

            ConsensusAction::Commit(proposal) => {
                let block = proposal.get_block();

                let mut latency_sum = 0u128;
                let mut latency_samples = 0u64;

                for tx in &block.transactions {
                    if let Some(start) =
                        self.metrics.transaction_received_at.remove(&tx.to_digest())
                    {
                        latency_sum += start.elapsed().as_millis();
                        latency_samples += 1;
                    }
                }

                if latency_samples > 0 {
                    self.metrics.last_commit_latency_ms =
                        Some(latency_sum / latency_samples as u128);

                    self.metrics.commit_latency_sum_ms += latency_sum;

                    self.metrics.commit_latency_samples += latency_samples;
                }

                self.metrics.blocks_committed += 1;

                self.metrics.transactions_committed += block.transactions.len() as u64;

                println!(
                    "Node {} committed seq {} txs={}",
                    self.node_id,
                    block.sequence,
                    block.transactions.len()
                );

                self.state.commit_block(&block);

                self.state.remove_included_txs_from_mempool(&block);
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
            if let Err(error) = self.forward_transaction(primary, tx) {
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
    ) -> Result<(), std::io::Error> {
        let mut serialized = Message::Transaction(transaction).to_str()?;
        serialized.push('\n');
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

            average_commit_latency_ms,

            last_commit_latency_ms: self.metrics.last_commit_latency_ms,

            recent_blocks,
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

                let mut serialized = serde_json::to_string(&status)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;

                serialized.push('\n');

                stream.write_all(serialized.as_bytes())?;

                Ok(())
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

        thread::spawn(move || loop {
            thread::sleep(Duration::from_millis(block_time_ms));

            if block_tx.send(NodeEvent::ProduceBlock).is_err() {
                break;
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

        thread::spawn(move || loop {
            let Ok(event) = network_rx.recv() else {
                break;
            };

            match event {
                NetworkEvent::Message { from, message } => {
                    match message {
                        Message::Consensus(message) => {
                            let _ = network_node_tx.send(NodeEvent::Consensus(
                                ConsensusEvent::Message { from, message },
                            ));
                        }

                        // Peer forwarded a transaction.
                        Message::Transaction(transaction) => {
                            let _ = network_node_tx.send(NodeEvent::NewTransaction(transaction));
                        }

                        Message::Control(_) => {}
                    }
                }

                NetworkEvent::IncomingPeer { node_id, stream } => {
                    let _ = network_node_tx.send(NodeEvent::Control(ControlEvent::PeerConnected {
                        node_id,
                        tcp_stream: stream,
                    }));
                }

                NetworkEvent::PeerConnected { .. } => {}

                NetworkEvent::ClientTransaction(transaction) => {
                    let _ = network_node_tx.send(NodeEvent::NewTransaction(transaction));
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

                    ControlMessage::Shutdown => {
                        let _ = network_node_tx.send(NodeEvent::Shutdown);
                    }

                    ControlMessage::Hello { .. } => {}
                },
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
                NodeEvent::Consensus(event) => {
                    let old_primary = self.consensus.current_primary();
                    
                    let actions = self.consensus.on_event(event);

                    self.execute_consensus_actions(&actions, &node_tx)?;
                    let new_primary = self.consensus.current_primary();

                    if new_primary != old_primary {
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
                    let primary = self.consensus.current_primary();

                    let is_new = self.state.add_transaction(&transaction);

                    if is_new {
                        self.metrics.transactions_received += 1;

                        self.metrics
                            .transaction_received_at
                            .insert(transaction.to_digest(), Instant::now());
                    }

                    // Only forward toward the current leader.
                    if is_new && self.node_id != primary {
                        if let Err(error) = self.forward_transaction(primary, transaction) {
                            eprintln!(
                                "Node {} failed forwarding tx to primary {}: {}",
                                self.node_id, primary, error
                            );

                            let actions = self.consensus.on_event(ConsensusEvent::Timeout {
                                view: self.consensus.current_view(),
                                sequence: self.consensus.next_sequence(),
                            });

                            self.execute_consensus_actions(&actions, &node_tx)?;
                        }
                    }
                }

                NodeEvent::ProduceBlock => {
                    self.maybe_propose(&node_tx)?;
                }

                NodeEvent::Shutdown => {
                    println!("Node {} shutting down", self.node_id);
                    break;
                }
            }

            self.maybe_start_consensus(&node_tx)?;
        }

        Ok(())
    }
}
