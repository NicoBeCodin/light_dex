//! A transport-only WebSocket relay. It does not parse prices or calculate an index.
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use tungstenite::{Message, stream::MaybeTlsStream};

const NAMES: [&str; 3] = ["binance", "okx", "coinbase"];
pub const DEFAULT_URLS: [&str; 3] = [
    "wss://data-stream.binance.vision/ws/btcusdt@bookTicker",
    "wss://wseea.okx.com/ws/v5/public",
    "wss://ws-feed.exchange.coinbase.com",
];

/// Each feed has one upstream connection per hub, irrespective of validator count.
/// The hub only filters and relays raw frames; nodes still parse and vote on
/// their own oracle observations.
#[derive(Clone, Debug)]
pub struct FeedSpec {
    pub base_asset: u64,
    pub venue: String,
    pub symbol: String,
    pub upstream_url: String,
}

pub struct HubHandle {
    addresses: HashMap<(u64, String), SocketAddr>,
    stop: Arc<AtomicBool>,
}

impl HubHandle {
    /// Binds three local WS endpoints; each has exactly one upstream WS connection.
    pub fn start(bind_ip: IpAddr, upstream_urls: [String; 3]) -> io::Result<Self> {
        let feeds = NAMES
            .into_iter()
            .zip(["BTCUSDT", "BTC-USDT", "BTC-USD"])
            .zip(upstream_urls)
            .map(|((venue, symbol), upstream_url)| FeedSpec {
                base_asset: 1,
                venue: venue.into(),
                symbol: symbol.into(),
                upstream_url,
            })
            .collect();
        Self::start_many(bind_ip, feeds)
    }

    pub fn start_many(bind_ip: IpAddr, feeds: Vec<FeedSpec>) -> io::Result<Self> {
        if feeds.is_empty() {
            return Err(io::Error::other("market data hub needs at least one feed"));
        }
        let mut listeners = Vec::with_capacity(feeds.len());
        let mut addresses = HashMap::new();
        for feed in &feeds {
            if !matches!(feed.venue.as_str(), "binance" | "okx" | "coinbase")
                || feed.symbol.is_empty()
            {
                return Err(io::Error::other("invalid market data feed"));
            }
            let listener = TcpListener::bind(SocketAddr::new(bind_ip, 0))?;
            addresses.insert(
                (feed.base_asset, feed.venue.clone()),
                listener.local_addr()?,
            );
            listeners.push(listener);
        }
        if addresses.len() != feeds.len() {
            return Err(io::Error::other("duplicate market data feed"));
        }
        for listener in &listeners {
            listener.set_nonblocking(true)?;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<(usize, String)>();
        for (index, feed) in feeds.iter().cloned().enumerate() {
            let tx = tx.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Err(error) = read_upstream(&feed, &stop, |text| {
                        let _ = tx.send((index, text));
                    }) {
                        eprintln!("{} {} feed: {error}", feed.venue, feed.symbol);
                    }
                    if !stop.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_secs(2));
                    }
                }
            });
        }
        drop(tx);
        let relay_stop = stop.clone();
        thread::spawn(move || {
            let mut latest = vec![None::<String>; listeners.len()];
            let mut subscribers = vec![Vec::<SyncSender<String>>::new(); listeners.len()];
            let mut last_sent = vec![None::<Instant>; listeners.len()];
            while !relay_stop.load(Ordering::Relaxed) {
                // Bounded work keeps accepting subscribers responsive under high feed rates.
                for _ in 0..1_000 {
                    let Ok((index, text)) = rx.try_recv() else {
                        break;
                    };
                    latest[index] = Some(text.clone());
                    if last_sent[index].is_some_and(|at| at.elapsed() < Duration::from_millis(200))
                    {
                        continue;
                    }
                    last_sent[index] = Some(Instant::now());
                    subscribers[index]
                        .retain(|subscriber| subscriber.try_send(text.clone()).is_ok());
                }
                for (index, listener) in listeners.iter().enumerate() {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let (subscriber, receiver) = mpsc::sync_channel(32);
                            subscribers[index].push(subscriber);
                            let initial = latest[index].clone();
                            let stop = relay_stop.clone();
                            thread::spawn(move || serve_node(stream, initial, receiver, stop));
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                        Err(error) => eprintln!(
                            "{} {} relay accept: {error}",
                            feeds[index].venue, feeds[index].symbol
                        ),
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        Ok(Self { addresses, stop })
    }

    pub fn url(&self, venue: &str) -> Option<String> {
        self.url_for(1, venue)
    }

    pub fn url_for(&self, base_asset: u64, venue: &str) -> Option<String> {
        self.addresses
            .get(&(base_asset, venue.to_string()))
            .map(|address| format!("ws://{address}"))
    }
}

impl Drop for HubHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn is_quote(feed: &FeedSpec, text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    match feed.venue.as_str() {
        "binance" => {
            value.get("s").and_then(Value::as_str) == Some(feed.symbol.as_str())
                && value.get("b").is_some()
                && value.get("a").is_some()
        }
        "okx" => {
            value
                .get("data")
                .and_then(Value::as_array)
                .and_then(|rows| rows.first())
                .and_then(|row| row.get("instId"))
                .and_then(Value::as_str)
                == Some(feed.symbol.as_str())
        }
        "coinbase" => {
            value.get("type").and_then(Value::as_str) == Some("ticker")
                && value.get("product_id").and_then(Value::as_str) == Some(feed.symbol.as_str())
        }
        _ => false,
    }
}

fn read_upstream(
    feed: &FeedSpec,
    stop: &AtomicBool,
    mut emit: impl FnMut(String),
) -> Result<(), String> {
    let (mut socket, _) =
        tungstenite::connect(feed.upstream_url.as_str()).map_err(|error| error.to_string())?;
    let timeout = Some(Duration::from_secs(2));
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => {
            let _ = stream.set_read_timeout(timeout);
        }
        MaybeTlsStream::NativeTls(stream) => {
            let _ = stream.get_mut().set_read_timeout(timeout);
        }
        _ => {}
    }
    let subscription = match feed.venue.as_str() {
        "binance" => None,
        "okx" => Some(format!(
            r#"{{"op":"subscribe","args":[{{"channel":"tickers","instId":"{}"}}]}}"#,
            feed.symbol
        )),
        "coinbase" => Some(format!(
            r#"{{"type":"subscribe","product_ids":["{}"],"channels":["ticker"]}}"#,
            feed.symbol
        )),
        _ => unreachable!(),
    };
    if let Some(subscription) = subscription {
        socket
            .send(Message::Text(subscription.into()))
            .map_err(|error| error.to_string())?;
    }
    while !stop.load(Ordering::Relaxed) {
        match socket.read() {
            Ok(Message::Text(text)) if is_quote(feed, &text) => emit(text.to_string()),
            Ok(Message::Ping(payload)) => socket
                .send(Message::Pong(payload))
                .map_err(|error| error.to_string())?,
            Ok(Message::Close(_)) => return Err("upstream closed".into()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

fn serve_node(
    stream: TcpStream,
    initial: Option<String>,
    receiver: mpsc::Receiver<String>,
    stop: Arc<AtomicBool>,
) {
    let Ok(mut socket) = tungstenite::accept(stream) else {
        return;
    };
    if let Some(text) = initial {
        if socket.send(Message::Text(text.into())).is_err() {
            return;
        }
    }
    while !stop.load(Ordering::Relaxed) {
        match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(text) => {
                if socket.send(Message::Text(text.into())).is_err() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relays_three_raw_websocket_feeds() {
        let payloads = [
            r#"{"s":"BTCUSDT","b":"100","a":"101"}"#,
            r#"{"data":[{"instId":"BTC-USDT","bidPx":"100","askPx":"101"}]}"#,
            r#"{"type":"ticker","product_id":"BTC-USD","best_bid":"100","best_ask":"101"}"#,
        ];
        let mut upstream = Vec::new();
        for (index, payload) in payloads.into_iter().enumerate() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            upstream.push(format!("ws://{}", listener.local_addr().unwrap()));
            thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                let mut socket = tungstenite::accept(stream).unwrap();
                if index != 0 {
                    let _ = socket.read().unwrap();
                }
                socket.send(Message::Text(payload.into())).unwrap();
                thread::sleep(Duration::from_millis(300));
            });
        }
        let hub =
            HubHandle::start("127.0.0.1".parse().unwrap(), upstream.try_into().unwrap()).unwrap();
        for (index, name) in NAMES.into_iter().enumerate() {
            let (mut socket, _) = tungstenite::connect(hub.url(name).unwrap()).unwrap();
            let message = socket.read().unwrap().into_text().unwrap();
            assert_eq!(message, payloads[index]);
        }
    }
}
