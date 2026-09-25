use crate::exchange::account::AssetId;



pub type MarketId = u64;

pub const USD: AssetId = 0;
pub const BTC: AssetId = 1;

pub const BTC_USD : MarketId = 0;

#[derive(Debug, Clone)]
pub struct Market {
    pub id: MarketId,
    pub base_asset: AssetId,
    pub quote_asset: AssetId,
}

