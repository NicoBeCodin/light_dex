use serde::{Deserialize, Serialize};

use super::{
    Venue, VenueQuote,
    config::{required_venues, usd_price_e8, venue_symbol},
};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

pub const MAX_FEED_AGE: Duration = Duration::from_secs(5);
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ts_rs::TS,
)]
pub struct OraclePair {
    pub base_asset: u64,
    pub quote_asset: u64,
}

impl OraclePair {
    pub const fn new(base_asset: u64, quote_asset: u64) -> Self {
        Self {
            base_asset,
            quote_asset,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OracleObservation {
    pub venue: Venue,
    pub symbol: String,
    pub bid: String,
    pub ask: String,
    pub source_sequence: Option<u64>,
    pub source_time: Option<String>,
}

// Prices returned in 10^-8 USD units.
// Required order and weights are configured per pair.
pub fn index_from_observations(
    pair: OraclePair,
    observations: &[OracleObservation],
) -> Option<u64> {
    let required = required_venues(pair)?;
    if observations.len() != required.len() {
        return None;
    }
    let mut numerator = 0u128;
    let mut total_weight = 0u128;

    for (observation, &(venue, weight)) in observations.iter().zip(required) {
        let expected_symbol = venue_symbol(pair, venue)?;

        if observation.venue != venue || observation.symbol != expected_symbol {
            return None;
        }

        let bid = usd_price_e8(venue, &observation.bid)?;
        let ask = usd_price_e8(venue, &observation.ask)?;
        if bid == 0
            || bid >= ask
            || observation.bid.len() > 32
            || observation.ask.len() > 32
            || observation
                .source_time
                .as_ref()
                .is_some_and(|value| value.len() > 64)
        {
            return None;
        }

        // Each venue contributes weight × (bid + ask) / 2.
        numerator = numerator.checked_add(bid.checked_add(ask)?.checked_mul(weight)?)?;
        total_weight = total_weight.checked_add(weight)?;
    }

    u64::try_from(numerator / (2 * total_weight)).ok()
}

use sha2::{Digest as _, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OracleUpdate {
    pub pair: OraclePair,
    pub observations: Vec<OracleObservation>,
    pub index_e8: u64,
}

impl OracleObservation {
    fn from_quote(quote: &VenueQuote) -> Self {
        Self {
            venue: quote.venue,
            symbol: quote.symbol.clone(),
            bid: quote.bid.clone(),
            ask: quote.ask.clone(),
            source_sequence: quote.source_sequence,
            source_time: quote.source_time.clone(),
        }
    }
}

/// Only the proposer uses local receipt times to select candidate inputs.
pub fn candidate_from_cache(
    pair: OraclePair,
    quotes: &HashMap<(OraclePair, Venue), (VenueQuote, Instant)>,
) -> Option<OracleUpdate> {
    let pick = |venue| {
        let (quote, received) = quotes.get(&(pair, venue))?;
        if received.elapsed() > MAX_FEED_AGE {
            return None;
        }
        Some(OracleObservation::from_quote(quote))
    };

    let observations = required_venues(pair)?
        .iter()
        .map(|(venue, _)| pick(*venue))
        .collect::<Option<Vec<_>>>()?;
    let index_e8 = index_from_observations(pair, &observations)?;

    Some(OracleUpdate {
        pair,
        observations,
        index_e8,
    })
}

/// Node-local display value. Never use this during block validation.
pub fn reference_index_e8(
    pair: OraclePair,
    quotes: &HashMap<(OraclePair, Venue), (VenueQuote, Instant)>,
) -> Option<u64> {
    Some(candidate_from_cache(pair, quotes)?.index_e8)
}

fn write_string(bytes: &mut Vec<u8>, value: &str) {
    bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
    bytes.extend_from_slice(value.as_bytes());
}

impl OracleUpdate {
    /// Canonical digest used by the block hash and committed state.
    pub fn digest(&self) -> [u8; 32] {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"light-dex/oracle-update/v1");
        bytes.extend_from_slice(&self.pair.base_asset.to_be_bytes());
        bytes.extend_from_slice(&self.pair.quote_asset.to_be_bytes());
        bytes.extend_from_slice(&self.index_e8.to_be_bytes());
        bytes.extend_from_slice(&(self.observations.len() as u64).to_be_bytes());

        for observation in &self.observations {
            bytes.push(match observation.venue {
                Venue::Binance => 0,
                Venue::Okx => 1,
                Venue::Coinbase => 2,
            });
            write_string(&mut bytes, &observation.symbol);
            write_string(&mut bytes, &observation.bid);
            write_string(&mut bytes, &observation.ask);

            match observation.source_sequence {
                Some(sequence) => {
                    bytes.push(1);
                    bytes.extend_from_slice(&sequence.to_be_bytes());
                }
                None => bytes.push(0),
            }
            match &observation.source_time {
                Some(time) => {
                    bytes.push(1);
                    write_string(&mut bytes, time);
                }
                None => bytes.push(0),
            }
        }

        Sha256::digest(&bytes).into()
    }
}

#[cfg(test)]
mod multi_market_tests {
    use super::*;
    use crate::exchange::market::{BNB, USD};

    #[test]
    fn bnb_requires_exactly_binance_and_okx_and_normalizes_weights() {
        let pair = OraclePair::new(BNB, USD);
        let observations = [
            OracleObservation {
                venue: Venue::Binance,
                symbol: "BNBUSDT".into(),
                bid: "499".into(),
                ask: "501".into(),
                source_sequence: None,
                source_time: None,
            },
            OracleObservation {
                venue: Venue::Okx,
                symbol: "BNB-USDT".into(),
                bid: "509".into(),
                ask: "511".into(),
                source_sequence: None,
                source_time: None,
            },
        ];
        assert_eq!(
            index_from_observations(pair, &observations),
            Some(504_28571428)
        );
        let mut invalid = observations.to_vec();
        invalid.push(OracleObservation {
            venue: Venue::Coinbase,
            symbol: "BNB-USD".into(),
            bid: "499".into(),
            ask: "501".into(),
            source_sequence: None,
            source_time: None,
        });
        assert_eq!(index_from_observations(pair, &invalid), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::market::{BTC, ETH, USD};
    use crate::oracle::{parse_pair_quote, parse_quote};
    #[test]
    fn requires_three_fresh_feeds_and_applies_weights() {
        let samples = [
            (Venue::Binance, r#"{"s":"BTCUSDT","b":"100","a":"102"}"#),
            (
                Venue::Okx,
                r#"{"data":[{"instId":"BTC-USDT","bidPx":"110","askPx":"112"}]}"#,
            ),
            (
                Venue::Coinbase,
                r#"{"type":"ticker","product_id":"BTC-USD","best_bid":"120","best_ask":"122"}"#,
            ),
        ];
        let mut quotes = HashMap::new();
        for (venue, sample) in samples {
            quotes.insert(
                (OraclePair::new(BTC, USD), venue),
                (parse_quote(venue, sample).unwrap(), Instant::now()),
            );
        }
        let pair = OraclePair::new(BTC, USD);
        assert_eq!(reference_index_e8(pair, &quotes), Some(11_000_000_000));
        assert_eq!(reference_index_e8(OraclePair::new(ETH, USD), &quotes), None);
        quotes.get_mut(&(pair, Venue::Coinbase)).unwrap().1 =
            Instant::now() - Duration::from_secs(6);
        assert_eq!(reference_index_e8(pair, &quotes), None);
        quotes.remove(&(pair, Venue::Coinbase));
        assert_eq!(reference_index_e8(pair, &quotes), None);
    }

    #[test]
    fn pair_identity_and_inputs_are_bound_to_update_digest() {
        let pair = OraclePair::new(ETH, USD);
        let samples = [
            (
                Venue::Binance,
                r#"{"s":"ETHUSDT","b":"2000","a":"2002","u":17}"#,
            ),
            (
                Venue::Okx,
                r#"{"data":[{"instId":"ETH-USDT","bidPx":"2010","askPx":"2012"}]}"#,
            ),
            (
                Venue::Coinbase,
                r#"{"type":"ticker","product_id":"ETH-USD","best_bid":"2020","best_ask":"2022"}"#,
            ),
        ];
        let mut quotes = HashMap::new();
        for (venue, sample) in samples {
            quotes.insert(
                (pair, venue),
                (
                    parse_pair_quote(pair, venue, sample).unwrap(),
                    Instant::now(),
                ),
            );
        }
        let update = candidate_from_cache(pair, &quotes).unwrap();
        assert_eq!(update.index_e8, 201_000_000_000);
        assert_eq!(
            update.digest(),
            candidate_from_cache(pair, &quotes).unwrap().digest()
        );
        let mut changed_pair = update.clone();
        changed_pair.pair = OraclePair::new(BTC, USD);
        assert_ne!(update.digest(), changed_pair.digest());
        assert!(index_from_observations(changed_pair.pair, &changed_pair.observations).is_none());
        let mut changed_price = update.clone();
        changed_price.observations[0].bid = "1999".into();
        assert_ne!(update.digest(), changed_price.digest());
    }
}
