//! Temporary local prototype policy; not a real USD/USDT peg guarantee.
use super::{Venue, feed::decimal_e8, index::OraclePair};
use crate::exchange::market::{BNB, BTC, ETH, SOL, USD, XRP};

pub const USDT_TO_USD_NUMERATOR: u128 = 1;
pub const USDT_TO_USD_DENOMINATOR: u128 = 1;

/// BNB has only the two verified local source adapters. The protocol requires
/// both, then normalizes their 40:30 weights. Other pairs require all three.
pub fn required_venues(pair: OraclePair) -> Option<&'static [(Venue, u128)]> {
    const THREE: &[(Venue, u128)] = &[
        (Venue::Binance, 40),
        (Venue::Okx, 30),
        (Venue::Coinbase, 30),
    ];
    const BNB_TWO: &[(Venue, u128)] = &[(Venue::Binance, 40), (Venue::Okx, 30)];
    match (pair.base_asset, pair.quote_asset) {
        (BNB, USD) => Some(BNB_TWO),
        (BTC | ETH | XRP | SOL, USD) => Some(THREE),
        _ => None,
    }
}

/// Protocol-level feed symbol registry. Adding a pair requires a deliberate
/// configuration change, never a fallback to another asset's quotes.
pub fn venue_symbol(pair: OraclePair, venue: Venue) -> Option<&'static str> {
    let symbols = match (pair.base_asset, pair.quote_asset) {
        (BTC, USD) => ["BTCUSDT", "BTC-USDT", "BTC-USD"],
        (ETH, USD) => ["ETHUSDT", "ETH-USDT", "ETH-USD"],
        (SOL, USD) => ["SOLUSDT", "SOL-USDT", "SOL-USD"],
        (BNB, USD) => ["BNBUSDT", "BNB-USDT", ""],
        (XRP, USD) => ["XRPUSDT", "XRP-USDT", "XRP-USD"],
        _ => return None,
    };
    let symbol = match venue {
        Venue::Binance => symbols[0],
        Venue::Okx => symbols[1],
        Venue::Coinbase => symbols[2],
    };
    (!symbol.is_empty()).then_some(symbol)
}

pub fn quote_currency(venue: Venue) -> &'static str {
    match venue {
        Venue::Binance | Venue::Okx => "USDT",
        Venue::Coinbase => "USD",
    }
}

pub fn usd_price_e8(venue: Venue, price: &str) -> Option<u128> {
    let raw = decimal_e8(price)?;
    match quote_currency(venue) {
        "USD" => Some(raw),
        "USDT" => raw
            .checked_mul(USDT_TO_USD_NUMERATOR)?
            .checked_div(USDT_TO_USD_DENOMINATOR),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn current_usdt_policy_is_explicitly_one_to_one_with_usd() {
        assert_eq!((USDT_TO_USD_NUMERATOR, USDT_TO_USD_DENOMINATOR), (1, 1));
        assert_eq!(quote_currency(Venue::Binance), "USDT");
        assert_eq!(quote_currency(Venue::Coinbase), "USD");
        for venue in Venue::ALL {
            assert_eq!(
                usd_price_e8(venue, "85000.12345678"),
                Some(8_500_012_345_678)
            );
        }
    }
}
