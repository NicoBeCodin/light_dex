//! Localhost-only browser transport for the existing Rust controller. Keys
//! and the validator authentication secret never enter the browser process.
use crate::{controller::ControllerState, types::*};
use axum::{
    Json, Router,
    extract::{
        Path, Query, State,
        ws::{Message as BrowserMessage, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use light_dex::{blockchain::Transaction, exchange::address::Address, message::hex_encode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{net::SocketAddr, path::PathBuf};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tower_http::services::ServeDir;

#[derive(Clone)]
struct WebState {
    controller: ControllerState,
    token: String,
}

#[derive(Deserialize)]
struct InvokeRequest {
    command: String,
    #[serde(default)]
    args: Value,
}
#[derive(Deserialize)]
struct StreamToken {
    token: String,
}

// The browser has scoped node streams for books, trades, and accounts. Keep
// one full account projection for wallet/risk panels; summary fields remain
// available for every validator card and for explicit Query now responses.
fn browser_snapshot(mut snapshot: ControlSnapshot) -> ControlSnapshot {
    let account_source = snapshot
        .nodes
        .iter()
        .filter(|node| node.state == "ok")
        .filter_map(|node| {
            node.status
                .as_ref()
                .filter(|status| !status.account_nonces.is_empty())
                .map(|status| (node.node_id, status.chain_height))
        })
        .max_by_key(|(_, height)| *height)
        .map(|(id, _)| id);
    for node in &mut snapshot.nodes {
        let Some(status) = node.status.as_mut() else {
            continue;
        };
        status.recent_transaction_outcomes.clear();
        status.recent_trades.clear();
        status.open_orders.clear();
        status.order_books.clear();
        status.recent_commit_latency_ms.clear();
        if Some(node.node_id) != account_source {
            status.account_nonces.clear();
            status.agent_balances.clear();
            status.perp_risk.clear();
            status.perp_positions.clear();
        }
    }
    snapshot
}

fn local_host(headers: &HeaderMap) -> bool {
    headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|host| host.starts_with("127.0.0.1:") || host.starts_with("localhost:"))
}
fn same_origin(headers: &HeaderMap) -> bool {
    headers
        .get("origin")
        .and_then(|value| value.to_str().ok())
        .is_none_or(|origin| {
            origin.starts_with("http://127.0.0.1:") || origin.starts_with("http://localhost:")
        })
}
fn authorized(headers: &HeaderMap, state: &WebState) -> bool {
    local_host(headers)
        && same_origin(headers)
        && headers
            .get("x-light-dex-token")
            .and_then(|value| value.to_str().ok())
            == Some(&state.token)
}
fn arg<T: DeserializeOwned>(args: &Value, name: &str) -> Result<T, String> {
    serde_json::from_value(
        args.get(name)
            .cloned()
            .ok_or_else(|| format!("missing {name}"))?,
    )
    .map_err(|error| format!("invalid {name}: {error}"))
}
fn value<T: Serialize>(result: T) -> Result<Value, String> {
    serde_json::to_value(result).map_err(|error| error.to_string())
}

async fn session(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    if !local_host(&headers) || !same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(Json(json!({ "token":state.token })))
}

async fn invoke(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(request): Json<InvokeRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !authorized(&headers, &state) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error":"browser session denied"})),
        ));
    }
    let controller = state.controller;
    let args = request.args;
    let result = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        match request.command.as_str() {
            "default_config" => value(super::default_config()),
            "remote_profiles" => value(controller.remote_profiles()?),
            "launch" => {
                controller.launch(arg::<LaunchConfig>(&args, "config")?)?;
                value(())
            }
            "stop_all" => {
                controller.stop_all();
                value(())
            }
            "submit_transaction" => {
                controller.submit_transaction(
                    arg::<Transaction>(&args, "transaction")?,
                    arg::<TargetNode>(&args, "target")?,
                )?;
                value(())
            }
            "create_address" => value(controller.create_address()?),
            "claim_new_accounts" => {
                value(controller.claim_new_accounts(arg::<u64>(&args, "count")?)?)
            }
            "airdrop_new_address" => value(controller.airdrop_new_address(
                arg::<u64>(&args, "fromLabel")?,
                arg::<u64>(&args, "asset")?,
                arg::<u64>(&args, "amount")?,
                arg::<TargetNode>(&args, "target")?,
            )?),
            "fund_address" => {
                controller.fund_address(
                    arg::<u64>(&args, "fromLabel")?,
                    arg::<Address>(&args, "to")?,
                    arg::<u64>(&args, "asset")?,
                    arg::<u64>(&args, "amount")?,
                    arg::<TargetNode>(&args, "target")?,
                )?;
                value(())
            }
            "pause_node" => {
                controller.pause_node(
                    arg::<TargetNode>(&args, "target")?,
                    arg::<u64>(&args, "millis")?,
                )?;
                value(())
            }
            "kill_node" => {
                controller.kill_node(arg::<TargetNode>(&args, "target")?)?;
                value(())
            }
            "start_load" => {
                controller.start_load(arg::<LoadConfig>(&args, "config")?)?;
                value(())
            }
            "stop_load" => {
                controller.stop_load();
                value(())
            }
            "start_market_maker" => {
                controller.start_market_maker(arg::<MarketMakerConfig>(&args, "config")?)?;
                value(())
            }
            "stop_market_maker" => {
                controller.stop_market_maker();
                value(())
            }
            "start_agent" => {
                controller.start_agent(arg::<AgentConfig>(&args, "config")?)?;
                value(())
            }
            "start_agents" => {
                value(controller.start_agents(arg::<Vec<AgentConfig>>(&args, "configs")?)?)
            }
            "stop_agent" => {
                controller.stop_agent(arg::<u64>(&args, "agentId")?)?;
                value(())
            }
            "stop_agents" => {
                controller.stop_agents(arg::<Vec<u64>>(&args, "agentIds")?)?;
                value(())
            }
            "default_participant_fleet" => value(super::participant_fleet_template()),
            "hundred_participant_fleet" => value(super::hundred_participant_fleet_template()),
            "perp_participant_fleet" => value(super::perp_participant_fleet_template()),
            "get_snapshot" => value(browser_snapshot(controller.snapshot())),
            "refresh_node_status" => {
                value(controller.refresh_node_status(arg::<u64>(&args, "nodeId")?)?)
            }
            _ => Err(format!("unknown command: {}", request.command)),
        }
    })
    .await
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":error.to_string()})),
        )
    })?;
    result
        .map(Json)
        .map_err(|error| (StatusCode::BAD_REQUEST, Json(json!({"error":error}))))
}

fn node_path(scope: &str, key: &str, stream: bool) -> Option<String> {
    let base = match scope {
        "markets" if key.parse::<u64>().is_ok() => format!("/v1/markets/{key}"),
        "accounts" if key.parse::<Address>().is_ok() => format!("/v1/accounts/{key}"),
        "liquidations" if key.parse::<u64>().is_ok() => format!("/v1/liquidations/{key}"),
        "explorer" if !stream && key == "recent" => "/v1/explorer/recent".to_string(),
        "blocks" if !stream && key.parse::<u64>().is_ok() => format!("/v1/explorer/blocks/{key}"),
        "transactions"
            if !stream && key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
        {
            format!("/v1/explorer/transactions/{key}")
        }
        "hashes"
            if !stream && key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
        {
            format!("/v1/explorer/hashes/{key}")
        }
        _ => return None,
    };
    Some(if stream {
        format!("{base}/stream")
    } else {
        base
    })
}

async fn node_query(
    State(state): State<WebState>,
    Path((id, scope, key)): Path<(u64, String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&headers, &state) {
        return Err(StatusCode::FORBIDDEN);
    }
    let path = node_path(&scope, &key, false).ok_or(StatusCode::BAD_REQUEST)?;
    let value =
        tokio::task::spawn_blocking(move || state.controller.read_node_projection(id, &path))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
    Ok(Json(value))
}

async fn node_stream(
    State(state): State<WebState>,
    Path((id, scope, key)): Path<(u64, String, String)>,
    Query(token): Query<StreamToken>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    if !local_host(&headers) || !same_origin(&headers) || token.token != state.token {
        return Err(StatusCode::FORBIDDEN);
    }
    let path = node_path(&scope, &key, true).ok_or(StatusCode::BAD_REQUEST)?;
    let (address, auth) = state
        .controller
        .read_api_target(id)
        .ok_or(StatusCode::BAD_GATEWAY)?;
    Ok(ws.on_upgrade(move |socket| proxy_stream(socket, address, auth, path)))
}

async fn proxy_stream(socket: WebSocket, address: SocketAddr, auth: [u8; 32], path: String) {
    let url = format!("ws://{address}{path}");
    let Ok(mut request) = url.into_client_request() else {
        return;
    };
    let Ok(header) = format!("Bearer {}", hex_encode(&auth)).parse() else {
        return;
    };
    request.headers_mut().insert("Authorization", header);
    let Ok((upstream, _)) = tokio_tungstenite::connect_async(request).await else {
        return;
    };
    let (mut downstream_out, mut downstream_in) = socket.split();
    let (mut upstream_out, mut upstream_in) = upstream.split();
    loop {
        tokio::select! {
            message = upstream_in.next() => match message {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                    if downstream_out.send(BrowserMessage::Text(text.to_string().into())).await.is_err() { break; }
                }
                Some(Ok(_)) => {},
                _ => break,
            },
            message = downstream_in.next() => match message {
                Some(Ok(BrowserMessage::Close(_))) | None | Some(Err(_)) => break,
                _ => {},
            }
        }
    }
    let _ = upstream_out.close().await;
}

pub async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let port = std::env::var("LIGHT_DEX_WEB_PORT")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(1421);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let token = hex_encode(&rand::random::<[u8; 32]>());
    let state = WebState {
        controller: ControllerState::default(),
        token,
    };
    let dist = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../dist");
    let app = Router::new()
        .route("/api/session", get(session))
        .route("/api/invoke", post(invoke))
        .route("/api/nodes/{id}/{scope}/{key}", get(node_query))
        .route("/api/nodes/{id}/{scope}/{key}/stream", get(node_stream))
        .fallback_service(ServeDir::new(dist).append_index_html_on_directories(true))
        .with_state(state);
    println!("Light DEX browser control plane: http://127.0.0.1:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}
