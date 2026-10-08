use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
    sync::Arc,
};
use ed25519_dalek::SigningKey;

use crate::{
    blockchain::Transaction,
    crypto::SignedTransaction,
    crypto::{SessionManifest, random_nonce, sign_handshake, verify_handshake},
    message::{Message, sign_line, verify_line},
    node::{ControlMessage, NodeId},
};

pub struct Network {
    pub node_id: NodeId,
    pub parent_address: SocketAddr,
    pub listener: TcpListener,
    pub auth_key: [u8; 32],
    signing: Option<(Arc<SessionManifest>, SigningKey)>,

    pub peers_address: HashMap<NodeId, SocketAddr>,
    pub peers_stream: HashMap<NodeId, TcpStream>,
    last_reconnect_attempt: Option<Instant>,

    pub event_tx: Sender<NetworkEvent>,
    pub event_rx: Option<Receiver<NetworkEvent>>,
}

#[derive(Debug)]
pub enum NetworkEvent {
    AuthenticationRejected,
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
    SignedClientTransaction(SignedTransaction),

    ClientControl {
        message: ControlMessage,
        stream: TcpStream,
    },
}

pub fn spawn_reader(
    peer_id: NodeId,
    mut reader: BufReader<TcpStream>,
    tx: Sender<NetworkEvent>,
    auth_key: [u8; 32],
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

        let message = match verify_line::<Message>(&buffer, &auth_key) {
            Ok(message) => message,

            Err(error) => {
                eprintln!(
                    "Failed decoding message from peer {peer_id}: {error}"
                );
                let _ = tx.send(NetworkEvent::AuthenticationRejected);
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
    auth_key: [u8; 32],
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

        let message = match verify_line::<Message>(&buffer, &auth_key) {
            Ok(message) => message,
            Err(_) => {
                let _ = tx.send(NetworkEvent::AuthenticationRejected);
                continue;
            }
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

            Message::SignedTransaction(transaction) => {
                if tx.send(NetworkEvent::SignedClientTransaction(transaction)).is_err() { break; }
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

            Message::Consensus(_) | Message::SignedConsensus(_) | Message::PeerHandshake(_) => {
                // External clients aren't allowed to inject
                // consensus messages.
            }
        }
    }
}

impl Network {
    pub fn attach_signing(&mut self, manifest: Arc<SessionManifest>, key: SigningKey) -> std::io::Result<()> {
        if key.verifying_key() != manifest.verifier(true, self.node_id)? {
            return Err(std::io::Error::other("network validator key mismatch"));
        }
        self.signing = Some((manifest, key));
        Ok(())
    }

    fn connect_peer(&self, peer_id: NodeId, address: &SocketAddr) -> std::io::Result<(TcpStream, BufReader<TcpStream>)> {
        let stream = TcpStream::connect_timeout(address, Duration::from_secs(5))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut writer = stream.try_clone()?;
        let mut reader = BufReader::new(stream);
        if let Some((manifest, key)) = &self.signing {
            let nonce = random_nonce()?;
            let hello = sign_handshake("hello", self.node_id, &nonce, "", manifest, key)?;
            writer.write_all(sign_line(&Message::PeerHandshake(hello), &self.auth_key)?.as_bytes())?;
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let Message::PeerHandshake(ack) = verify_line::<Message>(&line, &self.auth_key)? else {
                return Err(std::io::Error::other("peer did not acknowledge signed hello"));
            };
            verify_handshake(&ack, "ack", manifest)?;
            if ack.node_id != peer_id || ack.echo != nonce { return Err(std::io::Error::other("peer handshake challenge mismatch")); }
            let finish = sign_handshake("finish", self.node_id, &nonce, &ack.nonce, manifest, key)?;
            writer.write_all(sign_line(&Message::PeerHandshake(finish), &self.auth_key)?.as_bytes())?;
        } else {
            let hello = sign_line(&Message::Control(ControlMessage::Hello { node_id: self.node_id }), &self.auth_key)?;
            writer.write_all(hello.as_bytes())?;
        }
        reader.get_mut().set_read_timeout(None)?;
        writer.set_write_timeout(None)?;
        Ok((writer, reader))
    }

    fn reconnect_peer(&mut self, peer_id: NodeId) -> Result<(), std::io::Error> {
        if self.peers_stream.contains_key(&peer_id) { return Ok(()); }
        let address = *self.peers_address.get(&peer_id).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "unknown peer")
        })?;
        let (stream, reader) = self.connect_peer(peer_id, &address)?;
        self.peers_stream.insert(peer_id, stream);
        let tx = self.event_tx.clone();
        let auth_key = self.auth_key;
        thread::spawn(move || spawn_reader(peer_id, reader, tx, auth_key));
        Ok(())
    }

    pub fn reconnect_missing(&mut self) {
        if self.last_reconnect_attempt.is_some_and(|at| at.elapsed() < Duration::from_secs(1)) { return; }
        self.last_reconnect_attempt = Some(Instant::now());
        let missing = self.peers_address.keys().filter(|id| !self.peers_stream.contains_key(id))
            .copied().collect::<Vec<_>>();
        for peer_id in missing {
            if let Err(error) = self.reconnect_peer(peer_id) {
                eprintln!("Could not reconnect peer {peer_id}: {error}");
            }
        }
    }
    pub fn new(
        node_id: NodeId,
        parent_address: SocketAddr,
        auth_key: [u8; 32],
    ) -> Result<Network, std::io::Error> {
        let listener = TcpListener::bind("127.0.0.1:0")?;

        let (event_tx, event_rx) = mpsc::channel::<NetworkEvent>();

        Ok(Network {
            node_id,
            parent_address,
            listener,
            auth_key,
            signing: None,

            peers_address: HashMap::new(),
            peers_stream: HashMap::new(),
            last_reconnect_attempt: None,

            event_tx,
            event_rx: Some(event_rx),
        })
    }

    pub fn take_event_receiver(&mut self) -> Option<Receiver<NetworkEvent>> {
        self.event_rx.take()
    }

    pub fn start(&mut self) -> Result<(), std::io::Error> {
        // Accept before dialing peers: otherwise lower-ID nodes may wait on
        // handshakes from higher-ID nodes that have not started accepting yet.
        let listener = self.listener.try_clone()?;
        let event_tx = self.event_tx.clone();
        let auth_key = self.auth_key;
        let signing = self.signing.clone();

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

            let message = match verify_line::<Message>(&buffer, &auth_key) {
                Ok(message) => message,
                Err(error) => {
                    eprintln!("Invalid initial message: {error}");
                    let _ = event_tx.send(NetworkEvent::AuthenticationRejected);
                    continue;
                }
            };

            match message {
                Message::PeerHandshake(hello) => {
                    let Some((manifest, key)) = &signing else { continue; };
                    if verify_handshake(&hello, "hello", manifest).is_err() || hello.node_id == node_id_for_key(&key, manifest).unwrap_or(u64::MAX) { continue; }
                    let Ok(nonce) = random_nonce() else { continue; };
                    let self_id = node_id_for_key(key, manifest).unwrap_or(u64::MAX);
                    let Ok(ack) = sign_handshake("ack", self_id, &nonce, &hello.nonce, manifest, key) else { continue; };
                    let Ok(line) = sign_line(&Message::PeerHandshake(ack), &auth_key) else { continue; };
                    let mut writer = write_stream;
                    if writer.write_all(line.as_bytes()).is_err() { continue; }
                    let mut finish_line = String::new();
                    if reader.get_mut().set_read_timeout(Some(Duration::from_secs(5))).is_err()
                        || reader.read_line(&mut finish_line).is_err() { continue; }
                    let Ok(Message::PeerHandshake(finish)) = verify_line::<Message>(&finish_line, &auth_key) else { continue; };
                    if verify_handshake(&finish, "finish", manifest).is_err()
                        || finish.node_id != hello.node_id || finish.nonce != hello.nonce || finish.echo != nonce { continue; }
                    let _ = reader.get_mut().set_read_timeout(None);
                    let _ = event_tx.send(NetworkEvent::IncomingPeer { node_id: hello.node_id, stream: writer });
                    let tx = event_tx.clone();
                    thread::spawn(move || spawn_reader(hello.node_id, reader, tx, auth_key));
                }
                //
                // Peer connection
                //
                Message::Control(ControlMessage::Hello { node_id }) => {
                    if signing.is_some() { continue; }
                    let _ =
                        event_tx.send(NetworkEvent::IncomingPeer {
                            node_id,
                            stream: write_stream,
                        });

                    let tx = event_tx.clone();

                    // Important: continue using the SAME BufReader.
                    thread::spawn(move || {
                        spawn_reader(node_id, reader, tx, auth_key);
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
                        spawn_client_reader(reader, tx, auth_key);
                    });
                }


                Message::SignedTransaction(transaction) => {
                    let _ = event_tx.send(NetworkEvent::SignedClientTransaction(transaction));
                    let tx = event_tx.clone();
                    thread::spawn(move || spawn_client_reader(reader, tx, auth_key));
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

                Message::Consensus(_) | Message::SignedConsensus(_) => {
                    // Consensus traffic must arrive through an
                    // established peer connection.
                }
            }
        });

        // One TCP connection per pair: the lower ID initiates toward the
        // higher ID. All nodes now have a live accept loop before this begins.
        for (peer_id, peer_address) in &self.peers_address {
            if peer_id > &self.node_id {
                let (write_stream, reader) = self.connect_peer(*peer_id, peer_address)?;
                self.peers_stream.insert(*peer_id, write_stream);
                let _ = self.event_tx.send(NetworkEvent::PeerConnected { node_id: *peer_id });
                let tx = self.event_tx.clone();
                let peer_id = *peer_id;
                let auth_key = self.auth_key;
                thread::spawn(move || spawn_reader(peer_id, reader, tx, auth_key));
            }
        }

        Ok(())
    }

    pub fn send(
        &mut self,
        peer: &NodeId,
        payload: &[u8],
    ) -> Result<(), std::io::Error> {
        if !self.peers_stream.contains_key(peer) { self.reconnect_missing(); }
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
        self.reconnect_missing();
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
        self.peers_stream.insert(node_id, stream);

        Ok(())
    }

    pub fn is_ready(&self) -> bool {
        self.peers_stream.len() == self.peers_address.len()
    }
}

fn node_id_for_key(key: &SigningKey, manifest: &SessionManifest) -> Option<NodeId> {
    let encoded = crate::message::hex_encode(&key.verifying_key().to_bytes());
    manifest.validators.iter().find(|(_, public)| **public == encoded).map(|(id, _)| *id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn missing_peer_is_reconnected_with_authenticated_hello() {
        let peer_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        peer_listener.set_nonblocking(false).unwrap();
        let peer_address = peer_listener.local_addr().unwrap();
        let auth_key = [13; 32];
        let mut network = Network::new(0, peer_address, auth_key).unwrap();
        network.peers_address.insert(1, peer_address);
        network.reconnect_missing();
        let (stream, _) = peer_listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let hello: Message = verify_line(&line, &auth_key).unwrap();
        assert!(matches!(hello, Message::Control(ControlMessage::Hello { node_id: 0 })));
        assert!(network.peers_stream.contains_key(&1));
    }
}
