use light_dex::exchange::market_config::DecimalStep;
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

/// Keep the existing BTC-USD whole-dollar behavior.
#[cfg(test)]
pub fn desired_book(depth: &PartialDepth, levels: usize) -> DesiredBook {
    desired_book_for_market(
        depth,
        levels,
        DecimalStep {
            decimals: 0,
            step: 1,
        },
    )
}

/// Convert decimal feed strings into integer chain units using the market's
/// configured tick. Bids round down and asks round up without using f64.
pub fn desired_book_for_market(
    depth: &PartialDepth,
    levels: usize,
    tick: DecimalStep,
) -> DesiredBook {
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
        let Ok(mut price) = tick.parse_bid(&entry[0]) else {
            continue;
        };
        if let Some(&previous) = book.bids.last() {
            price = price.min(previous.saturating_sub(tick.step));
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
        let Ok(mut price) = tick.parse_ask(&entry[0]) else {
            continue;
        };
        if let Some(&previous) = book.asks.last() {
            price = price.max(previous.saturating_add(tick.step));
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
    fn maps_feed_to_fractional_chain_tick() {
        let depth: PartialDepth = serde_json::from_str(r#"{"lastUpdateId":1,"bids":[["12.349","1"],["12.348","1"]],"asks":[["12.351","1"],["12.352","1"]]}"#).unwrap();
        let tick = DecimalStep::new(2, 5).unwrap();
        assert_eq!(
            desired_book_for_market(&depth, 2, tick),
            DesiredBook {
                bids: vec![1230, 1225],
                asks: vec![1240, 1245],
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
