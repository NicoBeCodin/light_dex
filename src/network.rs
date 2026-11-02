use crate::network_writer::PeerTransportStats;
use crate::priority::{self, Receiver, Sender as SyncSender};
use ed25519_dalek::SigningKey;
use std::sync::atomic::Ordering;
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use crate::{
    blockchain::Transaction,
    crypto::SignedTransaction,
    crypto::{SessionManifest, random_nonce, sign_handshake, verify_handshake},
    message::{Message, sign_line, verify_line},
    node::{ControlMessage, NodeId},
};

/// Physical peer connections are independent so a bulk frame already being
/// written cannot hold a vote or view-change message behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeerLane {
    Control,
    Bulk,
}

impl PeerLane {
    fn stage(self, base: &str) -> String {
        match self {
            Self::Control => base.to_owned(),
            Self::Bulk => format!("{base}-bulk"),
        }
    }
}

pub struct Network {
    pub node_id: NodeId,
    pub parent_address: SocketAddr,
    pub listener: TcpListener,
    pub advertised_address: SocketAddr,
    pub auth_key: [u8; 32],
    signing: Option<(Arc<SessionManifest>, SigningKey)>,

    pub peers_address: HashMap<NodeId, SocketAddr>,
    pub peers_stream: HashMap<NodeId, crate::network_writer::PeerWriter>,
    pub bulk_peers_stream: HashMap<NodeId, crate::network_writer::PeerWriter>,
    pub gossip_shed_bytes: u64,
    pub transport_stats: Arc<PeerTransportStats>,
    last_reconnect_attempt: Option<Instant>,
    reconnecting: std::collections::HashSet<(NodeId, PeerLane)>,
    reconnect_tx: std::sync::mpsc::Sender<(
        NodeId,
        PeerLane,
        std::io::Result<(TcpStream, BufReader<TcpStream>)>,
    )>,
    reconnect_rx: std::sync::mpsc::Receiver<(
        NodeId,
        PeerLane,
        std::io::Result<(TcpStream, BufReader<TcpStream>)>,
    )>,

    pub event_tx: SyncSender<NetworkEvent>,
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
        lane: PeerLane,
        stream: TcpStream,
    },

    ClientTransaction(Transaction),
    SignedClientTransaction(SignedTransaction),
    SubmitTransaction {
        transaction: SignedTransaction,
        stream: TcpStream,
        received: Instant,
    },

    ClientControl {
        message: ControlMessage,
        stream: TcpStream,
    },
}

impl priority::Prioritized for NetworkEvent {
    fn priority(&self) -> bool {
        if matches!(self, Self::Message { message: Message::SignedConsensus(signed), .. }
            if signed.message.is_bulk())
        {
            return false;
        }
        if matches!(self, Self::Message { message: Message::Consensus(message), .. }
            if message.is_bulk())
        {
            return false;
        }
        matches!(
            self,
            Self::Message {
                message: Message::Consensus(_) | Message::SignedConsensus(_),
                ..
            } | Self::IncomingPeer { .. }
                | Self::PeerConnected { .. }
                | Self::ClientControl {
                    message: ControlMessage::Shutdown,
                    ..
                }
        )
    }
}

pub fn spawn_reader(
    peer_id: NodeId,
    mut reader: BufReader<TcpStream>,
    tx: SyncSender<NetworkEvent>,
    auth_key: [u8; 32],
    stats: Arc<PeerTransportStats>,
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
        if !buffer.ends_with('\n') {
            stats.truncated_frames.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "Peer {peer_id} closed during a message ({} bytes)",
                buffer.len()
            );
            break;
        }

        let message = match verify_line::<Message>(&buffer, &auth_key) {
            Ok(message) => message,

            Err(error) => {
                eprintln!("Failed decoding message from peer {peer_id}: {error}");
                let _ = tx.send(NetworkEvent::AuthenticationRejected);
                continue;
            }
        };

        if matches!(
            &message,
            Message::SignedTransaction(_)
                | Message::SignedTransactionBatch { .. }
                | Message::Transaction(_)
        ) {
            // A full transaction lane must not block this peer's later votes.
            let _ = tx.try_send(NetworkEvent::Message {
                from: peer_id,
                message,
            });
            continue;
        }
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
    tx: SyncSender<NetworkEvent>,
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
                if tx
                    .send(NetworkEvent::SignedClientTransaction(transaction))
                    .is_err()
                {
                    break;
                }
            }

            Message::SubmitTransaction(transaction) => {
                let Ok(stream) = reader.get_ref().try_clone() else {
                    break;
                };
                enqueue_submission(&tx, transaction, stream, &auth_key);
            }

            Message::Control(message) => {
                let stream = match reader.get_ref().try_clone() {
                    Ok(stream) => stream,
                    Err(_) => break,
                };

                if tx
                    .send(NetworkEvent::ClientControl { message, stream })
                    .is_err()
                {
                    break;
                }
            }

            Message::Consensus(_)
            | Message::SignedConsensus(_)
            | Message::SignedTransactionBatch { .. }
            | Message::PeerHandshake(_) => {
                // External clients aren't allowed to inject
                // consensus messages.
            }
        }
    }
}

fn enqueue_submission(
    tx: &SyncSender<NetworkEvent>,
    transaction: SignedTransaction,
    stream: TcpStream,
    key: &[u8; 32],
) {
    if let Err(error) = tx.try_send(NetworkEvent::SubmitTransaction {
        transaction,
        stream,
        received: Instant::now(),
    }) {
        let event = match error {
            std::sync::mpsc::TrySendError::Full(event)
            | std::sync::mpsc::TrySendError::Disconnected(event) => event,
        };
        if let NetworkEvent::SubmitTransaction {
            transaction,
            mut stream,
            ..
        } = event
        {
            reply_admission(
                &mut stream,
                &transaction,
                false,
                Some("ingress queue full"),
                key,
            );
        }
    }
}

pub(crate) fn reply_admission(
    stream: &mut TcpStream,
    transaction: &SignedTransaction,
    accepted: bool,
    error: Option<&str>,
    key: &[u8; 32],
) {
    reply_admission_with_leader(stream, transaction, accepted, error, key, None);
}

pub(crate) fn reply_admission_with_leader(
    stream: &mut TcpStream,
    transaction: &SignedTransaction,
    accepted: bool,
    error: Option<&str>,
    key: &[u8; 32],
    leader: Option<(u64, u64)>,
) {
    let reply = crate::message::AdmissionReply {
        digest: crate::message::hex_encode(&transaction.transaction.to_digest()),
        accepted,
        error: error.map(str::to_owned),
        leader,
    };
    let _ = stream.set_write_timeout(Some(Duration::from_millis(50)));
    if let Ok(line) = sign_line(&reply, key) {
        let _ = stream.write_all(line.as_bytes());
    }
}

impl Network {
    pub fn attach_signing(
        &mut self,
        manifest: Arc<SessionManifest>,
        key: SigningKey,
    ) -> std::io::Result<()> {
        if key.verifying_key() != manifest.verifier(true, self.node_id)? {
            return Err(std::io::Error::other("network validator key mismatch"));
        }
        self.signing = Some((manifest, key));
        Ok(())
    }

    fn dial_peer(
        node_id: NodeId,
        auth_key: [u8; 32],
        signing: Option<&(Arc<SessionManifest>, SigningKey)>,
        peer_id: NodeId,
        lane: PeerLane,
        address: &SocketAddr,
    ) -> std::io::Result<(TcpStream, BufReader<TcpStream>)> {
        let stream = TcpStream::connect_timeout(address, Duration::from_secs(5))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut writer = stream.try_clone()?;
        let mut reader = BufReader::new(stream);
        if let Some((manifest, key)) = signing {
            let nonce = random_nonce()?;
            let hello = sign_handshake(&lane.stage("hello"), node_id, &nonce, "", manifest, key)?;
            writer.write_all(sign_line(&Message::PeerHandshake(hello), &auth_key)?.as_bytes())?;
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let Message::PeerHandshake(ack) = verify_line::<Message>(&line, &auth_key)? else {
                return Err(std::io::Error::other(
                    "peer did not acknowledge signed hello",
                ));
            };
            verify_handshake(&ack, &lane.stage("ack"), manifest)?;
            if ack.node_id != peer_id || ack.echo != nonce {
                return Err(std::io::Error::other("peer handshake challenge mismatch"));
            }
            let finish = sign_handshake(
                &lane.stage("finish"),
                node_id,
                &nonce,
                &ack.nonce,
                manifest,
                key,
            )?;
            writer.write_all(sign_line(&Message::PeerHandshake(finish), &auth_key)?.as_bytes())?;
        } else {
            let hello = sign_line(
                &Message::Control(match lane {
                    PeerLane::Control => ControlMessage::Hello { node_id },
                    PeerLane::Bulk => ControlMessage::BulkHello { node_id },
                }),
                &auth_key,
            )?;
            writer.write_all(hello.as_bytes())?;
        }
        reader.get_mut().set_read_timeout(None)?;
        writer.set_write_timeout(None)?;
        Ok((writer, reader))
    }

    pub fn reconnect_missing(&mut self) {
        self.peers_stream.retain(|_, writer| writer.is_alive());
        self.bulk_peers_stream.retain(|_, writer| writer.is_alive());
        while let Ok((peer_id, lane, result)) = self.reconnect_rx.try_recv() {
            self.reconnecting.remove(&(peer_id, lane));
            if let Ok((stream, reader)) = result {
                // The peer may have connected to us while our dial was pending.
                let peers = match lane {
                    PeerLane::Control => &mut self.peers_stream,
                    PeerLane::Bulk => &mut self.bulk_peers_stream,
                };
                if peers.contains_key(&peer_id) {
                    continue;
                }
                if let Ok(writer) = crate::network_writer::PeerWriter::with_stats(
                    stream,
                    self.transport_stats.clone(),
                ) {
                    peers.insert(peer_id, writer);
                    self.transport_stats
                        .reconnects
                        .fetch_add(1, Ordering::Relaxed);
                    let _ = self
                        .event_tx
                        .send(NetworkEvent::PeerConnected { node_id: peer_id });
                    let tx = self.event_tx.clone();
                    let key = self.auth_key;
                    let stats = self.transport_stats.clone();
                    thread::spawn(move || spawn_reader(peer_id, reader, tx, key, stats));
                }
            }
        }
        if self
            .last_reconnect_attempt
            .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
        {
            return;
        }
        self.last_reconnect_attempt = Some(Instant::now());
        let mut missing = Vec::new();
        for peer_id in self.peers_address.keys().copied() {
            if !self.peers_stream.contains_key(&peer_id) {
                missing.push((peer_id, PeerLane::Control));
            }
            if !self.bulk_peers_stream.contains_key(&peer_id) {
                missing.push((peer_id, PeerLane::Bulk));
            }
        }
        for (peer_id, lane) in missing {
            // Match bootstrap ownership. Two simultaneous opposite-direction
            // reconnects would otherwise replace and close each other's socket.
            if peer_id < self.node_id {
                continue;
            }
            if !self.reconnecting.insert((peer_id, lane)) {
                continue;
            }
            let address = self.peers_address[&peer_id];
            let tx = self.reconnect_tx.clone();
            let signing = self.signing.clone();
            let key = self.auth_key;
            let id = self.node_id;
            thread::spawn(move || {
                let _ = tx.send((
                    peer_id,
                    lane,
                    Self::dial_peer(id, key, signing.as_ref(), peer_id, lane, &address),
                ));
            });
        }
    }
    pub fn new(
        node_id: NodeId,
        parent_address: SocketAddr,
        auth_key: [u8; 32],
    ) -> Result<Network, std::io::Error> {
        Self::new_with_addresses(
            node_id,
            parent_address,
            auth_key,
            "127.0.0.1:0".parse().unwrap(),
            None,
        )
    }

    pub fn new_with_addresses(
        node_id: NodeId,
        parent_address: SocketAddr,
        auth_key: [u8; 32],
        listen_address: SocketAddr,
        advertise_address: Option<SocketAddr>,
    ) -> Result<Network, std::io::Error> {
        let listener = TcpListener::bind(listen_address)?;
        let advertised_address = advertise_address.unwrap_or(listener.local_addr()?);

        let (event_tx, event_rx) = priority::channel::<NetworkEvent>(8_192);
        let (reconnect_tx, reconnect_rx) = std::sync::mpsc::channel();

        Ok(Network {
            node_id,
            parent_address,
            listener,
            advertised_address,
            auth_key,
            signing: None,

            peers_address: HashMap::new(),
            peers_stream: HashMap::new(),
            bulk_peers_stream: HashMap::new(),
            gossip_shed_bytes: 0,
            transport_stats: Arc::new(PeerTransportStats::default()),
            reconnecting: std::collections::HashSet::new(),
            reconnect_tx,
            reconnect_rx,
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
        let transport_stats = self.transport_stats.clone();
        let clients = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // Initial bytes can take more than 500 ms through a WAN tunnel. A
        // small bounded pool keeps a slow first read from blocking peer votes,
        // health checks and catch-up requests behind it.
        let (incoming_tx, incoming_rx) = std::sync::mpsc::sync_channel::<TcpStream>(64);
        let incoming_rx = Arc::new(std::sync::Mutex::new(incoming_rx));
        thread::spawn(move || {
            loop {
                let (stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(_) => break,
                };
                let _ = incoming_tx.try_send(stream);
            }
        });
        for _ in 0..4 {
            let incoming_rx = incoming_rx.clone();
            let event_tx = event_tx.clone();
            let signing = signing.clone();
            let clients = clients.clone();
            let transport_stats = transport_stats.clone();
            thread::spawn(move || {
                loop {
                    let stream = match incoming_rx.lock().unwrap().recv() {
                        Ok(stream) => stream,
                        Err(_) => break,
                    };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = stream.set_nodelay(true);

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
                            let Some((manifest, key)) = &signing else {
                                continue;
                            };
                            let lane = match hello.stage.as_str() {
                                "hello" => PeerLane::Control,
                                "hello-bulk" => PeerLane::Bulk,
                                _ => continue,
                            };
                            if verify_handshake(&hello, &lane.stage("hello"), manifest).is_err()
                                || hello.node_id
                                    == node_id_for_key(&key, manifest).unwrap_or(u64::MAX)
                            {
                                continue;
                            }
                            let Ok(nonce) = random_nonce() else {
                                continue;
                            };
                            let self_id = node_id_for_key(key, manifest).unwrap_or(u64::MAX);
                            let Ok(ack) = sign_handshake(
                                &lane.stage("ack"),
                                self_id,
                                &nonce,
                                &hello.nonce,
                                manifest,
                                key,
                            ) else {
                                continue;
                            };
                            let Ok(line) = sign_line(&Message::PeerHandshake(ack), &auth_key)
                            else {
                                continue;
                            };
                            let mut writer = write_stream;
                            if writer.write_all(line.as_bytes()).is_err() {
                                continue;
                            }
                            let mut finish_line = String::new();
                            if reader
                                .get_mut()
                                .set_read_timeout(Some(Duration::from_secs(5)))
                                .is_err()
                                || reader.read_line(&mut finish_line).is_err()
                            {
                                continue;
                            }
                            let Ok(Message::PeerHandshake(finish)) =
                                verify_line::<Message>(&finish_line, &auth_key)
                            else {
                                continue;
                            };
                            if verify_handshake(&finish, &lane.stage("finish"), manifest).is_err()
                                || finish.node_id != hello.node_id
                                || finish.nonce != hello.nonce
                                || finish.echo != nonce
                            {
                                continue;
                            }
                            let _ = reader.get_mut().set_read_timeout(None);
                            let _ = event_tx.send(NetworkEvent::IncomingPeer {
                                node_id: hello.node_id,
                                lane,
                                stream: writer,
                            });
                            let tx = event_tx.clone();
                            let stats = transport_stats.clone();
                            thread::spawn(move || {
                                spawn_reader(hello.node_id, reader, tx, auth_key, stats)
                            });
                        }
                        //
                        // Peer connection
                        //
                        Message::Control(
                            hello @ (ControlMessage::Hello { .. }
                            | ControlMessage::BulkHello { .. }),
                        ) => {
                            if signing.is_some() {
                                continue;
                            }
                            let (node_id, lane) = match hello {
                                ControlMessage::Hello { node_id } => (node_id, PeerLane::Control),
                                ControlMessage::BulkHello { node_id } => (node_id, PeerLane::Bulk),
                                _ => unreachable!(),
                            };
                            let _ = event_tx.send(NetworkEvent::IncomingPeer {
                                node_id,
                                lane,
                                stream: write_stream,
                            });

                            let tx = event_tx.clone();
                            let stats = transport_stats.clone();

                            // Important: continue using the SAME BufReader.
                            thread::spawn(move || {
                                spawn_reader(node_id, reader, tx, auth_key, stats);
                            });
                        }

                        //
                        // External client transaction
                        //
                        Message::Transaction(transaction) => {
                            let _ = event_tx.send(NetworkEvent::ClientTransaction(transaction));
                        }

                        Message::SignedTransaction(transaction) => {
                            let _ =
                                event_tx.send(NetworkEvent::SignedClientTransaction(transaction));
                        }

                        Message::SignedTransactionBatch { .. } => {
                            let _ = event_tx.try_send(NetworkEvent::AuthenticationRejected);
                        }

                        Message::SubmitTransaction(transaction) => {
                            use std::sync::atomic::Ordering;
                            // Bound persistent submission workers independently of peers.
                            if clients
                                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                                    (n < 64).then_some(n + 1)
                                })
                                .is_err()
                            {
                                let mut stream = write_stream;
                                reply_admission(
                                    &mut stream,
                                    &transaction,
                                    false,
                                    Some("submission connections full"),
                                    &auth_key,
                                );
                                continue;
                            }
                            let clients = clients.clone();
                            let tx = event_tx.clone();
                            thread::spawn(move || {
                                struct Lease(Arc<std::sync::atomic::AtomicUsize>);
                                impl Drop for Lease {
                                    fn drop(&mut self) {
                                        self.0.fetch_sub(1, Ordering::AcqRel);
                                    }
                                }
                                let _lease = Lease(clients);
                                let _ = reader
                                    .get_mut()
                                    .set_read_timeout(Some(Duration::from_secs(10)));
                                enqueue_submission(&tx, transaction, write_stream, &auth_key);
                                spawn_client_reader(reader, tx, auth_key);
                            });
                        }

                        //
                        // External control-plane request
                        //
                        Message::Control(message) => {
                            let _ = event_tx.send(NetworkEvent::ClientControl {
                                message,
                                stream: write_stream,
                            });
                        }

                        Message::Consensus(_) | Message::SignedConsensus(_) => {
                            // Consensus traffic must arrive through an
                            // established peer connection.
                        }
                    }
                }
            });
        }

        // The accept loop is live. Connect asynchronously: a transient SSH
        // forward or peer startup delay must not terminate the validator.
        self.reconnect_missing();

        Ok(())
    }

    pub fn send(&mut self, peer: &NodeId, payload: &[u8]) -> Result<(), std::io::Error> {
        if !self.peers_stream.contains_key(peer) {
            self.reconnect_missing();
        }
        let result = match self.peers_stream.get_mut(peer) {
            Some(stream) => stream.send(Arc::from(payload)),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "peer not found",
            )),
        };
        if let Err(error) = result {
            if error.kind() != std::io::ErrorKind::WouldBlock {
                self.peers_stream.remove(peer);
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn broadcast(&mut self, payload: &[u8]) -> Result<(), std::io::Error> {
        self.reconnect_missing();
        let payload: Arc<[u8]> = Arc::from(payload);
        // If a node was killed, don't let one broken connection
        // terminate consensus on every surviving replica.
        self.peers_stream
            .retain(|peer_id, stream| match stream.send(payload.clone()) {
                Ok(()) => true,

                Err(error) => {
                    eprintln!("Dropping peer {peer_id} after write error: {error}");

                    false
                }
            });

        Ok(())
    }

    /// Bulk bodies use a distinct TCP connection. If that connection has not
    /// finished reconnecting, the control connection is a liveness fallback.
    pub fn send_bulk(&mut self, peer: &NodeId, payload: &[u8]) -> Result<(), std::io::Error> {
        self.send_bulk_shared(peer, Arc::from(payload))
    }

    fn send_bulk_shared(
        &mut self,
        peer: &NodeId,
        payload: Arc<[u8]>,
    ) -> Result<(), std::io::Error> {
        if !self.bulk_peers_stream.contains_key(peer) {
            self.reconnect_missing();
        }
        let on_bulk = self
            .bulk_peers_stream
            .get(peer)
            .is_some_and(|writer| writer.is_alive());
        if on_bulk {
            let result = self.bulk_peers_stream.get(peer).unwrap().send(payload);
            if result
                .as_ref()
                .is_err_and(|error| error.kind() != std::io::ErrorKind::WouldBlock)
            {
                self.bulk_peers_stream.remove(peer);
            }
            result
        } else {
            if !self.peers_stream.contains_key(peer) {
                self.reconnect_missing();
            }
            let result = self
                .peers_stream
                .get(peer)
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "peer not found"))?
                .send(payload);
            if result
                .as_ref()
                .is_err_and(|error| error.kind() != std::io::ErrorKind::WouldBlock)
            {
                self.peers_stream.remove(peer);
            }
            result
        }
    }

    pub fn broadcast_bulk(&mut self, payload: &[u8]) -> Result<(), std::io::Error> {
        self.reconnect_missing();
        let payload: Arc<[u8]> = Arc::from(payload);
        for peer in self.peers_address.keys().copied().collect::<Vec<_>>() {
            let _ = self.send_bulk_shared(&peer, payload.clone());
        }
        Ok(())
    }

    /// Early transaction availability is best effort. Reserve the bulk
    /// writer's capacity for ordered block bodies and branch repair.
    pub fn broadcast_gossip(&mut self, except: Option<NodeId>, payload: &[u8]) {
        const GOSSIP_QUEUE_BUDGET: usize = 256 * 1024;
        let payload: Arc<[u8]> = Arc::from(payload);
        for (peer, writer) in &self.bulk_peers_stream {
            if Some(*peer) == except {
                continue;
            }
            if writer.is_alive()
                && writer.queued_bytes().saturating_add(payload.len()) <= GOSSIP_QUEUE_BUDGET
            {
                if writer.send(payload.clone()).is_err() {
                    self.gossip_shed_bytes =
                        self.gossip_shed_bytes.saturating_add(payload.len() as u64);
                }
            } else {
                self.gossip_shed_bytes =
                    self.gossip_shed_bytes.saturating_add(payload.len() as u64);
            }
        }
    }

    pub fn add_peer(
        &mut self,
        node_id: NodeId,
        lane: PeerLane,
        stream: TcpStream,
    ) -> Result<(), std::io::Error> {
        let peers = match lane {
            PeerLane::Control => &mut self.peers_stream,
            PeerLane::Bulk => &mut self.bulk_peers_stream,
        };
        peers.insert(
            node_id,
            crate::network_writer::PeerWriter::with_stats(stream, self.transport_stats.clone())?,
        );

        Ok(())
    }

    pub fn is_ready(&self) -> bool {
        self.peers_stream.len() == self.peers_address.len()
    }
}

fn node_id_for_key(key: &SigningKey, manifest: &SessionManifest) -> Option<NodeId> {
    let encoded = crate::message::hex_encode(&key.verifying_key().to_bytes());
    manifest
        .validators
        .iter()
        .find(|(_, public)| **public == encoded)
        .map(|(id, _)| *id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn startup_waits_for_temporarily_unavailable_peer() {
        let unavailable = TcpListener::bind("127.0.0.1:0").unwrap();
        let peer_address = unavailable.local_addr().unwrap();
        drop(unavailable);
        let mut network = Network::new(0, peer_address, [19; 32]).unwrap();
        network.peers_address.insert(1, peer_address);
        network.start().unwrap();
        assert!(!network.is_ready());
    }

    #[test]
    fn delayed_wan_control_request_does_not_block_other_connections() {
        let key = [23; 32];
        let mut network = Network::new(0, "127.0.0.1:1".parse().unwrap(), key).unwrap();
        let address = network.listener.local_addr().unwrap();
        let events = network.take_event_receiver().unwrap();
        network.start().unwrap();
        let (received_tx, received_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            received_tx.send(events.recv()).unwrap();
            received_tx.send(events.recv()).unwrap();
        });
        let mut slow = TcpStream::connect(address).unwrap();
        let mut fast = TcpStream::connect(address).unwrap();
        fast.write_all(
            sign_line(&Message::Control(ControlMessage::Health), &key)
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        let first = received_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert!(matches!(
            first,
            NetworkEvent::ClientControl {
                message: ControlMessage::Health,
                ..
            }
        ));
        thread::sleep(Duration::from_millis(750));
        slow.write_all(
            sign_line(&Message::Control(ControlMessage::Health), &key)
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        let second = received_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert!(matches!(
            second,
            NetworkEvent::ClientControl {
                message: ControlMessage::Health,
                ..
            }
        ));
    }

    #[test]
    fn missing_peer_is_reconnected_with_authenticated_hello() {
        let peer_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        peer_listener.set_nonblocking(false).unwrap();
        let peer_address = peer_listener.local_addr().unwrap();
        let auth_key = [13; 32];
        let mut network = Network::new(0, peer_address, auth_key).unwrap();
        network.peers_address.insert(1, peer_address);
        network.reconnect_missing();
        let mut lanes = [false; 2];
        for _ in 0..2 {
            let (stream, _) = peer_listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            let hello: Message = verify_line(&line, &auth_key).unwrap();
            match hello {
                Message::Control(ControlMessage::Hello { node_id: 0 }) => lanes[0] = true,
                Message::Control(ControlMessage::BulkHello { node_id: 0 }) => lanes[1] = true,
                _ => panic!("unexpected peer hello"),
            }
        }
        assert_eq!(lanes, [true, true]);
        let deadline = Instant::now() + Duration::from_secs(1);
        while !network.peers_stream.contains_key(&1) && Instant::now() < deadline {
            network.reconnect_missing();
            thread::sleep(Duration::from_millis(1));
        }
        assert!(network.peers_stream.contains_key(&1));
    }
}
