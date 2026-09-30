use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::exchange::{account::AccountId, market::MarketId};

pub type OrderId = u64;
#[derive(Debug, PartialEq, Deserialize, Serialize, Clone, Copy, TS)]
pub enum OrderType {
    IOC,
    FOK,
    LO,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, TS)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone)]
pub struct OrderLocation {
    pub market: MarketId,
    pub side: Side,
    pub price: u64,
}

#[derive(Debug, Clone)]
pub struct Order {
    pub id: OrderId,
    pub account: AccountId,
    pub market: MarketId,
    pub side: Side,
    pub price: u64,
    pub original_quantity: u64,
    pub remaining_quantity: u64,
}
