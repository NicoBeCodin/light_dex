use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::exchange::{
    account::AccountId,
    market::MarketId,
    order::{Order, OrderId, OrderType, Side},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderBookError {
    InvalidOrder,
    KilledFOK,
    InexistentOrder,
    SelfTrade,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fill {
    pub maker_order_id: OrderId,
    pub maker_account: AccountId,
    pub maker_filled: bool,
    pub price: u64,
    pub quantity: u64,
}

#[derive(Debug, Clone)]
pub struct MatchResult {
    pub fills: Vec<Fill>,
    pub remaining: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct BookLevel {
    pub price: u64,
    pub quantity: u64,
    pub order_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct OrderBookSnapshot {
    pub market_id: MarketId,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
}

#[derive(Debug, Clone)]
pub struct OrderBook {
    pub asks: BTreeMap<u64, VecDeque<Order>>,
    pub bids: BTreeMap<u64, VecDeque<Order>>,
}

impl OrderBook {
    pub fn new() -> Self {
        Self { asks: BTreeMap::new(), bids: BTreeMap::new() }
    }

    pub fn snapshot(&self, market_id: MarketId, depth: usize) -> OrderBookSnapshot {
        fn levels<'a>(entries: impl Iterator<Item = (&'a u64, &'a VecDeque<Order>)>, depth: usize) -> Vec<BookLevel> {
            entries.take(depth).map(|(&price, orders)| BookLevel {
                price,
                quantity: orders.iter().fold(0u64, |total, order| total.saturating_add(order.remaining_quantity)),
                order_count: orders.len(),
            }).collect()
        }
        OrderBookSnapshot {
            market_id,
            bids: levels(self.bids.iter().rev(), depth),
            asks: levels(self.asks.iter(), depth),
        }
    }

    pub fn fillable(&self, order: &Order) -> bool {
        let mut remaining = order.remaining_quantity;
        let opposite: Box<dyn Iterator<Item = (&u64, &VecDeque<Order>)> + '_> = match order.side {
            Side::Buy => Box::new(self.asks.iter()),
            Side::Sell => Box::new(self.bids.iter().rev()),
        };
        for (&price, orders) in opposite {
            if (order.side == Side::Buy && price > order.price)
                || (order.side == Side::Sell && price < order.price) { break; }
            for maker in orders {
                remaining = remaining.saturating_sub(maker.remaining_quantity);
                if remaining == 0 { return true; }
            }
        }
        false
    }

    fn would_self_trade(&self, order: &Order) -> bool {
        let opposite: Box<dyn Iterator<Item = (&u64, &VecDeque<Order>)> + '_> = match order.side {
            Side::Buy => Box::new(self.asks.iter()),
            Side::Sell => Box::new(self.bids.iter().rev()),
        };
        for (&price, orders) in opposite {
            if (order.side == Side::Buy && price > order.price)
                || (order.side == Side::Sell && price < order.price) { break; }
            if orders.iter().any(|maker| maker.account == order.account) { return true; }
        }
        false
    }

    /// Determine fills without changing the book. Exchange settlement uses
    /// this to validate balances before applying the book mutation.
    pub fn preview_order(&self, order: &Order, order_type: OrderType) -> Result<MatchResult, OrderBookError> {
        if order.price == 0 || order.remaining_quantity == 0 { return Err(OrderBookError::InvalidOrder); }
        if self.would_self_trade(order) { return Err(OrderBookError::SelfTrade); }
        let mut remaining = order.remaining_quantity;
        let mut fills = Vec::new();
        let levels: Box<dyn Iterator<Item = (&u64, &VecDeque<Order>)> + '_> = match order.side {
            Side::Buy => Box::new(self.asks.iter()),
            Side::Sell => Box::new(self.bids.iter().rev()),
        };
        for (&price, makers) in levels {
            if (order.side == Side::Buy && price > order.price)
                || (order.side == Side::Sell && price < order.price) { break; }
            for maker in makers {
                let quantity = maker.remaining_quantity.min(remaining);
                if quantity == 0 { break; }
                fills.push(Fill {
                    maker_order_id: maker.id,
                    maker_account: maker.account,
                    maker_filled: quantity == maker.remaining_quantity,
                    price,
                    quantity,
                });
                remaining -= quantity;
                if remaining == 0 { break; }
            }
            if remaining == 0 { break; }
        }
        if order_type == OrderType::FOK && remaining > 0 { return Err(OrderBookError::KilledFOK); }
        Ok(MatchResult { fills, remaining })
    }

    pub fn apply_order(&mut self, mut order: Order, order_type: OrderType) -> Result<MatchResult, OrderBookError> {
        if order.price == 0 || order.remaining_quantity == 0 { return Err(OrderBookError::InvalidOrder); }
        if self.would_self_trade(&order) { return Err(OrderBookError::SelfTrade); }
        if order_type == OrderType::FOK && !self.fillable(&order) { return Err(OrderBookError::KilledFOK); }

        let mut fills = Vec::new();
        while order.remaining_quantity > 0 {
            let price = match order.side {
                Side::Buy => self.asks.first_key_value().map(|(&price, _)| price),
                Side::Sell => self.bids.last_key_value().map(|(&price, _)| price),
            };
            let Some(price) = price else { break; };
            if (order.side == Side::Buy && price > order.price)
                || (order.side == Side::Sell && price < order.price) { break; }

            let levels = match order.side { Side::Buy => &mut self.asks, Side::Sell => &mut self.bids };
            let makers = levels.get_mut(&price).expect("best level exists");
            let maker = makers.front_mut().expect("nonempty level");
            let quantity = maker.remaining_quantity.min(order.remaining_quantity);
            maker.remaining_quantity -= quantity;
            order.remaining_quantity -= quantity;
            let maker_filled = maker.remaining_quantity == 0;
            fills.push(Fill { maker_order_id: maker.id, maker_account: maker.account, maker_filled, price, quantity });
            if maker_filled { makers.pop_front(); }
            if makers.is_empty() { levels.remove(&price); }
        }

        let remaining = order.remaining_quantity;
        if remaining > 0 && order_type == OrderType::LO {
            let levels = match order.side { Side::Buy => &mut self.bids, Side::Sell => &mut self.asks };
            levels.entry(order.price).or_default().push_back(order);
        }
        Ok(MatchResult { fills, remaining })
    }

    pub fn cancel_order(&mut self, order_id: OrderId, side: Side, price: u64, account_id: AccountId) -> Result<Order, OrderBookError> {
        let levels = match side { Side::Buy => &mut self.bids, Side::Sell => &mut self.asks };
        let orders = levels.get_mut(&price).ok_or(OrderBookError::InexistentOrder)?;
        let index = orders.binary_search_by_key(&order_id, |order| order.id).map_err(|_| OrderBookError::InexistentOrder)?;
        if orders[index].account != account_id{
            return Err(OrderBookError::InexistentOrder);
        }
        let order = orders.remove(index).expect("order index exists");
        if orders.is_empty() { levels.remove(&price); }
        Ok(order)
    }

    pub fn find_order(&self, order_id: OrderId, side: Side, price: u64, account_id: AccountId) -> Option<&Order> {
        let levels = match side { Side::Buy => &self.bids, Side::Sell => &self.asks };
        let order_level = levels.get(&price)?;
        order_level.binary_search_by_key(&order_id, |order| order.id)
            .ok()
            .and_then(|index| order_level.get(index))
            .filter(|order| order.account == account_id)
    }
}
