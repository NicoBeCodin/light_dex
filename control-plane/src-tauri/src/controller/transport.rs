use super::*;

thread_local! {
    // Each submitting worker owns its sockets: no network I/O under the global
    // controller lock. Session key in the key prevents reuse across restarts.
    static SUBMISSIONS: std::cell::RefCell<HashMap<(SocketAddr, [u8; 32]), BufReader<TcpStream>>>
        = std::cell::RefCell::new(HashMap::new());
}

fn submit_with_ack(
    address: SocketAddr,
    signed: &light_dex::crypto::SignedTransaction,
    auth_key: &[u8; 32],
    timeout: Duration,
) -> io::Result<Option<(u64, NodeId)>> {
    let expected = hex_encode(&signed.transaction.to_digest());
    let line = sign_line(&Message::SubmitTransaction(signed.clone()), auth_key)?;
    SUBMISSIONS.with(|pool| {
        let mut pool = pool.borrow_mut();
        let key = (address, *auth_key);
        for attempt in 0..2 {
            if !pool.contains_key(&key) {
                if pool.len() >= 32 {
                    pool.clear();
                }
                let stream = TcpStream::connect_timeout(&address, timeout)?;
                stream.set_nodelay(true)?;
                stream.set_read_timeout(Some(timeout))?;
                stream.set_write_timeout(Some(timeout))?;
                pool.insert(key, BufReader::new(stream));
            }
            let result = (|| {
                let reader = pool.get_mut(&key).unwrap();
                reader.get_mut().write_all(line.as_bytes())?;
                let mut response = String::new();
                // Keep the SAME BufReader for subsequent frames.
                if reader.take(4096).read_line(&mut response)? == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "admission connection closed",
                    ));
                }
                let reply: light_dex::message::AdmissionReply =
                    verify_line(response.trim(), auth_key)?;
                if reply.digest != expected {
                    return Err(io::Error::other("admission digest mismatch"));
                }
                Ok(reply)
            })();
            match result {
                Ok(reply) if reply.accepted => return Ok(reply.leader),
                Ok(reply) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        reply.error.unwrap_or_else(|| "transaction rejected".into()),
                    ));
                }
                Err(error) => {
                    // Socket WouldBlock can mean a read timeout, not an admission
                    // rejection. Discard it so a delayed ACK cannot match a new tx.
                    pool.remove(&key);
                    if attempt == 1 {
                        return Err(error);
                    }
                }
            }
            // Reconnect after idle close or an ambiguous response. Reusing the
            // exact signed transaction is safe because admission is idempotent.
        }
        unreachable!()
    })
}

pub(super) fn resolve_target(
    state: &ControllerInner,
    target: &TargetNode,
) -> Option<(NodeId, SocketAddr)> {
    let id = resolve_target_id(state, target)?;
    Some((id, state.nodes.get(&id)?.address?))
}
pub(super) fn resolve_target_id(state: &ControllerInner, target: &TargetNode) -> Option<NodeId> {
    let alive = state
        .nodes
        .iter()
        .filter(|(_, node)| node.state != "dead" && node.address.is_some())
        .map(|(id, _)| *id)
        .collect::<Vec<_>>();
    if alive.is_empty() {
        return None;
    }
    match target {
        TargetNode::Node { node_id } => alive.contains(node_id).then_some(*node_id),
        TargetNode::Random => {
            let responsive = alive
                .iter()
                .copied()
                .filter(|id| state.nodes.get(id).is_some_and(|node| node.state == "ok"))
                .collect::<Vec<_>>();
            let candidates = if responsive.is_empty() {
                &alive
            } else {
                &responsive
            };
            Some(candidates[rand::rng().random_range(0..candidates.len())])
        }
        TargetNode::Primary => {
            // A majority of cached round-robin observations can all describe
            // the *previous* leader. Prefer the newest observed view instead.
            let observed = state
                .nodes
                .values()
                .filter(|node| node.state == "ok")
                .filter_map(|node| node.status.as_ref())
                .filter(|status| alive.contains(&status.current_primary))
                .max_by_key(|status| (status.current_view, status.chain_height))
                .map(|status| (status.current_view, status.current_primary));
            observed
                .into_iter()
                .chain(state.leader_hint)
                .filter(|(_, id)| alive.contains(id))
                .max_by_key(|(view, _)| *view)
                .map(|(_, id)| id)
        }
    }
}

pub(super) fn send_signed_transaction(
    shared: &Shared,
    address: SocketAddr,
    transaction: Transaction,
    auth_key: &[u8; 32],
) -> io::Result<()> {
    let (manifest, key, timeout) = {
        let state = shared
            .lock()
            .map_err(|_| io::Error::other("controller lock poisoned"))?;
        // Backpressure belongs to the ingress node. A lagging follower can
        // recover independently; it must not stop a healthy quorum from
        // accepting transactions through a different node.
        let overloaded = state
            .nodes
            .values()
            .filter(|node| node.state == "ok" && node.address == Some(address))
            .filter_map(|node| node.status.as_ref().map(|status| (node, status)))
            .any(|(node, status)| {
                status.mempool_size >= 8_192
                    || (status.mempool_size >= 512
                        && node.last_commit_seen.elapsed() >= Duration::from_secs(5))
            });
        if overloaded {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "validator backlog high or commits stalled",
            ));
        }
        let manifest = state
            .signing_manifest
            .as_ref()
            .ok_or_else(|| io::Error::other("no active signing session"))?;
        let slot = state
            .wallet_slots
            .get(&transaction.account_id())
            .copied()
            .or_else(|| address_slot(&state, transaction.account_id()))
            .ok_or_else(|| io::Error::other("sender address is not in the controller wallet"))?;
        let key = state
            .account_keys
            .get(&slot)
            .ok_or_else(|| io::Error::other("account has no signing key"))?;
        let timeout = if state
            .nodes
            .values()
            .any(|node| node.address == Some(address) && node.remote.is_some())
        {
            Duration::from_secs(3)
        } else {
            Duration::from_millis(500)
        };
        (manifest.clone(), key.clone(), timeout)
    };
    let signed = sign_transaction(transaction, &manifest, &key)?;
    let hint = submit_with_ack(address, &signed, auth_key, timeout)?;
    if let Some(hint) = hint {
        let mut state = shared
            .lock()
            .map_err(|_| io::Error::other("controller lock poisoned"))?;
        if state.auth_key == *auth_key
            && state.leader_hint.is_none_or(|previous| hint.0 > previous.0)
        {
            state.leader_hint = Some(hint);
        }
    }
    Ok(())
}

/// Retry the same signed digest on one alternate responsive validator when a
/// random target disappears. Deduplication and account nonces make this safe
/// even if the first connection failed after the validator received the bytes.
pub(super) fn send_signed_transaction_with_failover(
    shared: &Shared,
    first: (NodeId, SocketAddr),
    random_target: bool,
    transaction: Transaction,
    auth_key: &[u8; 32],
) -> io::Result<NodeId> {
    let mut target = first;
    for attempt in 0..2 {
        match send_signed_transaction(shared, target.1, transaction.clone(), auth_key) {
            Ok(()) => return Ok(target.0),
            Err(error)
                if random_target
                    && attempt == 0
                    && matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::BrokenPipe
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::NotConnected
                    ) =>
            {
                let alternate = {
                    let mut state = shared.lock().unwrap();
                    if let Some(node) = state.nodes.get_mut(&target.0) {
                        node.state = "unresponsive".into();
                    }
                    let responsive = state
                        .nodes
                        .iter()
                        .filter_map(|(&id, node)| {
                            (id != target.0 && node.state == "ok").then_some((id, node.address?))
                        })
                        .collect::<Vec<_>>();
                    let available = if responsive.is_empty() {
                        state
                            .nodes
                            .iter()
                            .filter_map(|(&id, node)| {
                                (id != target.0 && node.state != "dead")
                                    .then_some((id, node.address?))
                            })
                            .collect::<Vec<_>>()
                    } else {
                        responsive
                    };
                    (!available.is_empty())
                        .then(|| available[rand::rng().random_range(0..available.len())])
                };
                let Some(next) = alternate else {
                    return Err(error);
                };
                target = next;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("two attempts always return")
}

pub(super) fn send_message(
    address: SocketAddr,
    message: &Message,
    timeout: Duration,
    auth_key: &[u8; 32],
) -> io::Result<()> {
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_write_timeout(Some(timeout))?;
    let data = sign_line(message, auth_key)?;
    stream.write_all(data.as_bytes())
}
pub(super) fn request_status(
    address: SocketAddr,
    auth_key: &[u8; 32],
    timeout: Duration,
) -> io::Result<NodeStatus> {
    request_status_message(address, auth_key, timeout, ControlMessage::Status)
}
pub(super) fn request_health(
    address: SocketAddr,
    auth_key: &[u8; 32],
    timeout: Duration,
) -> io::Result<NodeStatus> {
    request_status_message(address, auth_key, timeout, ControlMessage::Health)
}
pub(super) fn request_scoped_status(
    address: SocketAddr,
    auth_key: &[u8; 32],
    timeout: Duration,
    accounts: Vec<Address>,
) -> io::Result<NodeStatus> {
    request_status_message(
        address,
        auth_key,
        timeout,
        ControlMessage::StatusAccounts { accounts },
    )
}
fn request_status_message(
    address: SocketAddr,
    auth_key: &[u8; 32],
    timeout: Duration,
    message: ControlMessage,
) -> io::Result<NodeStatus> {
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let request = sign_line(&Message::Control(message), auth_key)?;
    stream.write_all(request.as_bytes())?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    verify_line(response.trim(), auth_key)
}
pub(super) fn request_read_api(
    address: SocketAddr,
    path: &str,
    auth_key: &[u8; 32],
) -> io::Result<serde_json::Value> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(500))?;
    stream.set_read_timeout(Some(Duration::from_millis(1500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
        hex_encode(auth_key)
    )?;
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes)?;
    let response = String::from_utf8(bytes).map_err(io::Error::other)?;
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| io::Error::other("malformed node read API response"))?;
    if !headers.starts_with("HTTP/1.1 200") {
        return Err(io::Error::other(format!(
            "node read API rejected request: {}",
            headers.lines().next().unwrap_or("unknown")
        )));
    }
    serde_json::from_str(body).map_err(io::Error::other)
}
pub(super) fn read_line(stream: &TcpStream) -> io::Result<String> {
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    Ok(line)
}
pub(super) fn spawn_output_reader<R: Read + Send + 'static>(
    node_id: NodeId,
    reader: R,
    shared: Shared,
    log_path: PathBuf,
) {
    thread::spawn(move || {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .ok();
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(line) => {
                    if let Some(file) = file.as_mut() {
                        let _ = writeln!(file, "{line}");
                    }
                    if let Some(address) = line
                        .strip_prefix("READ_API ")
                        .and_then(|value| value.parse().ok())
                    {
                        if let Ok(mut state) = shared.lock() {
                            if let Some(node) = state.nodes.get_mut(&node_id) {
                                if node.remote.is_none() {
                                    node.read_api_address = Some(address);
                                }
                            }
                        }
                    }
                    log(&shared, format!("[node {node_id}] {line}"));
                }
                Err(_) => break,
            }
        }
    });
}
pub(super) fn log(shared: &Shared, message: impl Into<String>) {
    shared.lock().unwrap().log(message);
}
