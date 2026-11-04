use crate::exchange::account::AssetId;
use crate::exchange::market_config::{DecimalStep, MarketConfig, MarketType};

pub type MarketId = u64;

pub const USD: AssetId = 0;
pub const BTC: AssetId = 1;
pub const ETH: AssetId = 2;
pub const SOL: AssetId = 3;
pub const BNB: AssetId = 4;
pub const XRP: AssetId = 5;

pub const BTC_USD: MarketId = 0;
/// Account balances are stored in these smallest units, never floating point.
pub const USD_ATOMS: u64 = 1_000_000;
pub const BTC_ATOMS: u64 = 10_000;

pub fn asset_atoms(asset: AssetId) -> Option<u64> {
    match asset {
        USD => Some(USD_ATOMS),
        BTC | ETH | SOL | BNB => Some(10_000),
        XRP => Some(100),
        _ => None,
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Market {
    pub id: MarketId,
    pub base_asset: AssetId,
    pub quote_asset: AssetId,
    pub price: DecimalStep,
    pub quantity: DecimalStep,
    pub market_type: MarketType,
    pub fees: super::market_config::FeeSchedule,
    pub maintenance_margin_bps: u64,
    pub funding_interval_updates: u64,
    pub max_funding_bps: u64,
    pub max_liquidation_slippage_bps: u64,
}

impl Market {
    pub fn supports_current_spot_settlement(&self) -> bool {
        matches!(self.market_type, MarketType::Spot)
            && self.quote_asset == USD
            && asset_atoms(self.base_asset) == Some(self.quantity.scale())
            && self.price.decimals <= 6
            && u128::from(self.price.step)
                .checked_mul(u128::from(self.quantity.step))
                .and_then(|v| v.checked_mul(u128::from(USD_ATOMS)))
                .is_some_and(|minimum| {
                    minimum % (u128::from(self.price.scale()) * u128::from(self.quantity.scale()))
                        == 0
                })
    }

    /// USD micro-atoms for a base quantity expressed in the configured lot scale.
    /// Rejects non-integral values instead of rounding away a partial fill.
    pub fn quote_atoms(&self, price: u64, quantity: u64) -> Option<u64> {
        if !self.supports_current_spot_settlement() {
            return None;
        }
        let numerator = u128::from(price)
            .checked_mul(u128::from(quantity))?
            .checked_mul(u128::from(USD_ATOMS))?;
        let divisor = u128::from(self.price.scale()) * u128::from(self.quantity.scale());
        if numerator % divisor != 0 {
            return None;
        }
        u64::try_from(numerator / divisor).ok()
    }

    pub fn from_config(id: MarketId, config: MarketConfig) -> Self {
        Self {
            id,
            base_asset: config.base_asset,
            quote_asset: config.quote_asset,
            price: config.price,
            quantity: config.quantity,
            market_type: config.market_type,
            fees: config.fees,
            maintenance_margin_bps: config.maintenance_margin_bps,
            funding_interval_updates: config.funding_interval_updates,
            max_funding_bps: config.max_funding_bps,
            max_liquidation_slippage_bps: config.max_liquidation_slippage_bps,
        }
    }

    pub fn config(&self) -> MarketConfig {
        MarketConfig {
            base_asset: self.base_asset,
            quote_asset: self.quote_asset,
            price: self.price,
            quantity: self.quantity,
            market_type: self.market_type,
            fees: self.fees,
            maintenance_margin_bps: self.maintenance_margin_bps,
            funding_interval_updates: self.funding_interval_updates,
            max_funding_bps: self.max_funding_bps,
            max_liquidation_slippage_bps: self.max_liquidation_slippage_bps,
        }
    }
}
