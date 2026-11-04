//! Public submission limits and authenticated private validator admission.
use super::*;
pub(super) async fn submit(
    State(app): State<Shared>,
    headers: HeaderMap,
    ConnectInfo(address): ConnectInfo<SocketAddr>,
    Json(signed): Json<SignedTransaction>,
) -> Result<Json<Value>, Failure> {
    if !app.config.writes_enabled {
        return Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Trading is disabled during validation",
        ));
    }
    if headers
        .get("origin")
        .is_some_and(|v| v.to_str().ok() != Some(app.config.public_origin.as_str()))
    {
        return Err(error(StatusCode::FORBIDDEN, "Origin not allowed"));
    }
    let attempt_client = client_key(&headers, address, &app.auth);
    {
        let mut limits = app.limits.lock().unwrap();
        if !limits.take(
            format!("verify:{attempt_client}"),
            100,
            Duration::from_secs(2),
        ) || !limits.take("verify:global".into(), 100, Duration::from_secs(1))
        {
            return Err(error(
                StatusCode::TOO_MANY_REQUESTS,
                "Submission attempt limit reached",
            ));
        }
    }
    verify_transaction(&signed, &app.manifest)
        .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid signed transaction"))?;
    let owner = signed.transaction.account_id().to_string();
    let digest = hex_encode(&signed.transaction.to_digest());
    if app
        .db
        .lock()
        .unwrap()
        .lookup(&digest)
        .is_ok_and(|value| value["status"] == "committed")
    {
        return Ok(Json(
            json!({"digest":digest,"accepted":true,"status":"committed","error":null}),
        ));
    }
    let client = client_key(&headers, address, &app.auth);
    {
        let mut limits = app.limits.lock().unwrap();
        if !limits.take("submit:global".into(), 50, Duration::from_secs(1))
            || !limits.take(format!("submit:{owner}"), 10, Duration::from_secs(2))
            || !limits.take(format!("ip:{client}"), 50, Duration::from_secs(1))
        {
            return Err(error(
                StatusCode::TOO_MANY_REQUESTS,
                "Transaction rate limit reached; retry the same signed transaction",
            ));
        }
    }
    let backlog = app.network.read().await["validators"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|node| node["backlog"].as_u64())
        .max()
        .unwrap_or(0);
    if backlog > app.config.max_public_backlog {
        return Err(error(
            StatusCode::TOO_MANY_REQUESTS,
            "Network backlog limit reached; retry the same signed transaction",
        ));
    }
    if app.free_bytes.load(std::sync::atomic::Ordering::Relaxed) < 2 * 1024 * 1024 * 1024 {
        return Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Storage reserve reached",
        ));
    }
    {
        let mut db = app.db.lock().unwrap();
        if matches!(signed.transaction, Transaction::ClaimAirdrop { .. }) {
            db.claim(&owner, &digest, &client, now())
                .map_err(|message| error(StatusCode::TOO_MANY_REQUESTS, &message))?;
        }
        if !db
            .submission(
                &digest,
                &owner,
                &serde_json::to_string(&signed).unwrap(),
                now(),
            )
            .map_err(|_| {
                error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Submission storage unavailable",
                )
            })?
        {
            return Err(error(
                StatusCode::TOO_MANY_REQUESTS,
                "Eight transactions already pending",
            ));
        }
    }
    let permit = app
        .submissions
        .try_acquire()
        .map_err(|_| error(StatusCode::TOO_MANY_REQUESTS, "Admission capacity reached"))?;
    {
        let mut pending = app.pending.lock().unwrap();
        if pending.len() < 10000 {
            pending.entry(digest.clone()).or_insert_with(Instant::now);
        }
    }
    let nodes = app.config.nodes.clone();
    let auth = app.auth;
    let reply = tokio::task::spawn_blocking(move || admit(&nodes, &signed, &auth))
        .await
        .map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "Admission unavailable"))?
        .map_err(|_| {
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Admission could not be confirmed; retry the same signed transaction",
            )
        })?;
    drop(permit);
    app.db
        .lock()
        .unwrap()
        .0
        .execute(
            "UPDATE submissions SET status=? WHERE digest=? AND status<>'committed'",
            rusqlite::params![
                if reply.accepted {
                    "admitted"
                } else {
                    "rejected"
                },
                digest
            ],
        )
        .map_err(|_| {
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Admission status storage failed",
            )
        })?;
    Ok(Json(
        json!({"digest":digest,"accepted":reply.accepted,"error":reply.error,"status":if reply.accepted{"admitted"}else{"rejected"}}),
    ))
}
pub(super) fn admit(
    nodes: &[NodeConfig],
    signed: &SignedTransaction,
    auth: &[u8; 32],
) -> std::io::Result<AdmissionReply> {
    let line = sign_line(&Message::SubmitTransaction(signed.clone()), auth)?;
    for node in nodes {
        let result = (|| {
            let mut stream =
                std::net::TcpStream::connect_timeout(&node.peer, Duration::from_secs(2))?;
            stream.set_read_timeout(Some(Duration::from_secs(3)))?;
            stream.set_write_timeout(Some(Duration::from_secs(2)))?;
            stream.write_all(line.as_bytes())?;
            let mut response = String::new();
            BufReader::new(stream).take(4096).read_line(&mut response)?;
            let reply: AdmissionReply = verify_line(&response, auth)?;
            if reply.digest != hex_encode(&signed.transaction.to_digest()) {
                return Err(std::io::Error::other("wrong digest"));
            }
            Ok(reply)
        })();
        if result.is_ok() {
            return result;
        }
    }
    Err(std::io::Error::other("no admission endpoint"))
}
