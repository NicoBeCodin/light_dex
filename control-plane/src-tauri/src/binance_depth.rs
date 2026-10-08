use light_dex::participants::market_maker::DesiredBook;
use serde::Deserialize;

pub const STREAM_URL: &str = "wss://data-stream.binance.vision/ws/btcusdt@depth5@100ms";

#[derive(Debug, Clone, Deserialize)]
pub struct PartialDepth {
    #[serde(rename = "lastUpdateId")]
    pub last_update_id: u64,
    pub bids: Vec<[String; 2]>,
    pub asks: Vec<[String; 2]>,
}

pub fn mid_price(depth: &PartialDepth) -> Option<f64> {
    let bid: f64 = depth.bids.first()?.get(0)?.parse().ok()?;
    let ask: f64 = depth.asks.first()?.get(0)?.parse().ok()?;
    (bid.is_finite() && ask.is_finite() && bid > 0.0 && ask > bid).then_some((bid + ask) / 2.0)
}

fn integer_price(price: &str, bid: bool) -> Option<u64> {
    let value: f64 = price.parse().ok()?;
    if !value.is_finite() || value <= 0.0 || value > u64::MAX as f64 {
        return None;
    }
    Some(if bid { value.floor() } else { value.ceil() } as u64)
}

/// Chain prices are whole-USD integers. Keep level order and one-dollar tick
/// separation when Binance's finer prices map to the same chain tick.
pub fn desired_book(depth: &PartialDepth, levels: usize) -> DesiredBook {
    let mut book = DesiredBook::default();
    for entry in &depth.bids {
        if book.bids.len() == levels {
            break;
        }
        if entry[1]
            .parse::<f64>()
            .ok()
            .is_none_or(|quantity| quantity <= 0.0)
        {
            continue;
        }
        let Some(mut price) = integer_price(&entry[0], true) else {
            continue;
        };
        if let Some(&previous) = book.bids.last() {
            price = price.min(previous.saturating_sub(1));
        }
        if price > 0 {
            book.bids.push(price);
        }
    }
    for entry in &depth.asks {
        if book.asks.len() == levels {
            break;
        }
        if entry[1]
            .parse::<f64>()
            .ok()
            .is_none_or(|quantity| quantity <= 0.0)
        {
            continue;
        }
        let Some(mut price) = integer_price(&entry[0], false) else {
            continue;
        };
        if let Some(&previous) = book.asks.last() {
            price = price.max(previous.saturating_add(1));
        }
        if price > 0 {
            book.asks.push(price);
        }
    }
    if book
        .bids
        .first()
        .zip(book.asks.first())
        .is_some_and(|(bid, ask)| bid >= ask)
    {
        return DesiredBook::default();
    }
    book
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_decimal_binance_levels_to_distinct_integer_ticks() {
        let depth: PartialDepth = serde_json::from_str(r#"{"lastUpdateId":12,"bids":[["82762.94","4"],["82762.93","2"]],"asks":[["82762.95","1"],["82762.96","2"]]}"#).unwrap();
        assert_eq!(depth.last_update_id, 12);
        assert_eq!(
            desired_book(&depth, 2),
            DesiredBook {
                bids: vec![82762, 82761],
                asks: vec![82763, 82764]
            }
        );
    }

    #[test]
    #[ignore = "requires live Binance connectivity"]
    fn receives_live_partial_depth() {
        let (mut socket, _) = tungstenite::connect(STREAM_URL).unwrap();
        loop {
            if let tungstenite::Message::Text(text) = socket.read().unwrap() {
                let depth: PartialDepth = serde_json::from_str(&text).unwrap();
                assert!(!desired_book(&depth, 1).bids.is_empty());
                break;
            }
        }
    }
}
