//! Explicit, bounded Tokyo-side load probe; never started by the public router.
use super::*;
use futures_util::StreamExt;
use light_dex::crypto::{new_key, sign_transaction};
use light_dex::exchange::{
    address::Address,
    order::{OrderType, Side},
};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
pub async fn run(
    config: Config,
    manifest: SessionManifest,
    rate: u64,
    seconds: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    if !(1..=200).contains(&rate) || !(5..=60).contains(&seconds) {
        return Err("probe allows 1..200 TPS for 5..60 seconds".into());
    }
    let auth = hex_decode::<32>(std::fs::read_to_string(&config.auth_key_file)?.trim())?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?;
    async fn read(
        client: &reqwest::Client,
        config: &Config,
        auth: &[u8; 32],
        path: &str,
    ) -> Result<Value, Box<dyn std::error::Error>> {
        Ok(client
            .get(format!("{}/v1/{path}", config.nodes[0].read))
            .bearer_auth(hex_encode(auth))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
    let keys = (0..64)
        .map(|_| new_key())
        .collect::<std::io::Result<Vec<_>>>()?;
    for key in &keys {
        let signed = sign_transaction(
            Transaction::ClaimAirdrop {
                account_id: Address::from_public_key(&key.verifying_key()),
                nonce: 0,
            },
            &manifest,
            key,
        )?;
        let nodes = config.nodes.clone();
        tokio::task::spawn_blocking(move || admit(&nodes, &signed, &auth)).await??;
        tokio::time::sleep(Duration::from_millis(70)).await;
    }
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let mut all = true;
        for key in &keys {
            if read(
                &client,
                &config,
                &auth,
                &format!(
                    "accounts/{}",
                    Address::from_public_key(&key.verifying_key())
                ),
            )
            .await
            .is_err()
            {
                all = false;
                break;
            }
        }
        if all {
            break;
        }
        if Instant::now() > deadline {
            return Err("probe funding timed out".into());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let pending = Arc::new(Mutex::new(HashMap::<String, Instant>::new()));
    let measurements = Arc::new(Mutex::new(Vec::<(bool, u64)>::new()));
    let observer_pending = pending.clone();
    let observer_measurements = measurements.clone();
    let mut request = format!(
        "{}/v1/activity/stream",
        config.nodes[0].read.replacen("http://", "ws://", 1)
    )
    .into_client_request()?;
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", hex_encode(&auth)).parse()?,
    );
    let (mut stream, _) = tokio_tungstenite::connect_async(request).await?;
    let observer = tokio::spawn(async move {
        while let Some(Ok(message)) = stream.next().await {
            let Ok(text) = message.into_text() else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if let Some(rows) = value["outcomes"].as_array() {
                for row in rows {
                    if let Some(digest) = row["digest"].as_str() {
                        if let Some(started) = observer_pending.lock().unwrap().remove(digest) {
                            observer_measurements.lock().unwrap().push((
                                row["success"] == true,
                                started.elapsed().as_millis() as u64,
                            ));
                        }
                    }
                }
            }
        }
    });
    let market = read(&client, &config, &auth, "markets/0").await?;
    let mark = market["committed_mark_e8"]
        .as_u64()
        .ok_or("BTC oracle missing")?;
    let low = (mark as u128 * 10 * 95 / 100 / 100000000) as u64;
    let high = (mark as u128 * 10 * 105 / 100 / 100000000) as u64;
    let began_unix = now();
    let began = Instant::now();
    let rejected = Arc::new(Mutex::new(HashMap::<String, u64>::new()));
    let mut issued = 0u64;
    let mut admitted = 0u64;
    let mut jobs = tokio::task::JoinSet::new();
    let mut aborted = false;
    let mut maximum_backlog = 0u64;
    let mut last_guard = Instant::now() - Duration::from_secs(10);
    while began.elapsed() < Duration::from_secs(seconds) {
        if last_guard.elapsed() > Duration::from_secs(2) {
            let memory = std::fs::read_to_string("/proc/meminfo")
                .unwrap_or_default()
                .lines()
                .find(|s| s.starts_with("MemAvailable:"))
                .and_then(|s| s.split_whitespace().nth(1))
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let status = read(&client, &config, &auth, "node/summary").await?;
            maximum_backlog = maximum_backlog.max(status["mempool_size"].as_u64().unwrap_or(0));
            if memory < 250 * 1024
                || disk_free(&config.database)? < 5 * 1024 * 1024 * 1024
                || maximum_backlog > 4000
            {
                aborted = true;
                break;
            }
            last_guard = Instant::now();
        }
        let key = &keys[issued as usize % keys.len()];
        let nonce = 1 + issued / keys.len() as u64;
        let buy = (issued / keys.len() as u64) % 2 == 0;
        let tx = Transaction::PlaceOrder {
            account_id: Address::from_public_key(&key.verifying_key()),
            market_id: 0,
            order_type: OrderType::IOC,
            side: if buy { Side::Buy } else { Side::Sell },
            price: if buy { high } else { low },
            quantity: 1,
            nonce,
        };
        let signed = sign_transaction(tx, &manifest, key)?;
        let digest = hex_encode(&signed.transaction.to_digest());
        pending
            .lock()
            .unwrap()
            .insert(digest.clone(), Instant::now());
        let nodes = config.nodes.clone();
        let admission_pending = pending.clone();
        let admission_rejected = rejected.clone();
        jobs.spawn_blocking(move || {
            let result = admit(&nodes, &signed, &auth);
            let reason = match &result {
                Ok(reply) if !reply.accepted => {
                    Some(reply.error.clone().unwrap_or_else(|| "rejected".into()))
                }
                Err(e) => Some(e.to_string()),
                _ => None,
            };
            if let Some(reason) = reason {
                admission_pending.lock().unwrap().remove(&digest);
                *admission_rejected
                    .lock()
                    .unwrap()
                    .entry(reason)
                    .or_default() += 1;
            }
            result
        });
        issued += 1;
        while let Some(result) = jobs.try_join_next() {
            if result.is_ok_and(|r| r.is_ok_and(|reply| reply.accepted)) {
                admitted += 1;
            }
        }
        while jobs.len() >= 16 {
            if jobs
                .join_next()
                .await
                .is_some_and(|r| r.is_ok_and(|r| r.is_ok_and(|reply| reply.accepted)))
            {
                admitted += 1;
            }
        }
        tokio::time::sleep_until(tokio::time::Instant::from_std(
            began + Duration::from_secs_f64(issued as f64 / rate as f64),
        ))
        .await;
    }
    let sending_seconds = began.elapsed().as_secs_f64();
    while let Some(result) = jobs.join_next().await {
        if result.is_ok_and(|r| r.is_ok_and(|reply| reply.accepted)) {
            admitted += 1;
        }
    }
    let drain = Instant::now();
    while !pending.lock().unwrap().is_empty() && drain.elapsed() < Duration::from_secs(20) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    observer.abort();
    let completed_seconds = began.elapsed().as_secs_f64();
    // This is observed trade history, separate from successful IOC execution:
    // an IOC that finds no liquidity can still execute successfully.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let addresses = keys
        .iter()
        .map(|key| Address::from_public_key(&key.verifying_key()).to_string())
        .collect::<std::collections::HashSet<_>>();
    let fill_summary = (|| -> rusqlite::Result<(u64, usize)> {
        let connection = rusqlite::Connection::open_with_flags(
            &config.database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        connection.busy_timeout(Duration::from_secs(2))?;
        let mut query = connection.prepare("SELECT body FROM trades WHERE created>=?")?;
        let rows = query.query_map([began_unix], |r| r.get::<_, String>(0))?;
        let mut fills = 0;
        let mut orders = std::collections::BTreeSet::new();
        for row in rows {
            if let Ok(value) = serde_json::from_str::<Value>(&row?) {
                if value["taker_account"]
                    .as_str()
                    .is_some_and(|a| addresses.contains(a))
                {
                    fills += 1;
                    orders.insert((
                        value["block_sequence"].as_u64(),
                        value["transaction_index"].as_u64(),
                    ));
                }
            }
        }
        Ok((fills, orders.len()))
    })()
    .ok();
    let elapsed_to_drain = completed_seconds;
    let samples = measurements.lock().unwrap();
    let successful = samples.iter().filter(|(s, _)| *s).count();
    let mut latencies = samples.iter().map(|(_, l)| *l).collect::<Vec<_>>();
    latencies.sort_unstable();
    let percentile = |p: usize| {
        latencies
            .get(latencies.len().saturating_sub(1) * p / 100)
            .copied()
    };
    println!(
        "{}",
        json!({"probe":"BTC spot IOC","submitting_accounts":64,"started_unix":began_unix,"observed_fills":fill_summary.map(|s|s.0),"orders_with_observed_fills":fill_summary.map(|s|s.1),"fill_count_note":"observed trade-history lower bound; successful IOC is not a guarantee of a fill","offered_tps":rate,"duration_seconds":sending_seconds,"issued":issued,"admitted":admitted,"rejected_reasons":*rejected.lock().unwrap(),"observed_committed":samples.len(),"successful":successful,"failed_execution":samples.len()-successful,"successful_issued_tps":successful as f64/sending_seconds,"completion_tps":successful as f64/elapsed_to_drain,"completion_seconds":elapsed_to_drain,"latency_sample_coverage":samples.len() as f64/issued.max(1) as f64,"p50_ms":percentile(50),"p95_ms":percentile(95),"outstanding":pending.lock().unwrap().len(),"maximum_backlog":maximum_backlog,"resource_guard_aborted":aborted,"latency_boundary":"Tokyo driver send to validator 0 durable-commit stream","note":"short bounded probe, not sustainable capacity; failed executions do not count as successful trading TPS"})
    );
    Ok(())
}
