use super::{config::venue_symbol, index::OraclePair};
use crate::exchange::market::{BTC, USD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum Venue {
    Binance,
    Okx,
    Coinbase,
}

impl Venue {
    pub const ALL: [Self; 3] = [Self::Binance, Self::Okx, Self::Coinbase];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Binance => "binance",
            Self::Okx => "okx",
            Self::Coinbase => "coinbase",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct VenueQuote {
    pub venue: Venue,
    pub symbol: String,
    pub bid: String,
    pub ask: String,
    pub source_sequence: Option<u64>,
    pub source_time: Option<String>,
    /// Local receipt time is diagnostic only, never a consensus timestamp.
    pub received_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct VenueFeedStatus {
    pub pair: OraclePair,
    pub venue: Venue,
    pub symbol: Option<String>,
    pub bid: Option<String>,
    pub ask: Option<String>,
    pub age_ms: Option<u64>,
    pub source_sequence: Option<u64>,
}

pub(crate) fn decimal_e8(text: &str) -> Option<u128> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty()
        || fraction.len() > 8
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let whole: u128 = whole.parse().ok()?;
    let fractional: u128 = if fraction.is_empty() {
        0
    } else {
        fraction.parse().ok()?
    };
    whole
        .checked_mul(100_000_000)?
        .checked_add(fractional.checked_mul(10u128.pow(8 - fraction.len() as u32))?)
}

pub fn parse_quote(venue: Venue, text: &str) -> Option<VenueQuote> {
    parse_pair_quote(OraclePair::new(BTC, USD), venue, text)
}

pub fn parse_pair_quote(pair: OraclePair, venue: Venue, text: &str) -> Option<VenueQuote> {
    let value: Value = serde_json::from_str(text).ok()?;
    let (symbol, bid, ask, source_sequence, source_time) = match venue {
        Venue::Binance => (
            value.get("s")?.as_str()?,
            value.get("b")?.as_str()?,
            value.get("a")?.as_str()?,
            value.get("u").and_then(Value::as_u64),
            None,
        ),
        Venue::Okx => {
            let row = value.get("data")?.as_array()?.first()?;
            (
                row.get("instId")?.as_str()?,
                row.get("bidPx")?.as_str()?,
                row.get("askPx")?.as_str()?,
                None,
                row.get("ts").and_then(Value::as_str),
            )
        }
        Venue::Coinbase => {
            if value.get("type")?.as_str()? != "ticker" {
                return None;
            }
            (
                value.get("product_id")?.as_str()?,
                value.get("best_bid")?.as_str()?,
                value.get("best_ask")?.as_str()?,
                value.get("sequence").and_then(Value::as_u64),
                value.get("time").and_then(Value::as_str),
            )
        }
    };
    let expected = venue_symbol(pair, venue)?;
    if symbol != expected {
        return None;
    }
    let bid_value = decimal_e8(bid)?;
    let ask_value = decimal_e8(ask)?;
    if bid_value == 0 || bid_value >= ask_value {
        return None;
    }
    let received_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    Some(VenueQuote {
        venue,
        symbol: symbol.into(),
        bid: bid.into(),
        ask: ask.into(),
        source_sequence,
        source_time: source_time.map(str::to_owned),
        received_at_ms,
    })
}

/// The same venue adapter reads raw frames from either a local relay URL or an exchange URL.
pub fn run_feed(
    venue: Venue,
    url: &str,
    stop: &AtomicBool,
    on_quote: impl FnMut(VenueQuote),
) -> Result<(), String> {
    run_pair_feed(OraclePair::new(BTC, USD), venue, url, stop, on_quote)
}

pub fn run_pair_feed(
    pair: OraclePair,
    venue: Venue,
    url: &str,
    stop: &AtomicBool,
    mut on_quote: impl FnMut(VenueQuote),
) -> Result<(), String> {
    let symbol = venue_symbol(pair, venue).ok_or("unsupported oracle pair")?;
    let (mut socket, _) = tungstenite::connect(url).map_err(|error| error.to_string())?;
    use tungstenite::stream::MaybeTlsStream;
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
    let subscription = match venue {
        Venue::Binance => None,
        Venue::Okx => Some(format!(
            r#"{{"op":"subscribe","args":[{{"channel":"tickers","instId":"{symbol}"}}]}}"#
        )),
        Venue::Coinbase => Some(format!(
            r#"{{"type":"subscribe","product_ids":["{symbol}"],"channels":["ticker"]}}"#
        )),
    };
    if let Some(subscription) = subscription {
        socket
            .send(tungstenite::Message::Text(subscription.into()))
            .map_err(|error| error.to_string())?;
    }
    while !stop.load(Ordering::Relaxed) {
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => {
                if let Some(quote) = parse_pair_quote(pair, venue, &text) {
                    on_quote(quote);
                }
            }
            Ok(tungstenite::Message::Ping(payload)) => {
                socket
                    .send(tungstenite::Message::Pong(payload))
                    .map_err(|error| error.to_string())?;
            }
            Ok(tungstenite::Message::Close(_)) => return Err("WebSocket closed".into()),
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_three_venue_quotes_without_price_rounding() {
        let samples = [
            (
                Venue::Binance,
                r#"{"s":"BTCUSDT","b":"85000.01","a":"85000.02","u":7}"#,
            ),
            (
                Venue::Okx,
                r#"{"data":[{"instId":"BTC-USDT","bidPx":"85000.01","askPx":"85000.02"}]}"#,
            ),
            (
                Venue::Coinbase,
                r#"{"type":"ticker","product_id":"BTC-USD","best_bid":"85000.01","best_ask":"85000.02","sequence":7}"#,
            ),
        ];
        for (venue, sample) in samples {
            let quote = parse_quote(venue, sample).unwrap();
            assert_eq!(quote.bid, "85000.01");
            assert_eq!(quote.ask, "85000.02");
        }
        assert!(parse_quote(Venue::Binance, r#"{"s":"BTCUSDT","b":"2","a":"1"}"#).is_none());
    }
}
