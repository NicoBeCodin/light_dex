use serde::{Deserialize, Serialize};

use crate::exchange::{account::AccountId, market::MarketId};


pub type OrderId = u64;
#[derive(Debug, PartialEq, Deserialize, Serialize, Clone, Copy)]
pub enum OrderType {
    IOC,
    FOK,
    LO,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, )]
pub struct OrderLocation {
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
