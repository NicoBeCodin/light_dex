//! Checked-in genesis market catalog. Every validator compiles the same TOML
//! into its genesis state; the resulting market leaves are part of the root.
use serde::Deserialize;
use std::collections::HashSet;

use super::{market::MarketId, market_config::MarketConfig};

#[derive(Deserialize)]
struct Catalog {
    markets: Vec<CatalogEntry>,
}

#[derive(Deserialize)]
struct CatalogEntry {
    id: MarketId,
    #[serde(flatten)]
    config: MarketConfig,
}

pub(crate) fn genesis_markets() -> Result<Vec<(MarketId, MarketConfig)>, String> {
    let catalog: Catalog = toml::from_str(include_str!("../../config/markets.toml"))
        .map_err(|error| error.to_string())?;
    let mut ids = HashSet::new();
    let mut pairs = HashSet::new();
    let mut markets = Vec::new();
    for entry in catalog.markets {
        entry.config.validate().map_err(str::to_string)?;
        if !ids.insert(entry.id)
            || !pairs.insert((
                entry.config.base_asset,
                entry.config.quote_asset,
                matches!(
                    entry.config.market_type,
                    super::market_config::MarketType::Perp { .. }
                ),
            ))
        {
            return Err("duplicate market ID or asset pair/type".into());
        }
        markets.push((entry.id, entry.config));
    }
    markets.sort_unstable_by_key(|(id, _)| *id);
    if markets.first().map(|(id, _)| *id) != Some(0) {
        return Err("market 0 must be BTC-USD spot".into());
    }
    Ok(markets)
}

#[cfg(test)]
mod tests {
    #[test]
    fn catalog_has_five_spot_and_five_perp_markets() {
        let markets = super::genesis_markets().unwrap();
        assert_eq!(markets.len(), 10);
        assert_eq!(
            markets
                .iter()
                .filter(|(_, config)| matches!(
                    config.market_type,
                    crate::exchange::market_config::MarketType::Perp { .. }
                ))
                .count(),
            5
        );
    }
}
