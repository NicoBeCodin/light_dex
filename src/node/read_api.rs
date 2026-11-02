//! Read-only, market/address-scoped node RPC. The HTTP worker never borrows
//! consensus state: requests cross the node event channel and are answered by
//! the node's owner thread. WebSocket messages are emitted only after commits.
use super::*;
use axum::{
    Json, Router,
    extract::{
        Path, State,
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::get,
};
use serde_json::{Value, json};
use std::{collections::HashMap, net::TcpListener, str::FromStr};
use tokio::sync::broadcast;
mod published;
mod topics;
use super::explorer::ExplorerQuery;
pub use published::{ReadSnapshot, ReadUpdate};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadQuery {
    Summary,
    Market(u64),
    Account(Address),
    Candidates(u64),
    CandidatesAll,
}

#[derive(Clone)]
struct ApiState {
    events: crate::priority::Sender<NodeEvent>,
    updates: broadcast::Sender<Arc<ReadUpdate>>,
    auth: String,
    topics: topics::Topics,
}

fn authorized(headers: &HeaderMap, state: &ApiState) -> bool {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(state.auth.as_str())
}

async fn query(state: ApiState, request: ReadQuery) -> Result<Json<Value>, StatusCode> {
    let published = topics::get(state, request).await?;
    if published.value.get("error").is_some() {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(published.value.clone()))
}

async fn project(
    state: ApiState,
    request: ReadQuery,
) -> Result<Arc<topics::Published>, StatusCode> {
    tokio::task::spawn_blocking(move || {
        let (reply, response) = mpsc::channel();
        state
            .events
            .try_send(NodeEvent::ReadQuery {
                query: request,
                reply,
            })
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let snapshot = response
            .recv_timeout(Duration::from_millis(1500))
            .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?;
        let value = snapshot.render(request);
        Ok(Arc::new(topics::Published::new(value)))
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
}

async fn summary(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    query(state, ReadQuery::Summary).await
}
async fn market(
    State(state): State<ApiState>,
    Path(id): Path<u64>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    query(state, ReadQuery::Market(id)).await
}
async fn account(
    State(state): State<ApiState>,
    Path(address): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let address = Address::from_str(&address).map_err(|_| StatusCode::BAD_REQUEST)?;
    query(state, ReadQuery::Account(address)).await
}
async fn candidates(
    State(state): State<ApiState>,
    Path(id): Path<u64>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    query(state, ReadQuery::Candidates(id)).await
}
async fn all_candidates(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    query(state, ReadQuery::CandidatesAll).await
}

async fn explorer_query(
    state: ApiState,
    request: ExplorerQuery,
) -> Result<Json<Value>, StatusCode> {
    let value = tokio::task::spawn_blocking(move || {
        let (reply, response) = mpsc::channel();
        state
            .events
            .try_send(NodeEvent::ExplorerQuery {
                query: request,
                reply,
            })
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        response
            .recv_timeout(Duration::from_millis(1500))
            .map_err(|_| StatusCode::GATEWAY_TIMEOUT)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
    if value.get("error").is_some() {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(value))
}

async fn recent_blocks(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    explorer_query(state, ExplorerQuery::Recent).await
}
async fn block_detail(
    State(state): State<ApiState>,
    Path(sequence): Path<u64>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    explorer_query(state, ExplorerQuery::Block(sequence)).await
}
async fn transaction_detail(
    State(state): State<ApiState>,
    Path(digest): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let digest = crate::message::hex_decode::<32>(&digest).map_err(|_| StatusCode::BAD_REQUEST)?;
    explorer_query(state, ExplorerQuery::Transaction(digest)).await
}
async fn hash_lookup(
    State(state): State<ApiState>,
    Path(digest): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let digest = crate::message::hex_decode::<32>(&digest).map_err(|_| StatusCode::BAD_REQUEST)?;
    explorer_query(state, ExplorerQuery::Hash(digest)).await
}

async fn stream_market(
    State(state): State<ApiState>,
    Path(id): Path<u64>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    if !authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(ws.on_upgrade(move |socket| stream(socket, state, ReadQuery::Market(id))))
}
async fn stream_account(
    State(state): State<ApiState>,
    Path(address): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    if !authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let address = Address::from_str(&address).map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok(ws.on_upgrade(move |socket| stream(socket, state, ReadQuery::Account(address))))
}
async fn stream_candidates(
    State(state): State<ApiState>,
    Path(id): Path<u64>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    if !authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(ws.on_upgrade(move |socket| stream(socket, state, ReadQuery::Candidates(id))))
}
async fn stream_all_candidates(
    State(state): State<ApiState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    if !authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(ws.on_upgrade(move |socket| stream(socket, state, ReadQuery::CandidatesAll)))
}

async fn stream(mut socket: WebSocket, state: ApiState, request: ReadQuery) {
    let Ok(mut receiver) = topics::subscribe(state, request).await else {
        return;
    };
    loop {
        let value = receiver.borrow_and_update().clone();
        if let Some(Ok(value)) = value {
            if !matches!(
                tokio::time::timeout(
                    Duration::from_millis(1500),
                    socket.send(WsMessage::Text(value.text.to_string().into()))
                )
                .await,
                Ok(Ok(()))
            ) {
                break;
            }
        } else if value.is_some() {
            break;
        }
        if receiver.changed().await.is_err() {
            break;
        }
    }
}

pub(super) fn start(
    events: crate::priority::Sender<NodeEvent>,
    updates: broadcast::Sender<Arc<ReadUpdate>>,
    key: [u8; 32],
) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    let state = ApiState {
        events,
        updates,
        topics: topics::Topics::default(),
        auth: format!("Bearer {}", crate::message::hex_encode(&key)),
    };
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(2)
            .enable_all()
            .build()
            .expect("read API runtime");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).expect("read API listener");
            let app = Router::new()
                .route("/v1/node/summary", get(summary))
                .route("/v1/markets/{id}", get(market))
                .route("/v1/accounts/{address}", get(account))
                .route("/v1/liquidations/{id}", get(candidates))
                .route("/v1/liquidations", get(all_candidates))
                .route("/v1/explorer/recent", get(recent_blocks))
                .route("/v1/explorer/blocks/{sequence}", get(block_detail))
                .route(
                    "/v1/explorer/transactions/{digest}",
                    get(transaction_detail),
                )
                .route("/v1/explorer/hashes/{digest}", get(hash_lookup))
                .route("/v1/markets/{id}/stream", get(stream_market))
                .route("/v1/accounts/{address}/stream", get(stream_account))
                .route("/v1/liquidations/{id}/stream", get(stream_candidates))
                .route("/v1/liquidations/stream", get(stream_all_candidates))
                .with_state(state);
            let _ = axum::serve(listener, app).await;
        });
    });
    Ok(address)
}

/// A display-only linear estimate. In cross mode it holds all other market
/// marks fixed, so the validator's transaction-time risk check stays final.
fn estimated_trigger(
    mark_atoms: u64,
    size_atoms: i128,
    price_scale: u64,
    quantity_scale: u64,
    tick: u64,
    maintenance_bps: u64,
    equity_buffer_usd_atoms: i128,
) -> Option<(u64, u32)> {
    let size = size_atoms as f64 / quantity_scale as f64;
    if size == 0.0 || mark_atoms == 0 || tick == 0 {
        return None;
    }
    let maintenance = maintenance_bps as f64 / 10_000.0;
    let slope = if size > 0.0 {
        size * (1.0 - maintenance)
    } else {
        size * (1.0 + maintenance)
    };
    if slope == 0.0 {
        return None;
    }
    let mark = mark_atoms as f64 / price_scale as f64;
    let liquidation =
        mark - (equity_buffer_usd_atoms as f64 / crate::exchange::market::USD_ATOMS as f64) / slope;
    if !liquidation.is_finite() || liquidation <= 0.0 {
        return None;
    }
    let tick_f = tick as f64;
    let estimate_atoms = ((liquidation * price_scale as f64 / tick_f).round() * tick_f)
        .clamp(tick_f, u64::MAX as f64) as u64;
    let distance_bps = ((liquidation - mark).abs() / mark * 10_000.0).min(u32::MAX as f64) as u32;
    Some((estimate_atoms, distance_bps))
}

impl ReadSnapshot {
    pub(super) fn render(&self, query: ReadQuery) -> Value {
        let height = self.height;
        let digest = &self.digest;
        match query {
            ReadQuery::Summary => json!({
                "node_id": self.node_id, "height": height, "digest": digest,
                "view": self.view, "primary": self.primary,
                "mempool_size": self.mempool, "peer_count": self.peers,
                "transactions_committed": self.transactions,
                "blocks_committed": self.blocks,
            }),
            ReadQuery::Market(id) => {
                let Some(market) = self.exchange.markets().get(&id) else {
                    return json!({"error":"market not found"});
                };
                let book = self
                    .exchange
                    .order_books()
                    .get(&id)
                    .map(|book| book.snapshot(id, 20));
                let trades = self
                    .trades
                    .iter()
                    .rev()
                    .filter(|trade| trade.market_id == id)
                    .cloned()
                    .collect::<Vec<_>>();
                let pair = OraclePair::new(market.base_asset, market.quote_asset);
                let mark = self.exchange.oracle_mark(pair).map(|mark| mark.index_e8);
                json!({"node_id":self.node_id,"height":height,"digest":digest,"market_id":id,
                    "config":market.config(),"book":book,"recent_trades":trades,"committed_mark_e8":mark})
            }
            ReadQuery::Account(address) => {
                let Some(account) = self.exchange.accounts().get(&address) else {
                    return json!({"error":"account not found"});
                };
                let balances = account.balances.iter().map(|(&asset, balance)| json!({"asset_id":asset,"available":balance.available,"locked":balance.locked})).collect::<Vec<_>>();
                let positions = self.exchange.perp_positions_for(address);
                let risk = self.exchange.perp_risk_for(address);
                let orders = self
                    .exchange
                    .account_orders(address)
                    .into_iter()
                    .map(|order| OpenOrderSnapshot {
                        order_id: order.id,
                        account_id: address,
                        market_id: order.market,
                        side: order.side,
                        price: order.price,
                        remaining_quantity: order.remaining_quantity,
                    })
                    .collect::<Vec<_>>();
                let outcomes = self
                    .outcomes
                    .iter()
                    .filter(|outcome| outcome.account_id == address)
                    .cloned()
                    .collect::<Vec<_>>();
                json!({"node_id":self.node_id,"height":height,"digest":digest,"address":address,
                    "nonce":account.nonce,"used_nonces":account.used_nonces,
                    "balances":balances,"positions":positions,"risk":risk,"open_orders":orders,
                    "recent_transaction_outcomes":outcomes})
            }
            ReadQuery::Candidates(_) | ReadQuery::CandidatesAll => {
                let selected = match query {
                    ReadQuery::Candidates(id) => Some(id),
                    _ => None,
                };
                if selected.is_some_and(|id| {
                    !self.exchange.markets().get(&id).is_some_and(|market| {
                        matches!(
                            market.market_type,
                            crate::exchange::market_config::MarketType::Perp { .. }
                        )
                    })
                }) {
                    return json!({"error":"perp market not found"});
                }
                let scoped_accounts =
                    selected.map(|market| self.exchange.perp_accounts_for_market(market));
                let positions = match &scoped_accounts {
                    Some(accounts) => accounts
                        .iter()
                        .flat_map(|&owner| self.exchange.perp_positions_for(owner))
                        .filter(|position| Some(position.market_id) == selected)
                        .collect(),
                    None => self.exchange.perp_position_snapshots(),
                };
                let risks = match &scoped_accounts {
                    Some(accounts) => accounts
                        .iter()
                        .flat_map(|&owner| self.exchange.perp_risk_for(owner))
                        .collect(),
                    None => self.exchange.perp_risk_snapshots(),
                };
                let mut positions_by_account = HashMap::<Address, Vec<_>>::new();
                for position in &positions {
                    positions_by_account
                        .entry(position.account_id)
                        .or_default()
                        .push(position);
                }
                let mut candidates = risks
                    .into_iter()
                    .filter(|risk| selected.is_some() || risk.liquidatable)
                    .filter(|risk| {
                        selected
                            .is_none_or(|id| risk.market_id == Some(id) || risk.market_id.is_none())
                    })
                    .flat_map(|risk| {
                        positions_by_account
                            .get(&risk.account_id)
                            .into_iter()
                            .flatten()
                            .filter(move |position| {
                                selected.is_none_or(|id| position.market_id == id)
                            })
                            .filter(move |position| {
                                risk.market_id.is_none_or(|id| position.market_id == id)
                            })
                            .filter_map(move |position| {
                                let market = self.exchange.markets().get(&position.market_id)?;
                                let mark_e8 = self
                                    .exchange
                                    .oracle_mark(OraclePair::new(
                                        market.base_asset,
                                        market.quote_asset,
                                    ))?
                                    .index_e8;
                                let mark_atoms = market.price.round_e8_to_tick(mark_e8).ok()?;
                                let (estimate_atoms, distance_bps) = estimated_trigger(
                                    mark_atoms,
                                    position.size_atoms,
                                    market.price.scale(),
                                    market.quantity.scale(),
                                    market.price.step,
                                    market.maintenance_margin_bps,
                                    risk.equity_usd_atoms
                                        .saturating_sub(risk.maintenance_required_usd_atoms),
                                )?;
                                (selected.is_none() || risk.liquidatable || distance_bps <= 1_000)
                                    .then(|| {
                                        (
                                            distance_bps,
                                            json!({"risk":risk,"position":position,
                                    "estimated_liquidation_price_atoms":estimate_atoms,
                                    "distance_bps":distance_bps,"mark_price_atoms":mark_atoms}),
                                        )
                                    })
                            })
                    })
                    .collect::<Vec<_>>();
                candidates.sort_by_key(|(distance, _)| *distance);
                candidates.truncate(if selected.is_some() { 30 } else { 100 });
                let candidates = candidates
                    .into_iter()
                    .map(|(_, value)| value)
                    .collect::<Vec<_>>();
                json!({"node_id":self.node_id,"height":height,"digest":digest,"market_id":selected,"candidates":candidates})
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::estimated_trigger;

    #[test]
    fn estimated_liquidation_moves_against_position_direction() {
        let mark = 800_000; // $80,000 at one decimal place
        let buffer = 1_000 * 1_000_000; // $1,000 above maintenance
        let (long_trigger, long_distance) =
            estimated_trigger(mark, 10_000, 10, 10_000, 1, 500, buffer).unwrap();
        let (short_trigger, short_distance) =
            estimated_trigger(mark, -10_000, 10, 10_000, 1, 500, buffer).unwrap();
        assert!(long_trigger < mark && short_trigger > mark);
        assert!(long_distance > 0 && short_distance > 0);
        assert!(estimated_trigger(mark, 0, 10, 10_000, 1, 500, buffer).is_none());
    }
}
