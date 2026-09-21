use std::{
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpStream},
    sync::mpsc,
    thread, time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{
    blockchain::{BlockchainState, Transaction},
    consensus::pbft::{Consensus, ConsensusAction, ConsensusEvent},
    message::Message,
    network::{Network, NetworkEvent},
};

pub type NodeId = u64;
const BLOCK_TIME: u64 = 1;

//Where we write the child process
#[derive(Debug)]
pub struct Node<C: Consensus> {
    node_id: NodeId,
    pid: u32,
    network: Network,
    state: BlockchainState,
    // consensus_started: bool,
    consensus: C,
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
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ControlMessage {
    Hello { node_id: NodeId },
    Pause { millis: u64 },
}

impl ControlMessage {
    pub fn to_vec(&self) -> Result<Vec<u8>, std::io::Error> {
        Ok(serde_json::to_vec(&self).expect("Failed serializing"))
    }

    pub fn from_vec(bytes: &[u8]) -> Result<Self, std::io::Error> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

impl<C: Consensus> Node<C> {
    pub fn new(
        node_id: NodeId,
        pid: u32,
        network: Network,
        state: BlockchainState,
        consensus: C,
    ) -> Result<Node<C>, std::io::Error> {
        Ok(Node {
            node_id,
            pid,
            network,
            state,
            consensus,
            // consensus_started: false,
        })
    }
    //After initializeation for communicating with parent
    pub fn register_with_parent(&self) -> Result<(), std::io::Error> {
        let mut stream = TcpStream::connect(self.network.parent_address)
            .expect("Failed connecting to parent adress");
        let message = format!(
            "REGISTER {} {} \n",
            self.node_id,
            self.network.listener.local_addr()?
        );
        let _ = stream.write_all(message.as_bytes());
        Ok(())
    }

    pub fn discover_peers(&mut self) -> Result<(), std::io::Error> {
        //Accept the parent
        let (stream, _) = self.network.listener.accept().expect("Failed accepting");
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let _ = line.trim();

            let parts: Vec<&str> = line.split_whitespace().collect();
            match parts.first() {
                Some(&"REGISTER") => {
                    let node_id: NodeId = parts[1].parse().expect("Failed getting node_id");
                    if node_id == self.node_id {
                        continue;
                    }
                    let node_address: SocketAddr =
                        parts[2].parse().expect("Failed getting node_address");

                    self.network.peers_address.insert(node_id, node_address);
                }

                Some(&"DONE") => break,
                _ => {}
            }
        }
        let n_peers: u64 = self.network.peers_address.len().try_into().expect("Failed");
        println!("Discovered {n_peers} peers");
        self.consensus.set_node_count(n_peers + 1);
        Ok(())
    }

    pub fn wait_for_start(&self) -> std::io::Result<()> {
        let mut stream = TcpStream::connect(self.network.parent_address)?;

        let ready = format!("READY {}\n", self.node_id);
        stream.write_all(ready.as_bytes())?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();

        reader.read_line(&mut line)?;

        if line.trim() != "START" {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "expected START from parent",
            ));
        }

        Ok(())
    }

    pub fn execute_action(
        &mut self,
        consensus_action: &ConsensusAction,
        node_tx: &mpsc::Sender<NodeEvent>,
    ) -> Result<(), std::io::Error> {
        match consensus_action {
            ConsensusAction::Broadcast(msg) => {
                let mut serialized = Message::Consensus(msg.clone()).to_str()?;

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

                println!("Node {} committed seq {}", self.node_id, block.sequence);

                self.state.commit_block(&block);

                self.state.remove_included_txs_from_mempool(&block);
            }

            ConsensusAction::ScheduleTimeout {
                view,
                sequence,
                duration,
            } => {
                let event_tx = node_tx.clone();

                let view = *view;
                let sequence = *sequence;
                let duration = *duration;

                thread::spawn(move || {
                    thread::sleep(duration);

                    let _ = event_tx.send(NodeEvent::Consensus(ConsensusEvent::Timeout {
                        view,
                        sequence,
                    }));
                });
            }
        }

        Ok(())
    }

    pub fn execute_consensus_actions(
        &mut self,
        actions: &[ConsensusAction],
        node_tx: &mpsc::Sender<NodeEvent>,
    ) -> Result<(), std::io::Error> {
        for action in actions {
            self.execute_action(action, node_tx)?;
        }

        Ok(())
    }
    fn maybe_propose(&mut self, node_tx: &mpsc::Sender<NodeEvent>) -> std::io::Result<()> {
        if !self.consensus.can_propose() {
            return Ok(());
        }

        let Some(block) = self.state.build_block() else {
            return Ok(());
        };

        let actions = self.consensus.on_event(ConsensusEvent::Proposal(block));

        self.execute_consensus_actions(&actions, node_tx)?;

        Ok(())
    }

    pub fn run(&mut self) -> Result<(), std::io::Error> {
        //Start the sender/receiver
        let (node_tx, node_rx) = mpsc::channel::<NodeEvent>();
        //Block Timer
        let block_timer_tx = node_tx.clone();
        thread::spawn(move || loop {
            thread::sleep(std::time::Duration::from_millis(BLOCK_TIME));

            if block_timer_tx.send(NodeEvent::ProduceBlock).is_err() {
                break;
            }
        });

        let network_node_tx = node_tx.clone();
        let _ = self.network.start();
        let network_rx = self
            .network
            .take_event_receiver()
            .expect("Failed taking network rx");

        //Start the network thread
        thread::spawn(move || loop {
            let network_event_recv = network_rx.recv();
            if network_event_recv.is_err() {
                break;
            }
            let network_event = network_event_recv.unwrap();

            match network_event {
                NetworkEvent::Message { from, message } => {
                    match message {
                        Message::Consensus(consensus_message) => {
                            let _ = network_node_tx.send(NodeEvent::Consensus(
                                ConsensusEvent::Message {
                                    from,
                                    message: consensus_message,
                                },
                            ));
                        }

                        //from peer connections
                        Message::Transaction(transaction) => {
                            let _ = network_node_tx.send(NodeEvent::NewTransaction(transaction));
                        }

                        Message::Control(_) => {
                            // Usually nothing here.
                            // HELLO is already consumed during connection setup.
                        }
                    }
                }

                NetworkEvent::IncomingPeer { node_id, stream } => {
                    let _ = network_node_tx.send(NodeEvent::Control(ControlEvent::PeerConnected {
                        node_id,
                        tcp_stream: stream,
                    }));
                }

                NetworkEvent::PeerConnected { .. } => {
                    // notification only, or ignore
                }
                NetworkEvent::ClientTransaction(client_tx) => {
                    let _ = network_node_tx.send(NodeEvent::NewTransaction(client_tx));
                }
                NetworkEvent::ClientControl(ControlMessage::Pause { millis }) => {
                   let _ =
                        network_node_tx.send(NodeEvent::Control(ControlEvent::Pause { millis }));
                },
                _=> {
                    todo!()
                }
            }
        });

        //Start consensus check
        self.maybe_start_consensus(&node_tx)?;

        if self.node_id == 0 {
            let tx_sender = node_tx.clone();
            thread::spawn(move || {
                let mut counter = 0;
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    let tx_payload = format!("Dummy tx {counter}");
                    let transaction = Transaction::new_with(tx_payload);

                    if tx_sender
                        .send(NodeEvent::NewTransaction(transaction))
                        .is_err()
                    {
                        break;
                    }
                    counter += 1;
                }
            });
        }

        loop {
            let node_event = node_rx.recv().expect("Node event channel closed");

            //Initial consensus event

            match node_event {
                NodeEvent::Consensus(consensus_event) => {
                    let consensus_actions = self.consensus.on_event(consensus_event);
                    let _ = self.execute_consensus_actions(&consensus_actions, &node_tx);
                }
                NodeEvent::Control(control_event) => {
                    let _ = self.handle_control_event(control_event);
                }
                NodeEvent::NewTransaction(tx) => {
                    let primary = self.consensus.current_primary();
                    let is_new = self.state.add_transaction(&tx);
                    if is_new && (self.node_id != primary) {
                        let _ = self.forward_transaction(primary, tx);
                    }
                }
                NodeEvent::ProduceBlock => {
                    let _ = self.maybe_propose(&node_tx);
                }
                _ => {
                    break;
                }
            }
            self.maybe_start_consensus(&node_tx)?;
            // self.maybe_propose(&node_tx);
        }
        Ok(())
    }

    pub fn maybe_start_consensus(
        &mut self,
        node_tx: &mpsc::Sender<NodeEvent>,
    ) -> Result<(), std::io::Error> {
        if !self.consensus.consensus_started(false) && self.network.is_ready() {
            let actions = self.consensus.on_start();
            self.execute_consensus_actions(&actions, node_tx)?;
            let node_id = self.node_id;
            println!("READY node {node_id}");
            let _ = self.consensus.consensus_started(true);
        }
        Ok(())
    }

    pub fn handle_control_event(
        &mut self,
        control_event: ControlEvent,
    ) -> Result<(), std::io::Error> {
        match control_event {
            ControlEvent::PeerConnected {
                node_id,
                tcp_stream,
            } => {
                #[cfg(debug_assertions)]
                println!(
                    "Node {} connections: {}/{}",
                    self.node_id,
                    self.network.peers_stream.len(),
                    self.network.peers_address.len()
                );
                self.network.add_peer(node_id, tcp_stream)
            }
            ControlEvent::Pause { millis } => {
                println!("Node {} PAUSED for {} ms", self.node_id, millis);

                thread::sleep(Duration::from_millis(millis));

                println!("Node {} RESUMED", self.node_id);
                Ok(())
            }
        }
    }
    fn forward_transaction(
        &mut self,
        primary: NodeId,
        transaction: Transaction,
    ) -> Result<(), std::io::Error> {
        let message = Message::Transaction(transaction.clone());
        let mut serialized = message.to_str()?;
        serialized.push('\n');
        self.network.send(&primary, serialized.as_bytes())
    }
}
