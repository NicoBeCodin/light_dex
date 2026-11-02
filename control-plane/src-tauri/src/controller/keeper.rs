//! A signed, external-to-validator liquidation participant. One candidate
//! stream per keeper; the validator always rechecks risk at execution.
use super::*;
use light_dex::exchange::perp::PerpPositionSnapshot;
use tungstenite::{client::IntoClientRequest, stream::MaybeTlsStream};

pub(super) fn spawn_keeper(shared: Shared, id: u64, run_id: u64) {
    thread::spawn(move || {
        loop {
            let connection = {
                let mut state = shared.lock().unwrap();
                if !state.running || state.run_id != run_id {
                    return;
                }
                let Some(agent) = state.agents.get(&id) else {
                    return;
                };
                if !agent.active {
                    state.agents.remove(&id);
                    state.log(format!("Liquidation keeper {id} stopped"));
                    return;
                }
                let config = agent.config.clone();
                let all_markets = matches!(
                    config.strategy,
                    AgentStrategy::LiquidationKeeper { all_markets: true }
                );
                let account = slot_address(&state, config.account_id);
                let node = resolve_target_id(&state, &config.target)
                    .and_then(|node_id| state.nodes.get(&node_id))
                    .filter(|node| node.state != "dead")
                    .and_then(|node| Some((node.address?, node.read_api_address?)));
                account.zip(node).map(|(account, (consensus, read))| {
                    (
                        config.market_id,
                        all_markets,
                        account,
                        consensus,
                        read,
                        state.auth_key,
                    )
                })
            };
            let Some((market_id, all_markets, account, consensus, read, auth)) = connection else {
                thread::sleep(Duration::from_millis(500));
                continue;
            };
            let url = if all_markets {
                format!("ws://{read}/v1/liquidations/stream")
            } else {
                format!("ws://{read}/v1/liquidations/{market_id}/stream")
            };
            let Ok(mut request) = url.into_client_request() else {
                return;
            };
            let Ok(value) = format!("Bearer {}", hex_encode(&auth)).parse() else {
                return;
            };
            request.headers_mut().insert("Authorization", value);
            let socket = tungstenite::connect(request);
            let Ok((mut socket, _)) = socket else {
                log(
                    &shared,
                    format!("Keeper {id}: candidate stream unavailable; retrying"),
                );
                thread::sleep(Duration::from_secs(1));
                continue;
            };
            if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
            }
            let mut candidates = serde_json::Value::Null;
            loop {
                let still_active = {
                    let state = shared.lock().unwrap();
                    state.running && state.run_id == run_id
                        && state.agents.get(&id).is_some_and(|agent| agent.active
                            && agent.config.market_id == market_id
                            && matches!(agent.config.strategy, AgentStrategy::LiquidationKeeper { all_markets: enabled } if enabled == all_markets)
                            && slot_address(&state, agent.config.account_id) == Some(account)
                            && resolve_target_id(&state, &agent.config.target)
                                .and_then(|node_id| state.nodes.get(&node_id))
                                .is_some_and(|node| node.address == Some(consensus)
                                    && node.read_api_address == Some(read)))
                        && state.nodes.values().any(|node| node.read_api_address == Some(read) && node.state != "dead")
                };
                if !still_active {
                    break;
                }
                match socket.read() {
                    Ok(message) if message.is_text() => {
                        if let Ok(value) = serde_json::from_str(message.to_text().unwrap_or("")) {
                            candidates = value;
                        }
                    }
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(error))
                        if matches!(
                            error.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        ) => {}
                    Err(_) => break,
                }
                keeper_step(
                    &shared,
                    id,
                    account,
                    market_id,
                    all_markets,
                    consensus,
                    read,
                    &auth,
                    &candidates,
                );
            }
        }
    });
}

fn keeper_step(
    shared: &Shared,
    id: u64,
    account: Address,
    market_id: u64,
    all_markets: bool,
    consensus: SocketAddr,
    read: SocketAddr,
    auth: &[u8; 32],
    candidates: &serde_json::Value,
) {
    let Ok(own) = request_read_api(read, &format!("/v1/accounts/{account}"), auth) else {
        return;
    };
    let Some(confirmed_nonce) = own.get("nonce").and_then(serde_json::Value::as_u64) else {
        return;
    };
    let available_usd = own["balances"]
        .as_array()
        .and_then(|balances| {
            balances.iter().find(|balance| {
                balance["asset_id"].as_u64() == Some(light_dex::exchange::market::USD)
            })
        })
        .and_then(|balance| balance["available"].as_u64())
        .unwrap_or(0);
    let has_margin = own["risk"].as_array().is_some_and(|risks| {
        risks.iter().any(|risk| {
            risk["collateral_usd_atoms"]
                .as_i64()
                .is_some_and(|collateral| collateral > 0)
                && (risk["market_id"].is_null() || risk["market_id"].as_u64() == Some(market_id))
        })
    });
    let cross_mode = own["risk"].as_array().is_some_and(|risks| {
        risks
            .iter()
            .any(|risk| risk["mode"].as_str() == Some("Cross"))
    });
    let mut state = shared.lock().unwrap();
    let market_lots = best_status(&state)
        .map(|status| {
            status
                .markets
                .iter()
                .map(|market| (market.id, market.config.quantity.step))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let Some(agent) = state.agents.get_mut(&id) else {
        return;
    };
    if !agent.active {
        return;
    }
    let used = own["used_nonces"]
        .as_array()
        .filter(|values| values.len() == 2)
        .and_then(|values| Some([values[0].as_u64()?, values[1].as_u64()?]));
    agent.pending.retain(|pending| {
        let value = pending.transaction.nonce();
        if value >= confirmed_nonce {
            return true;
        }
        let distance = confirmed_nonce - 1 - value;
        if distance >= 128 {
            return false;
        }
        used.is_none_or(|bits| bits[(distance / 64) as usize] & (1u64 << (distance % 64)) == 0)
    });
    if let Some(outcomes) = own["recent_transaction_outcomes"].as_array() {
        if let Some(failed) = agent.pending.iter().find(|pending| {
            outcomes.iter().any(|outcome| {
                outcome["digest"].as_str() == Some(pending.digest.as_str())
                    && outcome["success"].as_bool() == Some(false)
            })
        }) {
            let failed_nonce = failed.transaction.nonce();
            agent
                .pending
                .retain(|pending| pending.transaction.nonce() != failed_nonce);
            agent.last_error = Some(format!("Keeper transaction at nonce {failed_nonce} failed"));
            agent.last_step = Instant::now();
            return;
        }
    }
    let now = Instant::now();
    if now.duration_since(agent.last_step) < Duration::from_millis(300) {
        return;
    }
    let transaction = if let Some(pending) = agent.pending.front_mut() {
        if now.duration_since(pending.last_sent) < Duration::from_secs(3) {
            return;
        }
        pending.last_sent = now;
        pending.transaction.clone()
    } else if all_markets && !cross_mode {
        Transaction::SetMarginMode {
            account_id: account,
            mode: light_dex::exchange::perp::MarginMode::Cross,
            nonce: confirmed_nonce,
        }
    } else if !has_margin {
        let amount = available_usd.min(100_000 * light_dex::exchange::market::USD_ATOMS);
        if amount == 0 {
            agent.last_error = Some("Keeper needs USD collateral".into());
            return;
        }
        Transaction::DepositPerpCollateral {
            account_id: account,
            market_id,
            amount,
            nonce: confirmed_nonce,
        }
    } else {
        let Some(candidate) = candidates["candidates"].as_array().and_then(|items| {
            items.iter().find(|item| {
                item["risk"]["liquidatable"].as_bool() == Some(true)
                    && item["position"]["account_id"].as_str() != Some(&account.to_string())
            })
        }) else {
            return;
        };
        let Ok(position) =
            serde_json::from_value::<PerpPositionSnapshot>(candidate["position"].clone())
        else {
            return;
        };
        let Ok(size) = u64::try_from(position.size_atoms.unsigned_abs()) else {
            return;
        };
        let lot = market_lots.get(&position.market_id).copied().unwrap_or(1);
        let half = ((size / 2) / lot) * lot;
        let quantity = if half == 0 { size } else { half };
        if quantity == 0 {
            return;
        }
        Transaction::LiquidatePerp {
            liquidator: account,
            target: position.account_id,
            market_id: position.market_id,
            quantity,
            nonce: confirmed_nonce,
        }
    };
    if agent.pending.is_empty() {
        agent.pending.push_back(PendingAgentTransaction {
            digest: hex_encode(&transaction.to_digest()),
            transaction: transaction.clone(),
            action: None,
            last_sent: now,
        });
    }
    agent.last_step = now;
    drop(state);
    let result = send_signed_transaction(shared, consensus, transaction, auth);
    let mut state = shared.lock().unwrap();
    match result {
        Ok(()) => {
            state.submitted += 1;
            state.submit_times.push_back(now);
        }
        Err(error) => {
            if let Some(agent) = state.agents.get_mut(&id) {
                agent.last_error = Some(format!("Keeper submission failed; retrying: {error}"));
            }
        }
    }
}
