use rand::{Rng, rngs::StdRng};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{exchange::order::{OrderType, Side}, participants::market_maker::DesiredBook};

/// Agent policy configuration. The controller supplies external or local
/// observations; policy calculations remain testable in the root Rust crate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentStrategy {
    BinanceMirror { levels: usize, refresh_ms: u64 },
    AvellanedaStoikov {
        gamma: f64,
        k: f64,
        horizon_seconds: f64,
        volatility_window_seconds: u64,
        target_inventory_btc: u64,
        max_inventory_deviation_btc: u64,
        refresh_ms: u64,
    },
    RandomOrder {
        order_type: OrderType,
        tps: f64,
        passive_offset_ticks: u64,
        max_resting_orders: usize,
        resting_ttl_ms: u64,
    },
    TrendFollower { lookback_updates: usize, threshold_ticks: u64, tps: f64 },
}

impl AgentStrategy {
    pub fn is_maker(&self) -> bool {
        matches!(self, Self::BinanceMirror { .. } | Self::AvellanedaStoikov { .. })
    }

    pub fn needs_binance(&self) -> bool { self.is_maker() }

    pub fn interval_ms(&self) -> u64 {
        match self {
            Self::BinanceMirror { refresh_ms, .. } | Self::AvellanedaStoikov { refresh_ms, .. } => *refresh_ms,
            Self::RandomOrder { tps, .. } | Self::TrendFollower { tps, .. } => (1000.0 / tps).round().max(50.0) as u64,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::BinanceMirror { levels, refresh_ms } => {
                if !(1..=5).contains(levels) || !(250..=60_000).contains(refresh_ms) {
                    return Err("Binance mirror requires 1–5 levels and 250–60,000 ms refresh".into());
                }
            }
            Self::AvellanedaStoikov { gamma, k, horizon_seconds, volatility_window_seconds,
                target_inventory_btc, max_inventory_deviation_btc, refresh_ms } => {
                if !gamma.is_finite() || !(0.000001..=0.1).contains(gamma)
                    || !k.is_finite() || !(0.0001..=1.0).contains(k)
                    || !horizon_seconds.is_finite() || !(1.0..=3600.0).contains(horizon_seconds)
                    || !(10..=600).contains(volatility_window_seconds)
                    || *target_inventory_btc > 1000 || !(1..=100).contains(max_inventory_deviation_btc)
                    || !(250..=60_000).contains(refresh_ms) {
                    return Err("invalid A–S parameters (gamma, k, horizon, volatility window, inventory or refresh)".into());
                }
            }
            Self::RandomOrder { tps, passive_offset_ticks, max_resting_orders, resting_ttl_ms, .. } => {
                if !tps.is_finite() || !(0.01..=20.0).contains(tps)
                    || *passive_offset_ticks > 10_000 || !(1..=20).contains(max_resting_orders)
                    || !(500..=120_000).contains(resting_ttl_ms) {
                    return Err("invalid random-order rate, passive offset, resting cap or TTL".into());
                }
            }
            Self::TrendFollower { lookback_updates, threshold_ticks, tps } => {
                if !(2..=100).contains(lookback_updates) || !(1..=10_000).contains(threshold_ticks)
                    || !tps.is_finite() || !(0.01..=20.0).contains(tps) {
                    return Err("trend follower requires 2–100 updates, positive threshold and 0.01–20 TPS".into());
                }
            }
        }
        Ok(())
    }

    /// Approximate Avellaneda–Stoikov equations (29) and (30), in USD/BTC
    /// price units: r = s - q*gamma*sigma²*H and
    /// half_spread = gamma*sigma²*H/2 + ln(1+gamma/k)/gamma.
    /// sigma² is the variance of one-second Binance mid changes.
    pub fn avellaneda_quotes(&self, mid: f64, variance_per_second: f64, inventory_btc: u64) -> Option<DesiredBook> {
        let Self::AvellanedaStoikov { gamma, k, horizon_seconds, target_inventory_btc,
            max_inventory_deviation_btc, .. } = self else { return None; };
        if !mid.is_finite() || mid <= 0.0 || !variance_per_second.is_finite() || variance_per_second < 0.0 { return None; }
        let q = (inventory_btc as i128 - *target_inventory_btc as i128)
            .clamp(-(*max_inventory_deviation_btc as i128), *max_inventory_deviation_btc as i128) as f64;
        let risk = gamma * variance_per_second * horizon_seconds;
        let reservation = mid - q * risk;
        let half_spread = risk / 2.0 + (gamma / k).ln_1p() / gamma;
        let bid = (reservation - half_spread).floor();
        let ask = (reservation + half_spread).ceil();
        if !bid.is_finite() || !ask.is_finite() || ask <= 0.0 || ask >= u64::MAX as f64 { return None; }
        let mut book = DesiredBook::default();
        if bid >= 1.0 { book.bids.push(bid as u64); }
        if ask >= 1.0 { book.asks.push(ask as u64); }
        Some(book)
    }

    pub fn trend_side(&self, mid_history: &[f64]) -> Option<Side> {
        let Self::TrendFollower { lookback_updates, threshold_ticks, .. } = self else { return None; };
        if mid_history.len() <= *lookback_updates { return None; }
        let current = *mid_history.last()?;
        let old = mid_history[mid_history.len() - 1 - lookback_updates];
        if current - old >= *threshold_ticks as f64 { Some(Side::Buy) }
        else if old - current >= *threshold_ticks as f64 { Some(Side::Sell) }
        else { None }
    }

    pub fn random_side(&self, rng: &mut StdRng) -> Option<Side> {
        matches!(self, Self::RandomOrder { .. }).then(|| if rng.random_bool(0.5) { Side::Buy } else { Side::Sell })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avellaneda() -> AgentStrategy {
        AgentStrategy::AvellanedaStoikov { gamma: 0.001, k: 0.02, horizon_seconds: 30.0,
            volatility_window_seconds: 60, target_inventory_btc: 1000,
            max_inventory_deviation_btc: 5, refresh_ms: 1000 }
    }

    #[test]
    fn inventory_moves_quotes_in_the_correct_direction() {
        let policy = avellaneda();
        policy.validate().unwrap();
        let flat = policy.avellaneda_quotes(100_000.0, 100.0, 1000).unwrap();
        let long = policy.avellaneda_quotes(100_000.0, 100.0, 1002).unwrap();
        let short = policy.avellaneda_quotes(100_000.0, 100.0, 998).unwrap();
        assert!(long.bids[0] < flat.bids[0] && long.asks[0] < flat.asks[0]);
        assert!(short.bids[0] > flat.bids[0] && short.asks[0] > flat.asks[0]);
        assert!(flat.bids[0] < flat.asks[0]);
    }

    #[test]
    fn trend_reads_distinct_book_mid_updates() {
        let policy = AgentStrategy::TrendFollower { lookback_updates: 2, threshold_ticks: 3, tps: 1.0 };
        assert_eq!(policy.trend_side(&[100.0, 101.0, 104.0]), Some(Side::Buy));
        assert_eq!(policy.trend_side(&[104.0, 103.0, 100.0]), Some(Side::Sell));
        assert_eq!(policy.trend_side(&[100.0, 101.0]), None);
    }
}
