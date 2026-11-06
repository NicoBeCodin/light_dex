//! Private node subscriptions, durable history indexing and observed metrics.
use super::*;
pub(super) async fn observe(app: Shared) {
    let mut last_prune = Instant::now();
    let mut previous_height = 0;
    let mut last_progress = Instant::now();
    loop {
        let tick = Instant::now();
        app.free_bytes.store(
            disk_free(&app.config.database).unwrap_or(0),
            std::sync::atomic::Ordering::Relaxed,
        );
        let mut nodes = Vec::new();
        let mut maximum = 0u64;
        for node in &app.config.nodes {
            let response = app
                .client
                .get(format!("{}/v1/node/summary", node.read))
                .bearer_auth(hex_encode(&app.auth))
                .send()
                .await;
            let value = match response {
                Ok(r) if r.status().is_success() => r.json::<Value>().await.ok(),
                _ => None,
            };
            if let Some(v) = value {
                maximum = maximum.max(v["height"].as_u64().unwrap_or(0));
                nodes.push(json!({"id":node.id,"region":node.region,"online":true,"height":v["height"],"view":v["view"],"primary":v["primary"],"backlog":v["mempool_size"]}));
            } else {
                nodes.push(json!({"id":node.id,"region":node.region,"online":false}));
            }
        }
        if maximum > previous_height {
            previous_height = maximum;
            last_progress = Instant::now();
        }
        if maximum > 0 {
            let cursor = app
                .db
                .lock()
                .unwrap()
                .0
                .query_row("SELECT coalesce(max(sequence),-1) FROM blocks", [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap_or(-1);
            let begin = (cursor + 1).max(maximum.saturating_sub(1000) as i64);
            for sequence in begin..(maximum as i64).min(begin + 64) {
                if let Ok(block) = read_node(&app, &format!("explorer/blocks/{sequence}")).await {
                    let timestamp = now();
                    let transactions = block["transactions"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    let db = app.db.lock().unwrap();
                    let indexed = (|| -> rusqlite::Result<()> {
                        let transaction = db.0.unchecked_transaction()?;
                        transaction.execute(
                            "INSERT OR REPLACE INTO blocks VALUES(?,?,?)",
                            rusqlite::params![sequence, block.to_string(), timestamp],
                        )?;
                        for receipt in &transactions {
                            if let Some(digest) = receipt["digest"].as_str() {
                                db.receipt(digest, receipt, timestamp, None)?;
                            }
                        }
                        transaction.commit()
                    })();
                    if indexed.is_err() {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        {
            let mut latencies = app.latencies.lock().unwrap();
            while latencies
                .front()
                .is_some_and(|(at, _)| at.elapsed() > Duration::from_secs(300))
            {
                latencies.pop_front();
            }
        }
        let (p50, p95, samples) = {
            let l = app.latencies.lock().unwrap();
            let mut v = l.iter().map(|(_, ms)| *ms).collect::<Vec<_>>();
            v.sort_unstable();
            let get = |p: usize| {
                v.get((v.len() * p).div_ceil(100).saturating_sub(1))
                    .copied()
            };
            (get(50), get(95), v.len())
        };
        let timestamp = now();
        let (orders, total, failed) = {
            let db = app.db.lock().unwrap();
            let counts = |filter: &str| {
                db.0.query_row(
                    &format!("SELECT count(*) FROM receipts WHERE created>? {filter}"),
                    [timestamp - 60],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0)
            };
            (
                counts(
                    "AND json_extract(body,'$.success')=1 AND json_extract(body,'$.kind') IN ('Spot order','Perp order','Update quotes','Cancel order','Reduce order')",
                ),
                counts("AND json_extract(body,'$.success')=1"),
                counts("AND json_extract(body,'$.success')=0"),
            )
        };
        let quorum = light_dex::consensus::quorum_size(app.config.nodes.len() as u64);
        let responsive = nodes.iter().filter(|n| n["online"] == true).count();
        *app.network.write().await = json!({"height":maximum,"validators":nodes,"quorum":quorum,"status":if responsive<quorum as usize{"unavailable"}else if maximum==0{"starting"}else if last_progress.elapsed()>Duration::from_secs(30)&&nodes.iter().any(|n|n["backlog"].as_u64().unwrap_or(0)>0){"stalled"}else if responsive<app.config.nodes.len(){"degraded"}else{"online"},"updated_at":timestamp,"uptime_seconds":app.started.elapsed().as_secs(),"successful_order_tps":orders as f64/60.,"successful_tps":total as f64/60.,"failed_execution_tps":failed as f64/60.,"gateway_commit_p50_ms":p50,"gateway_commit_p95_ms":p95,"latency_samples":samples,"latency_window_seconds":300,"latency_boundary":"Tokyo gateway or demo-agent submission to observed durable commit","entry_region":"Tokyo"});
        if last_prune.elapsed() > Duration::from_secs(600) {
            let _ = app.db.lock().unwrap().prune(now());
            app.pending
                .lock()
                .unwrap()
                .retain(|_, at| at.elapsed() < Duration::from_secs(300));
            last_prune = Instant::now();
        }
        tokio::time::sleep(Duration::from_secs(1).saturating_sub(tick.elapsed())).await;
    }
}
pub(super) async fn observe_trades(app: Shared) {
    loop {
        for market in 0..10 {
            if let Ok(value) = cached(&app, &format!("markets/{market}")).await {
                if let Some(trades) = value["recent_trades"].as_array() {
                    let db = app.db.lock().unwrap();
                    let Ok(batch) = db.0.unchecked_transaction() else {
                        continue;
                    };
                    for trade in trades {
                        let _ = db.0.execute(
                            "INSERT OR IGNORE INTO trades VALUES(?,?,?,?,?,?)",
                            rusqlite::params![
                                market,
                                trade["block_sequence"].as_u64().unwrap_or(0),
                                trade["transaction_index"].as_u64().unwrap_or(0),
                                trade["fill_index"].as_u64().unwrap_or(0),
                                {
                                    let mut t = trade.clone();
                                    t["observed_at"] = json!(now());
                                    t.to_string()
                                },
                                now()
                            ],
                        );
                    }
                    let _ = batch.commit();
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
pub(super) async fn observe_commits(app: Shared) {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    loop {
        for node in &app.config.nodes {
            let url = format!(
                "{}/v1/activity/stream",
                node.read.replacen("http://", "ws://", 1)
            );
            let Ok(mut request) = url.into_client_request() else {
                continue;
            };
            request.headers_mut().insert(
                "authorization",
                format!("Bearer {}", hex_encode(&app.auth)).parse().unwrap(),
            );
            let Ok(Ok((mut stream, _))) = tokio::time::timeout(
                Duration::from_secs(3),
                tokio_tungstenite::connect_async(request),
            )
            .await
            else {
                continue;
            };
            while let Ok(Some(Ok(message))) =
                tokio::time::timeout(Duration::from_secs(5), stream.next()).await
            {
                let Ok(text) = message.into_text() else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if let Some(outcomes) = value["outcomes"].as_array() {
                    let mut rows = Vec::with_capacity(outcomes.len());
                    for receipt in outcomes {
                        let Some(digest) = receipt["digest"].as_str() else {
                            continue;
                        };
                        let latency = app
                            .pending
                            .lock()
                            .unwrap()
                            .remove(digest)
                            .map(|at| at.elapsed().as_millis() as u64);
                        if let Some(ms) = latency {
                            app.latencies
                                .lock()
                                .unwrap()
                                .push_back((Instant::now(), ms));
                        }
                        rows.push((digest, receipt, latency.map(|ms| ms as i64)));
                    }
                    let _ = app.db.lock().unwrap().receipt_batch(&rows, now());
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Admission is not commitment. Retry persisted public intentions even when
/// the visitor closes the page or all validators restart. Do not invent a new
/// latency clock for requests received before this gateway process started.
pub(super) async fn retry_submissions(app: Shared) {
    let mut cursor = 0i64;
    loop {
        let rows = {
            let db = app.db.lock().unwrap();
            (||->rusqlite::Result<Vec<(i64,String)>>{
                let mut query=db.0.prepare("SELECT rowid,raw FROM submissions WHERE rowid>? AND status IN ('submitting','admitted') AND created<? ORDER BY rowid LIMIT 16")?;
                query.query_map(rusqlite::params![cursor,now()-5],|r|Ok((r.get(0)?,r.get(1)?)))?.collect()
            })().unwrap_or_default()
        };
        if rows.is_empty() {
            cursor = 0;
        }
        for (row, raw) in rows {
            cursor = row;
            let Ok(signed) = serde_json::from_str::<SignedTransaction>(&raw) else {
                continue;
            };
            let nodes = app.config.nodes.clone();
            let auth = app.auth;
            let _ = tokio::task::spawn_blocking(move || admit(&nodes, &signed, &auth)).await;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
