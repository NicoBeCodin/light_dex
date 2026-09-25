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

    pub event_tx: Sender<NetworkEvent>,
    pub event_rx: Option<Receiver<NetworkEvent>>,
}

#[derive(Debug)]
pub enum NetworkEvent {
    Message {
        from: NodeId,
        message: Message,
    },

    PeerConnected {
        node_id: NodeId,
    },

    IncomingPeer {
        node_id: NodeId,
        stream: TcpStream,
    },

    ClientTransaction(Transaction),

    ClientControl {
        message: ControlMessage,
        stream: TcpStream,
    },
}

pub fn spawn_reader(
    peer_id: NodeId,
    mut reader: BufReader<TcpStream>,
    tx: Sender<NetworkEvent>,
) {
    loop {
        let mut buffer = String::new();

        let n = match reader.read_line(&mut buffer) {
            Ok(n) => n,
            Err(error) => {
                eprintln!("Peer {peer_id} read error: {error}");
                break;
            }
        };

        if n == 0 {
            break;
        }

        let message = match Message::from_str(buffer) {
            Ok(message) => message,

            Err(error) => {
                eprintln!(
                    "Failed decoding message from peer {peer_id}: {error}"
                );
                continue;
            }
        };

        if tx
            .send(NetworkEvent::Message {
                from: peer_id,
                message,
            })
            .is_err()
        {
            break;
        }
    }
}

pub fn spawn_client_reader(
    mut reader: BufReader<TcpStream>,
    tx: Sender<NetworkEvent>,
) {
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

            Message::Control(message) => {
                let stream = match reader.get_ref().try_clone() {
                    Ok(stream) => stream,
                    Err(_) => break,
                };

                if tx
                    .send(NetworkEvent::ClientControl {
                        message,
                        stream,
                    })
                    .is_err()
                {
                    break;
                }
            }

            Message::Consensus(_) => {
                // External clients aren't allowed to inject
                // consensus messages.
            }
        }
    }
}

impl Network {
    pub fn new(
        node_id: NodeId,
        parent_address: SocketAddr,
    ) -> Result<Network, std::io::Error> {
        let listener = TcpListener::bind("127.0.0.1:0")?;

        let (event_tx, event_rx) = mpsc::channel::<NetworkEvent>();

        Ok(Network {
            node_id,
            parent_address,
            listener,

            peers_address: HashMap::new(),
            peers_stream: HashMap::new(),

            event_tx,
            event_rx: Some(event_rx),
        })
    }

    pub fn take_event_receiver(&mut self) -> Option<Receiver<NetworkEvent>> {
        self.event_rx.take()
    }

    pub fn start(&mut self) -> Result<(), std::io::Error> {
        // One TCP connection per pair:
        // the lower ID initiates toward the higher ID.
        for (peer_id, peer_address) in &self.peers_address {
            if peer_id > &self.node_id {
                let peer_stream = TcpStream::connect(peer_address)?;

                let mut write_stream = peer_stream.try_clone()?;

                let mut hello =
                    Message::Control(ControlMessage::Hello {
                        node_id: self.node_id,
                    })
                    .to_str()?;

                hello.push('\n');

                write_stream.write_all(hello.as_bytes())?;

                self.peers_stream.insert(*peer_id, write_stream);

                let _ = self.event_tx.send(
                    NetworkEvent::PeerConnected {
                        node_id: *peer_id,
                    },
                );

                let tx = self.event_tx.clone();
                let peer_id = *peer_id;

                thread::spawn(move || {
                    let reader = BufReader::new(peer_stream);

                    spawn_reader(peer_id, reader, tx);
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
                Err(error) => {
                    eprintln!("Invalid initial message: {error}");
                    continue;
                }
            };

            match message {
                //
                // Peer connection
                //
                Message::Control(ControlMessage::Hello { node_id }) => {
                    let _ =
                        event_tx.send(NetworkEvent::IncomingPeer {
                            node_id,
                            stream: write_stream,
                        });

                    let tx = event_tx.clone();

                    // Important: continue using the SAME BufReader.
                    thread::spawn(move || {
                        spawn_reader(node_id, reader, tx);
                    });
                }

                //
                // External client transaction
                //
                Message::Transaction(transaction) => {
                    let _ = event_tx.send(
                        NetworkEvent::ClientTransaction(transaction),
                    );

                    let tx = event_tx.clone();

                    thread::spawn(move || {
                        spawn_client_reader(reader, tx);
                    });
                }

                //
                // External control-plane request
                //
                Message::Control(message) => {
                    let _ =
                        event_tx.send(NetworkEvent::ClientControl {
                            message,
                            stream: write_stream,
                        });
                }

                Message::Consensus(_) => {
                    // Consensus traffic must arrive through an
                    // established peer connection.
                }
            }
        });

        Ok(())
    }

    pub fn send(
        &mut self,
        peer: &NodeId,
        payload: &[u8],
    ) -> Result<(), std::io::Error> {
        let result = match self.peers_stream.get_mut(peer) {
            Some(stream)=> {
                stream.write_all(payload)
            },
            None => {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "peer not found",
            ))
                
            }
        };
        if let Err(error) = result {
        self.peers_stream.remove(peer);
        return Err(error);
        }
        Ok(())
    }

    pub fn broadcast(
        &mut self,
        payload: &[u8],
    ) -> Result<(), std::io::Error> {
        // If a node was killed, don't let one broken connection
        // terminate consensus on every surviving replica.
        self.peers_stream.retain(|peer_id, stream| {
            match stream.write_all(payload) {
                Ok(()) => true,

                Err(error) => {
                    eprintln!(
                        "Dropping peer {peer_id} after write error: {error}"
                    );

                    false
                }
            }
        });

        Ok(())
    }

    pub fn add_peer(
        &mut self,
        node_id: NodeId,
        stream: TcpStream,
    ) -> Result<(), std::io::Error> {
        self.peers_stream.entry(node_id).or_insert(stream);

        Ok(())
    }

    pub fn is_ready(&self) -> bool {
        self.peers_stream.len() == self.peers_address.len()
    }
}
