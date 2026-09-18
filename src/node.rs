use std::{
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpStream},
    sync::mpsc,
    thread,
};

use serde::{Deserialize, Serialize};

use crate::{
    blockchain::{BlockchainState, Transaction},
    consensus::pbft::{Consensus, ConsensusAction, ConsensusEvent},
    message::Message,
    network::{Network, NetworkEvent},
};

pub type NodeId = u64;

//Where we write the child process
#[derive(Debug)]
pub struct Node<C: Consensus> {
    node_id: NodeId,
    pid: u32,
    network: Network,
    state: BlockchainState,
    consensus_started: bool,

    consensus: C,
}

pub enum NodeEvent {
    Consensus(ConsensusEvent),
    Control(ControlEvent),
    NewTransaction(Transaction),
    Timeout,
    Shutdown,
}

pub enum ControlEvent {
    PeerConnected {
        node_id: NodeId,
        tcp_stream: TcpStream,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ControlMessage {
    Hello { node_id: NodeId },
}

impl ControlMessage {
    pub fn to_vec(&self) -> Result<Vec<u8>, std::io::Error> {
        Ok(serde_json::to_vec(&self).expect("Failed serializing"))
    }

    pub fn from_vec(bytes: &[u8]) -> Result<Self, std::io::Error> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

// pub fn treat_control_message(
//     control_message: ControlMessage,
// ) -> Result<ControlEvent, std::io::Error> {
//     match control_message {
//         ControlMessage::Hello { node_id } => Ok(ControlEvent::PeerConnected { node_id, tcp_stream }),
//     }
// }

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
            consensus_started: false,
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
                let node_id = self.node_id;
                println!("Commited {proposal:?} node {node_id}");
                self.state.add_block(proposal.get_block());

            }
        }
        Ok(())
    }

    pub fn execute_consensus_actions(
        &mut self,
        consensus_actions: &Vec<ConsensusAction>,
    ) -> Result<(), std::io::Error> {
        for action in consensus_actions {
            self.execute_action(action)?;
        }
        return Ok(());
    }

    pub fn run(&mut self) -> Result<(), std::io::Error> {
        //initial action to start the network

        //Start the sender/receiver
        let (node_tx, node_rx) = mpsc::channel::<NodeEvent>();
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
                NetworkEvent::Message { from, message } => match message {
                    Message::Consensus(consensus_message) => match consensus_message {
                        _ => {
                            let _ = node_tx.send(NodeEvent::Consensus(ConsensusEvent::Message {
                                from: from,
                                message: consensus_message,
                            }));
                        }
                    },
                    Message::Control(_) => {}
                },
                NetworkEvent::IncomingPeer { node_id, stream } => {
                    let _ = node_tx.send(NodeEvent::Control(ControlEvent::PeerConnected {
                        node_id,
                        tcp_stream: stream,
                    }));
                }
                _ => {}
            };
        });

        self.maybe_start_consensus()?;
        loop {
            let node_event = node_rx.recv().expect("Node event channel closed");

            //Initial consensus event

            match node_event {
                NodeEvent::Consensus(consensus_event) => {
                    let consensus_actions = self.consensus.on_event(consensus_event);
                    let _ = self.execute_consensus_actions(&consensus_actions);
                }
                NodeEvent::Control(control_event) => {
                    let _ = self.handle_control_event(control_event);
                }
                _ => {
                    break;
                }
            }
            self.maybe_start_consensus()?;
        }
        Ok(())
    }

    pub fn maybe_start_consensus(&mut self)->Result<(), std::io::Error>{
        
        if !self.consensus_started && self.network.is_ready() {
            let actions = self.consensus.on_start();
            self.execute_consensus_actions(&actions)?;
            let node_id = self.node_id;
            println!("READY node {node_id}");
            self.consensus_started = true;
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
        }
    }
}
