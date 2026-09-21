use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

use crate::{
    blockchain::Transaction,
    message::Message,
    node::{ControlMessage, NodeId},
};

#[derive(Debug)]
pub struct Network {
    pub node_id: NodeId,
    pub parent_address: SocketAddr,
    pub listener: TcpListener,
    pub peers_address: HashMap<NodeId, SocketAddr>,
    pub peers_stream: HashMap<NodeId, TcpStream>,

    pub event_tx: mpsc::Sender<NetworkEvent>,
    pub event_rx: Option<mpsc::Receiver<NetworkEvent>>,
}

#[derive(Debug)]
pub enum NetworkEvent {
    Message { from: NodeId, message: Message },
    PeerConnected { node_id: NodeId },
    IncomingPeer { node_id: NodeId, stream: TcpStream },
    ClientTransaction(Transaction),
    ClientControl(ControlMessage),
}

pub fn spawn_reader(peer_id: NodeId, mut reader: BufReader<TcpStream>, tx: Sender<NetworkEvent>) {
    // let mut reader = BufReader::new(stream);
    loop {
        // let buffer: &mut Vec<u8> = &mut Vec::new();
        let mut buffer = String::new();
        let _ = reader.read_line(&mut buffer);
        if buffer.len() == 0 {
            break;
        }
        let message = Message::from_str(buffer).expect("Failed deserializing message");
        let network_event = NetworkEvent::Message {
            message,
            from: peer_id,
        };
        let _ = tx.send(network_event);
    }
}

pub fn spawn_transaction_reader(mut reader: BufReader<TcpStream>, tx: Sender<NetworkEvent>) {
    loop {
        let mut buffer = String::new();

        let n = match reader.read_line(&mut buffer) {
            Ok(n) => n,
            Err(_) => break,
        };

        if n == 0 {
            break;
        }

        let message = match Message::from_str(buffer) {
            Ok(message) => message,
            Err(_) => continue,
        };

        match message {
            Message::Transaction(transaction) => {
                if tx
                    .send(NetworkEvent::ClientTransaction(transaction))
                    .is_err()
                {
                    break;
                }
            }
            Message::Control(control_message) => {
                tx.send(NetworkEvent::ClientControl(control_message));
            }
            _ => {
                // Client connections are only expected to send transactions.
            }
        }
    }
}
impl Network {
    pub fn new(node_id: NodeId, parent_address: SocketAddr) -> Result<Network, std::io::Error> {
        let listener = TcpListener::bind("127.0.0.1:0").expect("Failed binding");
        let peers_address = HashMap::new();
        let peers = HashMap::new();
        let (event_tx, event_rx) = mpsc::channel::<NetworkEvent>();

        Ok(Network {
            node_id,
            parent_address,
            listener,
            peers_address,
            peers_stream: peers,
            event_tx,
            event_rx: Some(event_rx),
        })
    }
    pub fn take_event_receiver(&mut self) -> Option<Receiver<NetworkEvent>> {
        self.event_rx.take()
    }

    pub fn start(&mut self) -> Result<(), std::io::Error> {
        //Create a TCP connection with everybody (TCP is two ways so only needs one)
        for (peer_id, peer_address) in &self.peers_address {
            if peer_id > &self.node_id {
                let peer_stream =
                    TcpStream::connect(peer_address).expect("Failed connecting to peer stream");

                let mut write_stream = peer_stream.try_clone()?;
                let hello_message = ControlMessage::Hello {
                    node_id: self.node_id,
                };
                let mut hello_buffer = Message::Control(hello_message).to_str()?;
                hello_buffer.push('\n');
                write_stream.write_all(&hello_buffer.as_bytes())?;
                self.peers_stream.insert(*peer_id, write_stream);

                let connection_event = NetworkEvent::PeerConnected { node_id: *peer_id };
                self.event_tx.send(connection_event);

                let tx_clone = self.event_tx.clone();
                let peer_stream_clone = peer_stream.try_clone()?;
                let peer_id_clone = peer_id.clone();

                //Create a reader thread
                thread::spawn(move || {
                    let mut reader = BufReader::new(peer_stream_clone);
                    spawn_reader(peer_id_clone, reader, tx_clone);
                });
            }
        }
        let listener = self.listener.try_clone()?;
        let event_tx = self.event_tx.clone();

        thread::spawn(move || loop {
            let (stream, _) = match listener.accept() {
                Ok(value) => value,
                Err(_) => break,
            };

            let write_stream = match stream.try_clone() {
                Ok(stream) => stream,
                Err(_) => continue,
            };

            let mut reader = BufReader::new(stream);

            let mut buffer = String::new();

            let n = match reader.read_line(&mut buffer) {
                Ok(n) => n,
                Err(_) => continue,
            };

            if n == 0 {
                continue;
            }

            let message = match Message::from_str(buffer) {
                Ok(message) => message,
                Err(_) => continue,
            };

            match message {
                Message::Control(ControlMessage::Hello { node_id }) => {
                    let _ = event_tx.send(NetworkEvent::IncomingPeer {
                        node_id,
                        stream: write_stream,
                    });

                    let tx = event_tx.clone();

                    thread::spawn(move || {
                        spawn_reader(node_id, reader, tx);
                    });
                }

                Message::Transaction(transaction) => {
                    // The first transaction was already consumed above.
                    let _ = event_tx.send(NetworkEvent::ClientTransaction(transaction));

                    // Keep reading more transactions from this same connection.
                    let tx = event_tx.clone();

                    thread::spawn(move || {
                        spawn_transaction_reader(reader, tx);
                    });
                }
                Message::Control(control_message) => {
                    let _ = event_tx.send(NetworkEvent::ClientControl(control_message));
                }

                Message::Consensus(_) => {
                    // Invalid: consensus traffic must arrive over an
                    // established peer connection.
                }
            }
        });
        Ok(())
    }

    pub fn send(&mut self, peer: &NodeId, payload: &[u8]) -> Result<(), std::io::Error> {
        match self.peers_stream.get(&peer) {
            Some(mut peer_stream) => {
                peer_stream.write_all(&payload)?;

                Ok(())
            }
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "peer not found",
                ));
            }
        }
    }

    pub fn broadcast(&mut self, payload: &[u8]) -> Result<(), std::io::Error> {
        for (_, mut stream) in &self.peers_stream {
            stream.write_all(&payload)?;
        }
        Ok(())
    }

    pub fn add_peer(&mut self, node_id: NodeId, stream: TcpStream) -> Result<(), std::io::Error> {
        if self.peers_stream.get(&node_id).is_none() {
            println!("Added peer: {node_id}");
            self.peers_stream.insert(node_id, stream);
        }

        Ok(())
    }

    pub fn is_ready(&self) -> bool {
        self.peers_address.len() == self.peers_stream.len()
    }

    // pub fn receive_event(&self) -> Result<NetworkEvent, RecvError> {
    //     self.event_rx.recv()
    // }
}
