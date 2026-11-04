//! Supervised demo liquidity. Keys and retry intentions stay in private files.
use super::*;
use light_dex::{
    blockchain::QuoteOrder,
    crypto::{new_key, read_secret, sign_transaction, write_secret},
    exchange::{
        address::Address,
        market_config::MarketType,
        order::{OrderType, Side},
    },
};
struct Agent {
    key: ed25519_dalek::SigningKey,
    market: u64,
    role: u8,
    pending: Option<SignedTransaction>,
    path: PathBuf,
}
async fn query(
    client: &reqwest::Client,
    config: &Config,
    auth: &[u8; 32],
    path: &str,
) -> Option<Value> {
    for node in &config.nodes {
        if let Ok(response) = client
            .get(format!("{}/v1/{path}", node.read))
            .bearer_auth(hex_encode(auth))
            .send()
            .await
        {
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return None;
            }
            if response.status().is_success() {
                if let Ok(value) = response.json().await {
                    return Some(value);
                }
            }
        }
    }
    None
}
fn save_intent(path: &std::path::Path, signed: &SignedTransaction) -> std::io::Result<()> {
    let tmp = path.with_extension("pending-tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(&serde_json::to_vec(signed)?)?;
    file.sync_all()?;
    std::fs::rename(tmp, path)?;
    std::fs::File::open(path.parent().unwrap())?.sync_all()
}
pub async fn run(
    config: Config,
    manifest: SessionManifest,
) -> Result<(), Box<dyn std::error::Error>> {
    let history = store::Store::open(&config.database)?;
    let auth = hex_decode::<32>(std::fs::read_to_string(&config.auth_key_file)?.trim())?;
    let directory = config.database.parent().unwrap().join("bot-keys");
    std::fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;
    let exchange = light_dex::exchange::ExchangeState::with_claim_faucet(manifest.faucet)?;
    let mut agents = Vec::new();
    for (&market, info) in exchange.markets() {
        for role in 0..if matches!(info.market_type, MarketType::Perp { .. }) {
            4
        } else {
            3
        } {
            let path = directory.join(format!("market-{market}-role-{role}.key"));
            if !path.exists() {
                write_secret(&path, &new_key()?)?;
            }
            let key = read_secret(&path)?;
            let intent = path.with_extension("intent");
            let pending = std::fs::read(&intent)
                .ok()
                .map(|bytes| serde_json::from_slice(&bytes))
                .transpose()?;
            agents.push(Agent {
                key,
                market,
                role,
                pending,
                path: intent,
            });
        }
    }
    println!(
        "Supervised liquidity fleet: {} agents, maximum 20 attempts/s",
        agents.len()
    );
    let mut marks = HashMap::<u64, (String, Instant, bool)>::new();
    let mut cursor = 0usize;
    let mut last_disk_check = Instant::now() - Duration::from_secs(10);
    let mut disk_ok = true;
    loop {
        let started = Instant::now();
        if last_disk_check.elapsed() >= Duration::from_secs(10) {
            disk_ok = disk_free(&config.database).unwrap_or(0) >= 5 * 1024 * 1024 * 1024;
            last_disk_check = Instant::now();
        }
        if !disk_ok {
            tokio::time::sleep(Duration::from_secs(10)).await;
            continue;
        }
        let index = cursor;
        cursor = (cursor + 1) % agents.len();
        let agent = &mut agents[index];
        let owner = Address::from_public_key(&agent.key.verifying_key());
        let owner_text = owner.to_string();
        let account = query(&client, &config, &auth, &format!("accounts/{owner_text}")).await;
        if let Some(pending) = &agent.pending {
            let digest = hex_encode(&pending.transaction.to_digest());
            let nonce = pending.transaction.nonce();
            let outcome = account
                .as_ref()
                .and_then(|a| a["recent_transaction_outcomes"].as_array())
                .and_then(|rows| rows.iter().find(|row| row["digest"] == digest));
            let consumed = account
                .as_ref()
                .and_then(|a| a["nonce"].as_u64())
                .is_some_and(|n| n > nonce);
            let archived = history
                .lookup(&digest)
                .is_ok_and(|value| value["status"] == "committed");
            if outcome.is_some() || consumed || archived {
                agent.pending = None;
                let _ = std::fs::remove_file(&agent.path);
            } else {
                let nodes = config.nodes.clone();
                let signed = pending.clone();
                let _ = tokio::task::spawn_blocking(move || admit(&nodes, &signed, &auth)).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        }
        let nonce = account
            .as_ref()
            .and_then(|a| a["nonce"].as_u64())
            .unwrap_or(0);
        let info = &exchange.markets()[&agent.market];
        let perp = matches!(info.market_type, MarketType::Perp { .. });
        let mut intention = if account.is_none() {
            Some(Transaction::ClaimAirdrop {
                account_id: owner,
                nonce: 0,
            })
        } else {
            None
        };
        let account = account.unwrap_or(Value::Null);
        if intention.is_none() && perp {
            let cross = account["risk"]
                .as_array()
                .and_then(|r| r.iter().find(|r| r["mode"] == "Cross"));
            if cross.is_none() {
                intention = Some(Transaction::SetMarginMode {
                    account_id: owner,
                    mode: light_dex::exchange::perp::MarginMode::Cross,
                    nonce,
                });
            } else if cross.unwrap()["collateral_usd_atoms"].as_i64().unwrap_or(0) < 1000 * 1000000
            {
                let available = account["balances"]
                    .as_array()
                    .and_then(|b| b.iter().find(|b| b["asset_id"] == 0))
                    .and_then(|b| b["available"].as_u64())
                    .unwrap_or(0);
                if available >= 10000 * 1000000 {
                    intention = Some(Transaction::DepositPerpCollateral {
                        account_id: owner,
                        market_id: agent.market,
                        amount: 10000 * 1000000,
                        nonce,
                    });
                }
            }
        }
        if intention.is_none() {
            if let Some(market) = query(
                &client,
                &config,
                &auth,
                &format!("markets/{}", agent.market),
            )
            .await
            {
                let digest = market["oracle_update_digest"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                let entry =
                    marks
                        .entry(agent.market)
                        .or_insert((digest.clone(), Instant::now(), false));
                if entry.0 != digest {
                    entry.0 = digest;
                    entry.1 = Instant::now();
                    entry.2 = true;
                }
                let fresh = entry.2 && entry.1.elapsed() < Duration::from_secs(30);
                let orders = account["open_orders"]
                    .as_array()
                    .map(|o| {
                        o.iter()
                            .filter(|o| o["market_id"] == agent.market)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let cancel = orders
                    .iter()
                    .filter_map(|o| o["order_id"].as_u64())
                    .take(16)
                    .collect::<Vec<_>>();
                if agent.role < 2 {
                    let mark = market["committed_mark_e8"].as_u64().unwrap_or(0);
                    let midpoint =
                        (u128::from(mark) * u128::from(info.price.scale()) / 100000000) as u64;
                    let mut place = Vec::new();
                    if fresh && midpoint > info.price.step {
                        let base_available = account["balances"]
                            .as_array()
                            .and_then(|b| b.iter().find(|b| b["asset_id"] == info.base_asset))
                            .and_then(|b| b["available"].as_u64())
                            .unwrap_or(0);
                        let locked_base = orders
                            .iter()
                            .filter(|o| o["side"] == "Sell")
                            .filter_map(|o| o["remaining_quantity"].as_u64())
                            .sum::<u64>();
                        for (side, multiplier) in [(Side::Buy, -1i64), (Side::Sell, 1)] {
                            let mut last = 0;
                            for bps in [
                                2 + u64::from(agent.role),
                                5 + u64::from(agent.role),
                                10 + u64::from(agent.role),
                            ] {
                                let offset = (midpoint * bps / 10000).max(info.price.step);
                                let raw = if multiplier < 0 {
                                    midpoint.saturating_sub(offset)
                                } else {
                                    midpoint.saturating_add(offset)
                                };
                                let price = if side == Side::Buy {
                                    raw / info.price.step * info.price.step
                                } else {
                                    raw.div_ceil(info.price.step) * info.price.step
                                };
                                if price == last || price == 0 {
                                    continue;
                                }
                                last = price;
                                let target = (100u128
                                    * u128::from(info.quantity.scale())
                                    * u128::from(info.price.scale())
                                    / u128::from(price))
                                    as u64;
                                let capped = if !perp && side == Side::Sell {
                                    target.min(base_available.saturating_add(locked_base) / 4)
                                } else {
                                    target
                                };
                                let quantity = capped / info.quantity.step * info.quantity.step;
                                if quantity > 0 {
                                    place.push(QuoteOrder {
                                        order_type: OrderType::PostOnly,
                                        side,
                                        price,
                                        quantity,
                                        reduce_only: false,
                                    });
                                }
                            }
                        }
                    }
                    if !place.is_empty() || !cancel.is_empty() {
                        intention = Some(Transaction::UpdateQuotes {
                            account_id: owner,
                            market_id: agent.market,
                            cancel,
                            reduce: vec![],
                            place,
                            nonce,
                        });
                    }
                } else if agent.role == 2 && fresh {
                    let mut side = if rand::random::<bool>() {
                        Side::Buy
                    } else {
                        Side::Sell
                    };
                    let position = account["positions"]
                        .as_array()
                        .and_then(|p| p.iter().find(|p| p["market_id"] == agent.market))
                        .and_then(|p| p["size_atoms"].as_i64())
                        .unwrap_or(0);
                    if perp && position != 0 {
                        side = if position > 0 { Side::Sell } else { Side::Buy };
                    }
                    let level = if side == Side::Buy {
                        market["book"]["asks"].as_array().and_then(|v| v.first())
                    } else {
                        market["book"]["bids"].as_array().and_then(|v| v.first())
                    };
                    if let Some(level) = level {
                        let price = level["price"].as_u64().unwrap_or(0);
                        if price > 0 {
                            let mut quantity = ((25u128
                                * u128::from(info.quantity.scale())
                                * u128::from(info.price.scale())
                                / u128::from(price))
                                as u64
                                / info.quantity.step
                                * info.quantity.step)
                                .max(info.quantity.step);
                            if perp && position != 0 {
                                quantity = quantity.min(position.unsigned_abs());
                            }
                            if !perp && side == Side::Sell {
                                let available = account["balances"]
                                    .as_array()
                                    .and_then(|rows| {
                                        rows.iter().find(|b| b["asset_id"] == info.base_asset)
                                    })
                                    .and_then(|b| b["available"].as_u64())
                                    .unwrap_or(0);
                                quantity = quantity.min(available) / info.quantity.step
                                    * info.quantity.step;
                            }
                            if quantity < info.quantity.step {
                                tokio::time::sleep(Duration::from_millis(50)).await;
                                continue;
                            }
                            intention = Some(if perp {
                                Transaction::PlacePerpOrder {
                                    account_id: owner,
                                    market_id: agent.market,
                                    order_type: OrderType::IOC,
                                    side,
                                    price,
                                    quantity,
                                    reduce_only: position != 0,
                                    nonce,
                                }
                            } else {
                                Transaction::PlaceOrder {
                                    account_id: owner,
                                    market_id: agent.market,
                                    order_type: OrderType::IOC,
                                    side,
                                    price,
                                    quantity,
                                    nonce,
                                }
                            });
                        }
                    }
                } else if agent.role == 3 && fresh {
                    if let Some(candidates) = query(
                        &client,
                        &config,
                        &auth,
                        &format!("liquidations/{}", agent.market),
                    )
                    .await
                    {
                        if let Some(candidate) =
                            candidates["candidates"].as_array().and_then(|rows| {
                                rows.iter().find(|c| c["risk"]["liquidatable"] == true)
                            })
                        {
                            let target = candidate["position"]["account_id"]
                                .as_str()
                                .and_then(|s| s.parse::<Address>().ok());
                            let size = candidate["position"]["size_atoms"]
                                .as_i64()
                                .unwrap_or(0)
                                .unsigned_abs();
                            let quantity = (size / 2 / info.quantity.step * info.quantity.step)
                                .max(info.quantity.step);
                            if let Some(target) = target {
                                if target != owner {
                                    intention = Some(Transaction::LiquidatePerp {
                                        liquidator: owner,
                                        target,
                                        market_id: agent.market,
                                        quantity,
                                        nonce,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(transaction) = intention {
            let signed = sign_transaction(transaction, &manifest, &agent.key)?;
            save_intent(&agent.path, &signed)?;
            agent.pending = Some(signed.clone());
            let nodes = config.nodes.clone();
            let _ = tokio::task::spawn_blocking(move || admit(&nodes, &signed, &auth)).await;
        }
        tokio::time::sleep(Duration::from_millis(50).saturating_sub(started.elapsed())).await;
    }
}
