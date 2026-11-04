use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::exchange::{account::AccountId, market::MarketId};

pub type OrderId = u64;
#[derive(Debug, PartialEq, Eq, Hash, Deserialize, Serialize, Clone, Copy, TS)]
pub enum OrderType {
    IOC,
    FOK,
    LO,
    /// Resting limit order; reject rather than take liquidity at execution time.
    PostOnly,
}

impl OrderType {
    pub fn rests(self) -> bool {
        matches!(self, Self::LO | Self::PostOnly)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, TS)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OrderLocation {
    pub market: MarketId,
    pub side: Side,
    pub price: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Order {
    pub id: OrderId,
    pub account: AccountId,
    pub market: MarketId,
    pub side: Side,
    pub price: u64,
    pub original_quantity: u64,
    pub remaining_quantity: u64,
}
