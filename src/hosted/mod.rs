//! Public gateway. This binary deliberately has no operator-command dispatch.
mod admission;
use admission::{admit, submit};
mod observer;
use observer::{observe, observe_commits, observe_trades, retry_submissions};
mod benchmark;
mod bots;
mod store;
use axum::{
    Json, Router,
    extract::{
        ConnectInfo, DefaultBodyLimit, Path, State,
        ws::{Message as WsMessage, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
};
use light_dex::{
    blockchain::Transaction,
    crypto::{SessionManifest, SignedTransaction, verify_transaction},
    message::{AdmissionReply, Message, hex_decode, hex_encode, sign_line, verify_line},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
#[derive(Clone, Deserialize, Serialize)]
pub struct NodeConfig {
    pub id: u64,
    pub region: String,
    pub peer: SocketAddr,
    pub read: String,
}
#[derive(Deserialize, Serialize)]
pub struct Config {
    pub manifest: PathBuf,
    pub auth_key_file: PathBuf,
    pub database: PathBuf,
    pub frontend_dir: PathBuf,
    pub public_origin: String,
    pub nodes: Vec<NodeConfig>,
    #[serde(default)]
    pub writes_enabled: bool,
    #[serde(default = "default_public_backlog")]
    pub max_public_backlog: u64,
}
fn default_public_backlog() -> u64 {
    128
}
struct Cached {
    at: Instant,
    value: Value,
}
struct Limits {
    buckets: HashMap<String, (Instant, u32)>,
}
impl Limits {
    fn take(&mut self, key: String, maximum: u32, window: Duration) -> bool {
        if self.buckets.len() > 8192 {
            self.buckets
                .retain(|_, (at, _)| at.elapsed() < Duration::from_secs(60));
        }
        if self.buckets.len() > 8192 {
            return false;
        }
        let (at, count) = self.buckets.entry(key).or_insert((Instant::now(), 0));
        if at.elapsed() >= window {
            *at = Instant::now();
            *count = 0;
        }
        if *count >= maximum {
            return false;
        }
        *count += 1;
        true
    }
}
struct App {
    config: Config,
    manifest: SessionManifest,
    auth: [u8; 32],
    client: reqwest::Client,
    db: Mutex<store::Store>,
    cache: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Option<Cached>>>>>,
    limits: Mutex<Limits>,
    submissions: tokio::sync::Semaphore,
    sockets: Arc<tokio::sync::Semaphore>,
    pending: Mutex<HashMap<String, Instant>>,
    latencies: Mutex<VecDeque<(Instant, u64)>>,
    network: tokio::sync::RwLock<Value>,
    started: Instant,
    read_source: std::sync::atomic::AtomicU64,
    free_bytes: std::sync::atomic::AtomicU64,
}
type Shared = Arc<App>;
type Failure = (StatusCode, Json<Value>);
fn error(code: StatusCode, message: &str) -> Failure {
    (code, Json(json!({"error":message})))
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn client_key(headers: &HeaderMap, address: SocketAddr, auth: &[u8; 32]) -> String {
    let ip = if address.ip().is_loopback() {
        headers
            .get("x-real-ip")
            .and_then(|s| s.to_str().ok())
            .and_then(|s| s.parse::<IpAddr>().ok())
            .unwrap_or(address.ip())
    } else {
        address.ip()
    };
    let mut hash = Sha256::new();
    hash.update(auth);
    hash.update(ip.to_string().as_bytes());
    hash.update((now() / 86400).to_be_bytes());
    hex_encode(&hash.finalize())
}
fn allowed(path: &str) -> bool {
    let p = path.split('/').collect::<Vec<_>>();
    match p.as_slice() {
        ["node", "summary"] | ["explorer", "recent"] | ["liquidations"] => true,
        ["markets", id] | ["liquidations", id] => id.parse::<u64>().is_ok_and(|id| id < 10),
        ["accounts", address] => hex_decode::<32>(address).is_ok(),
        ["explorer", "blocks", id] => id.parse::<u64>().is_ok(),
        ["explorer", "transactions", digest] | ["explorer", "hashes", digest] => {
            hex_decode::<32>(digest).is_ok()
        }
        _ => false,
    }
}
async fn read_node(app: &App, path: &str) -> Result<Value, Failure> {
    let snapshot = app.network.read().await;
    let mut nodes = app.config.nodes.iter().collect::<Vec<_>>();
    let heights = snapshot["validators"].as_array();
    let maximum = heights
        .into_iter()
        .flatten()
        .filter_map(|n| n["height"].as_u64())
        .max()
        .unwrap_or(0);
    let selected = app.read_source.load(std::sync::atomic::Ordering::Relaxed);
    nodes.sort_by_key(|node| (node.id != selected, node.region != "Tokyo", node.id));
    nodes.retain(|node| {
        heights
            .and_then(|rows| rows.iter().find(|n| n["id"] == node.id))
            .and_then(|n| n["height"].as_u64())
            .is_none_or(|height| height.saturating_add(2) >= maximum)
    });
    drop(snapshot);
    let mut not_found = false;
    for node in nodes {
        let response = app
            .client
            .get(format!("{}/v1/{path}", node.read.trim_end_matches('/')))
            .bearer_auth(hex_encode(&app.auth))
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => {
                if response
                    .content_length()
                    .is_some_and(|n| n > 4 * 1024 * 1024)
                {
                    continue;
                }
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|_| error(StatusCode::BAD_GATEWAY, "Node response interrupted"))?;
                if bytes.len() > 4 * 1024 * 1024 {
                    continue;
                }
                if let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) {
                    app.read_source
                        .store(node.id, std::sync::atomic::Ordering::Relaxed);
                    value["source_validator"] = json!(node.id);
                    return Ok(value);
                }
            }
            Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => {
                not_found = true;
            }
            _ => {}
        }
    }
    if not_found {
        return Err(error(
            StatusCode::NOT_FOUND,
            "Account, market or retained history not found",
        ));
    }
    Err(error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Network is temporarily unavailable",
    ))
}
async fn cached(app: &App, path: &str) -> Result<Value, Failure> {
    let entry = {
        let mut cache = app.cache.lock().unwrap();
        if cache.len() >= 512 {
            cache.retain(|_, entry| Arc::strong_count(entry) > 1);
            if cache.len() >= 512 {
                return Err(error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Too many active resources",
                ));
            }
        }
        cache
            .entry(path.into())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None)))
            .clone()
    };
    let mut entry = entry.lock().await;
    if let Some(c) = entry.as_ref() {
        if c.at.elapsed() < Duration::from_millis(500) {
            return Ok(c.value.clone());
        }
    }
    let value = read_node(app, path).await?;
    *entry = Some(Cached {
        at: Instant::now(),
        value: value.clone(),
    });
    Ok(value)
}
async fn metadata(State(app): State<Shared>) -> Json<Value> {
    let mut markets = light_dex::exchange::ExchangeState::with_claim_faucet(app.manifest.faucet)
        .unwrap()
        .markets()
        .iter()
        .map(|(&id, m)| json!({"id":id,"config":m.config()}))
        .collect::<Vec<_>>();
    markets.sort_by_key(|market| market["id"].as_u64());
    Json(
        json!({"chain_id":app.manifest.chain_id,"protocol":app.manifest.version,"consensus":"HotStuff2","rotation_blocks":app.manifest.auto_view_change_blocks,"faucet":app.manifest.faucet,"markets":markets,"writes_enabled":app.config.writes_enabled,"history_days":7,"entry_region":"Tokyo","revision":std::env::var("LIGHT_DEX_REVISION").unwrap_or_else(|_|"development".into()),"validators":app.config.nodes.iter().map(|n|json!({"id":n.id,"region":n.region})).collect::<Vec<_>>()}),
    )
}
async fn network(State(app): State<Shared>) -> Json<Value> {
    Json(app.network.read().await.clone())
}
async fn rpc(
    State(app): State<Shared>,
    Path(path): Path<String>,
    headers: HeaderMap,
    ConnectInfo(address): ConnectInfo<SocketAddr>,
) -> Result<Json<Value>, Failure> {
    if !allowed(&path) {
        return Err(error(StatusCode::NOT_FOUND, "Unknown public resource"));
    }
    if !app.limits.lock().unwrap().take(
        format!("read:{}", client_key(&headers, address, &app.auth)),
        120,
        Duration::from_secs(10),
    ) {
        return Err(error(
            StatusCode::TOO_MANY_REQUESTS,
            "Read rate limit exceeded",
        ));
    }
    if let Some(digest) = path.strip_prefix("explorer/transactions/") {
        let value = app
            .db
            .lock()
            .unwrap()
            .lookup(digest)
            .map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "History unavailable"))?;
        if value["status"] == "committed" {
            return Ok(Json(value));
        }
    }
    if let Some(sequence) = path.strip_prefix("explorer/blocks/") {
        if let Ok(body) = app.db.lock().unwrap().0.query_row(
            "SELECT body FROM blocks WHERE sequence=?",
            [sequence],
            |r| r.get::<_, String>(0),
        ) {
            if let Ok(value) = serde_json::from_str::<Value>(&body) {
                return Ok(Json(value));
            }
        }
    }
    Ok(Json(cached(&app, &path).await?))
}
async fn status(
    State(app): State<Shared>,
    Path(digest): Path<String>,
) -> Result<Json<Value>, Failure> {
    if hex_decode::<32>(&digest).is_err() {
        return Err(error(StatusCode::BAD_REQUEST, "Invalid digest"));
    }
    app.db
        .lock()
        .unwrap()
        .lookup(&digest)
        .map(Json)
        .map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "History unavailable"))
}
async fn stream(
    State(app): State<Shared>,
    Path(path): Path<String>,
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    ConnectInfo(address): ConnectInfo<SocketAddr>,
) -> Result<Response, Failure> {
    if !allowed(&path) {
        return Err(error(StatusCode::NOT_FOUND, "Unknown stream"));
    }
    if headers.get("origin").and_then(|v| v.to_str().ok())
        != Some(app.config.public_origin.as_str())
    {
        return Err(error(StatusCode::FORBIDDEN, "Origin not allowed"));
    }
    let permit = app
        .sockets
        .clone()
        .try_acquire_owned()
        .map_err(|_| error(StatusCode::TOO_MANY_REQUESTS, "Stream capacity reached"))?;
    if !app.limits.lock().unwrap().take(
        format!("ws:{}", client_key(&headers, address, &app.auth)),
        30,
        Duration::from_secs(60),
    ) {
        return Err(error(StatusCode::TOO_MANY_REQUESTS, "Too many streams"));
    }
    Ok(ws.max_message_size(1024).max_frame_size(1024).on_upgrade(move |mut socket| async move {
        let _permit = permit;
        let mut last = String::new();
        loop {
            let value = match cached(&app, &path).await {
                Ok(value) => value,
                Err(_) => json!({"error":"Stream temporarily unavailable"}),
            };
            let text = value.to_string();
            if text != last {
                if !matches!(
                    tokio::time::timeout(
                        Duration::from_secs(2),
                        socket.send(WsMessage::Text(text.clone().into()))
                    )
                    .await,
                    Ok(Ok(()))
                ) {
                    break;
                }
                last = text;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(500)) => {},
                incoming = socket.recv() => {
                    if matches!(incoming, None | Some(Err(_)) | Some(Ok(WsMessage::Close(_)))) {break;}
                }
            }
        }
    }))
}
fn disk_free(path: &std::path::Path) -> std::io::Result<u64> {
    let output = std::process::Command::new("df")
        .args(["-Pk"])
        .arg(path.parent().unwrap_or(std::path::Path::new(".")))
        .output()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .nth(1)
        .and_then(|l| l.split_whitespace().nth(3))
        .and_then(|n| n.parse::<u64>().ok())
        .map(|n| n * 1024)
        .ok_or_else(|| std::io::Error::other("cannot read free disk space"))
}
async fn candles(
    State(app): State<Shared>,
    Path(market): Path<u64>,
) -> Result<Json<Value>, Failure> {
    let db = app.db.lock().unwrap();
    let mut query=db.0.prepare("SELECT body FROM trades WHERE market=? AND created>? ORDER BY sequence,tx,fill LIMIT 10000").map_err(|_|error(StatusCode::SERVICE_UNAVAILABLE,"History unavailable"))?;
    let rows = query
        .query_map(rusqlite::params![market, now() - 300], |r| {
            r.get::<_, String>(0)
        })
        .map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "History unavailable"))?;
    let trades = rows
        .filter_map(Result::ok)
        .filter_map(|s| serde_json::from_str::<Value>(&s).ok())
        .collect::<Vec<_>>();
    Ok(Json(json!({"market_id":market,"trades":trades})))
}
async fn faucet(
    State(app): State<Shared>,
    Path(address): Path<String>,
) -> Result<Json<Value>, Failure> {
    if hex_decode::<32>(&address).is_err() {
        return Err(error(StatusCode::BAD_REQUEST, "Invalid address"));
    }
    let account = cached(&app, &format!("accounts/{address}")).await;
    Ok(Json(
        json!({"account_exists":account.is_ok(),"writes_enabled":app.config.writes_enabled,"grant":app.manifest.faucet,"note":"One on-chain claim per address; public daily quotas apply"}),
    ))
}
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).map(String::as_str) == Some("init") {
        let path = PathBuf::from(
            args.get(2)
                .ok_or("usage: light-dex-hosted init DIRECTORY")?,
        );
        let manifest = light_dex::crypto::create_session_with_options(
            &path,
            "hotstuff2",
            4,
            light_dex::crypto::FaucetTerms {
                max_claims: 100000,
                usd_per_claim: 100000 * 1000000,
                btc_per_claim: 10 * 10000,
            },
            Some(4),
        )?;
        let secret = hex_encode(&rand::random::<[u8; 32]>());
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path.join("auth.key"))?;
        file.write_all(secret.as_bytes())?;
        file.sync_all()?;
        println!("Initialized chain {}", manifest.chain_id);
        return Ok(());
    }
    let config_path = PathBuf::from(args.get(1).ok_or("usage: light-dex-hosted CONFIG.json")?);
    let config: Config = serde_json::from_slice(&std::fs::read(&config_path)?)?;
    let manifest = SessionManifest::load(&config.manifest)?;
    if manifest.consensus != "hotstuff2"
        || config.nodes.len() != 4
        || config
            .nodes
            .iter()
            .map(|n| n.id)
            .collect::<std::collections::HashSet<_>>()
            .len()
            != 4
        || config
            .nodes
            .iter()
            .any(|n| !manifest.validators.contains_key(&n.id))
    {
        return Err("hosted configuration must match four HotStuff2 validators".into());
    }
    if !config.public_origin.starts_with("https://")
        && !config.public_origin.starts_with("http://127.0.0.1:")
    {
        return Err("public origin must use HTTPS".into());
    }
    let auth = hex_decode::<32>(std::fs::read_to_string(&config.auth_key_file)?.trim())?;
    if args.get(2).map(String::as_str) == Some("bench") {
        return benchmark::run(
            config,
            manifest,
            args.get(3).ok_or("missing rate")?.parse()?,
            args.get(4).ok_or("missing seconds")?.parse()?,
        )
        .await;
    }
    if args.get(2).map(String::as_str) == Some("bots") {
        return bots::run(config, manifest).await;
    }
    let db = store::Store::open(&config.database)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .connect_timeout(Duration::from_millis(500))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let app = Arc::new(App {
        free_bytes: std::sync::atomic::AtomicU64::new(disk_free(&config.database).unwrap_or(0)),
        config,
        manifest,
        auth,
        client,
        db: Mutex::new(db),
        cache: Mutex::new(HashMap::new()),
        limits: Mutex::new(Limits {
            buckets: HashMap::new(),
        }),
        submissions: tokio::sync::Semaphore::new(16),
        sockets: Arc::new(tokio::sync::Semaphore::new(128)),
        pending: Mutex::new(HashMap::new()),
        latencies: Mutex::new(VecDeque::new()),
        network: tokio::sync::RwLock::new(json!({"status":"starting"})),
        started: Instant::now(),
        read_source: std::sync::atomic::AtomicU64::new(0),
    });
    tokio::spawn(observe(app.clone()));
    tokio::spawn(observe_commits(app.clone()));
    tokio::spawn(observe_trades(app.clone()));
    tokio::spawn(retry_submissions(app.clone()));
    let router = Router::new()
        .route("/api/v1/meta", get(metadata))
        .route("/api/v1/network", get(network))
        .route("/api/v1/transactions", post(submit))
        .route("/api/v1/transactions/{digest}", get(status))
        .route("/api/v1/faucet/{address}", get(faucet))
        .route("/api/v1/history/{market}", get(candles))
        .route("/api/v1/stream/{*path}", get(stream))
        .route("/api/v1/rpc/{*path}", get(rpc))
        .layer(DefaultBodyLimit::max(32768))
        .fallback_service(tower_http::services::ServeDir::new(
            &app.config.frontend_dir,
        ))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:1422").await?;
    println!("Public gateway listening on loopback port 1422");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
