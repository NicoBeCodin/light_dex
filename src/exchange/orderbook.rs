use im::{OrdMap, Vector};

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
    WorkLimit,
    WouldTakeLiquidity,
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
    pub asks: OrdMap<u64, Vector<Order>>,
    pub bids: OrdMap<u64, Vector<Order>>,
    // Rebuildable, non-consensus indexes. u128 keeps level aggregation exact
    // even when the public u64 snapshot quantity saturates.
    bid_totals: OrdMap<u64, u128>,
    ask_totals: OrdMap<u64, u128>,
    owner_prices: OrdMap<(AccountId, bool, u64), usize>,
}

impl OrderBook {
    pub fn new() -> Self {
        Self {
            asks: OrdMap::new(),
            bids: OrdMap::new(),
            bid_totals: OrdMap::new(),
            ask_totals: OrdMap::new(),
            owner_prices: OrdMap::new(),
        }
    }

    pub fn snapshot(&self, market_id: MarketId, depth: usize) -> OrderBookSnapshot {
        fn levels<'a>(
            entries: impl Iterator<Item = (&'a u64, &'a Vector<Order>)>,
            totals: &OrdMap<u64, u128>,
            depth: usize,
        ) -> Vec<BookLevel> {
            entries
                .take(depth)
                .map(|(&price, orders)| BookLevel {
                    price,
                    quantity: totals[&price].min(u128::from(u64::MAX)) as u64,
                    order_count: orders.len(),
                })
                .collect()
        }
        OrderBookSnapshot {
            market_id,
            bids: levels(self.bids.iter().rev(), &self.bid_totals, depth),
            asks: levels(self.asks.iter(), &self.ask_totals, depth),
        }
    }

    pub fn fillable(&self, order: &Order) -> bool {
        let mut remaining = order.remaining_quantity;
        let opposite: Box<dyn Iterator<Item = (&u64, &Vector<Order>)> + '_> = match order.side {
            Side::Buy => Box::new(self.asks.iter()),
            Side::Sell => Box::new(self.bids.iter().rev()),
        };
        for (&price, orders) in opposite {
            if (order.side == Side::Buy && price > order.price)
                || (order.side == Side::Sell && price < order.price)
            {
                break;
            }
            for maker in orders {
                remaining = remaining.saturating_sub(maker.remaining_quantity);
                if remaining == 0 {
                    return true;
                }
            }
        }
        false
    }

    fn would_self_trade(&self, order: &Order) -> bool {
        let (buy, low, high) = match order.side {
            Side::Buy => (false, 0, order.price),
            Side::Sell => (true, order.price, u64::MAX),
        };
        self.owner_prices
            .range((order.account, buy, low)..=(order.account, buy, high))
            .next()
            .is_some()
    }

    fn add_index(&mut self, order: &Order) {
        let totals = match order.side {
            Side::Buy => &mut self.bid_totals,
            Side::Sell => &mut self.ask_totals,
        };
        *totals.entry(order.price).or_default() += u128::from(order.remaining_quantity);
        *self
            .owner_prices
            .entry((order.account, order.side == Side::Buy, order.price))
            .or_default() += 1;
    }

    fn subtract_index(
        &mut self,
        account: AccountId,
        _id: OrderId,
        side: Side,
        price: u64,
        quantity: u64,
        full: bool,
    ) {
        let totals = match side {
            Side::Buy => &mut self.bid_totals,
            Side::Sell => &mut self.ask_totals,
        };
        let total = totals.get_mut(&price).expect("indexed level");
        *total -= u128::from(quantity);
        if *total == 0 {
            totals.remove(&price);
        }
        if full {
            let key = (account, side == Side::Buy, price);
            let count = self.owner_prices.get_mut(&key).expect("indexed owner");
            *count -= 1;
            if *count == 0 {
                self.owner_prices.remove(&key);
            }
        }
    }

    /// Liquidation consumes one maker at a time; do not allocate/recompute a
    /// complete sweep only to discard all but its first fill.
    pub(crate) fn preview_first_fill(&self, order: &Order) -> Result<Option<Fill>, OrderBookError> {
        if order.price == 0 || order.remaining_quantity == 0 {
            return Err(OrderBookError::InvalidOrder);
        }
        if self.would_self_trade(order) {
            return Err(OrderBookError::SelfTrade);
        }
        let best = match order.side {
            Side::Buy => self.asks.iter().next(),
            Side::Sell => self.bids.iter().next_back(),
        };
        let Some((&price, makers)) = best else {
            return Ok(None);
        };
        if (order.side == Side::Buy && price > order.price)
            || (order.side == Side::Sell && price < order.price)
        {
            return Ok(None);
        }
        let maker = makers.front().expect("nonempty level");
        let quantity = maker.remaining_quantity.min(order.remaining_quantity);
        Ok(Some(Fill {
            maker_order_id: maker.id,
            maker_account: maker.account,
            maker_filled: quantity == maker.remaining_quantity,
            price,
            quantity,
        }))
    }

    /// Determine fills without changing the book. Exchange settlement uses
    /// this to validate balances before applying the book mutation.
    pub fn preview_order(
        &self,
        order: &Order,
        order_type: OrderType,
    ) -> Result<MatchResult, OrderBookError> {
        if order.price == 0 || order.remaining_quantity == 0 {
            return Err(OrderBookError::InvalidOrder);
        }
        if order_type == OrderType::PostOnly {
            let crosses = match order.side {
                Side::Buy => self
                    .asks
                    .keys()
                    .next()
                    .is_some_and(|price| *price <= order.price),
                Side::Sell => self
                    .bids
                    .keys()
                    .next_back()
                    .is_some_and(|price| *price >= order.price),
            };
            if crosses {
                return Err(OrderBookError::WouldTakeLiquidity);
            }
            return Ok(MatchResult {
                fills: Vec::new(),
                remaining: order.remaining_quantity,
            });
        }
        if self.would_self_trade(order) {
            return Err(OrderBookError::SelfTrade);
        }
        let mut remaining = order.remaining_quantity;
        let mut fills = Vec::new();
        let levels: Box<dyn Iterator<Item = (&u64, &Vector<Order>)> + '_> = match order.side {
            Side::Buy => Box::new(self.asks.iter()),
            Side::Sell => Box::new(self.bids.iter().rev()),
        };
        for (&price, makers) in levels {
            if (order.side == Side::Buy && price > order.price)
                || (order.side == Side::Sell && price < order.price)
            {
                break;
            }
            for maker in makers {
                if fills.len() >= crate::exchange::limits::MAX_FILLS_PER_ORDER {
                    return Err(OrderBookError::WorkLimit);
                }
                let quantity = maker.remaining_quantity.min(remaining);
                if quantity == 0 {
                    break;
                }
                fills.push(Fill {
                    maker_order_id: maker.id,
                    maker_account: maker.account,
                    maker_filled: quantity == maker.remaining_quantity,
                    price,
                    quantity,
                });
                remaining -= quantity;
                if remaining == 0 {
                    break;
                }
            }
            if remaining == 0 {
                break;
            }
        }
        if order_type == OrderType::FOK && remaining > 0 {
            return Err(OrderBookError::KilledFOK);
        }
        Ok(MatchResult { fills, remaining })
    }

    /// Commit a plan produced by `preview_order` against this unchanged book.
    /// Callers must finish all fallible settlement before calling this method.
    /// Each planned maker is at the front of its price level, so applying the
    /// plan touches only matched orders rather than searching the book again.
    pub(crate) fn apply_previewed_order(
        &mut self,
        mut order: Order,
        order_type: OrderType,
        plan: &MatchResult,
    ) {
        debug_assert_eq!(
            order.remaining_quantity,
            plan.remaining + plan.fills.iter().map(|fill| fill.quantity).sum::<u64>()
        );
        debug_assert!(order_type != OrderType::FOK || plan.remaining == 0);

        for fill in &plan.fills {
            let levels = match order.side {
                Side::Buy => &mut self.asks,
                Side::Sell => &mut self.bids,
            };
            let makers = levels.get_mut(&fill.price).expect("previewed level exists");
            let maker = makers.front_mut().expect("previewed maker exists");
            assert_eq!(maker.id, fill.maker_order_id, "book changed after preview");
            assert!(
                maker.remaining_quantity >= fill.quantity,
                "book changed after preview"
            );
            maker.remaining_quantity -= fill.quantity;
            assert_eq!(maker.remaining_quantity == 0, fill.maker_filled);
            if fill.maker_filled {
                makers.pop_front();
            }
            if makers.is_empty() {
                levels.remove(&fill.price);
            }
            self.subtract_index(
                fill.maker_account,
                fill.maker_order_id,
                match order.side {
                    Side::Buy => Side::Sell,
                    Side::Sell => Side::Buy,
                },
                fill.price,
                fill.quantity,
                fill.maker_filled,
            );
        }

        order.remaining_quantity = plan.remaining;
        if plan.remaining > 0 && order_type.rests() {
            self.add_index(&order);
            let levels = match order.side {
                Side::Buy => &mut self.bids,
                Side::Sell => &mut self.asks,
            };
            levels.entry(order.price).or_default().push_back(order);
        }
    }

    pub fn apply_order(
        &mut self,
        order: Order,
        order_type: OrderType,
    ) -> Result<MatchResult, OrderBookError> {
        let plan = self.preview_order(&order, order_type)?;
        self.apply_previewed_order(order, order_type, &plan);
        Ok(plan)
    }

    pub fn cancel_order(
        &mut self,
        order_id: OrderId,
        side: Side,
        price: u64,
        account_id: AccountId,
    ) -> Result<Order, OrderBookError> {
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        let orders = levels
            .get_mut(&price)
            .ok_or(OrderBookError::InexistentOrder)?;
        let index = orders
            .binary_search_by_key(&order_id, |order| order.id)
            .map_err(|_| OrderBookError::InexistentOrder)?;
        if orders[index].account != account_id {
            return Err(OrderBookError::InexistentOrder);
        }
        let order = orders.remove(index);
        if orders.is_empty() {
            levels.remove(&price);
        }
        self.subtract_index(
            account_id,
            order_id,
            side,
            price,
            order.remaining_quantity,
            true,
        );
        Ok(order)
    }

    pub fn find_order(
        &self,
        order_id: OrderId,
        side: Side,
        price: u64,
        account_id: AccountId,
    ) -> Option<&Order> {
        let levels = match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        };
        let order_level = levels.get(&price)?;
        order_level
            .binary_search_by_key(&order_id, |order| order.id)
            .ok()
            .and_then(|index| order_level.get(index))
            .filter(|order| order.account == account_id)
    }

    /// Strict decrease of remaining size, without changing ID or FIFO rank.
    /// The exchange must validate collateral/accounting before calling this.
    pub(crate) fn reduce_order(
        &mut self,
        id: OrderId,
        side: Side,
        price: u64,
        owner: AccountId,
        remaining: u64,
    ) -> Result<(), OrderBookError> {
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        let orders = levels
            .get_mut(&price)
            .ok_or(OrderBookError::InexistentOrder)?;
        let index = orders
            .binary_search_by_key(&id, |order| order.id)
            .map_err(|_| OrderBookError::InexistentOrder)?;
        let order = &mut orders[index];
        if order.account != owner {
            return Err(OrderBookError::InexistentOrder);
        }
        if remaining == 0 || remaining >= order.remaining_quantity {
            return Err(OrderBookError::InvalidOrder);
        }
        let reduction = order.remaining_quantity - remaining;
        order.remaining_quantity = remaining;
        order.original_quantity -= reduction;
        self.subtract_index(owner, id, side, price, reduction, false);
        Ok(())
    }
}

#[cfg(test)]
mod index_tests {
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    fn order(id: u64, owner: u64, side: Side, price: u64, quantity: u64) -> Order {
        Order {
            id,
            account: owner.into(),
            market: 0,
            side,
            price,
            original_quantity: quantity,
            remaining_quantity: quantity,
        }
    }

    fn check(book: &OrderBook) {
        let mut totals = (OrdMap::new(), OrdMap::new());
        let mut owners = OrdMap::new();
        for (buy, levels) in [(true, &book.bids), (false, &book.asks)] {
            for (&price, queue) in levels {
                assert!(!queue.is_empty());
                for order in queue {
                    let total = if buy { &mut totals.0 } else { &mut totals.1 };
                    *total.entry(price).or_insert(0u128) += u128::from(order.remaining_quantity);
                    *owners.entry((order.account, buy, price)).or_insert(0usize) += 1;
                }
            }
        }
        assert_eq!(book.bid_totals, totals.0);
        assert_eq!(book.ask_totals, totals.1);
        assert_eq!(book.owner_prices, owners);
    }

    #[test]
    fn cached_indexes_match_full_scan_after_churn_and_forks() {
        let mut rng = StdRng::seed_from_u64(831);
        let mut book = OrderBook::new();
        for id in 0..1500 {
            if rng.random_ratio(1, 4) {
                let candidate = book
                    .asks
                    .values()
                    .chain(book.bids.values())
                    .flat_map(|level| level.iter())
                    .next()
                    .cloned();
                if let Some(o) = candidate {
                    book.cancel_order(o.id, o.side, o.price, o.account).unwrap();
                }
            } else {
                let side = if rng.random_bool(0.5) {
                    Side::Buy
                } else {
                    Side::Sell
                };
                let before = book.clone();
                let result = book.apply_order(
                    order(
                        id,
                        rng.random_range(0..20),
                        side,
                        rng.random_range(90..110),
                        rng.random_range(1..20),
                    ),
                    if id % 3 == 0 {
                        OrderType::IOC
                    } else {
                        OrderType::LO
                    },
                );
                check(&before);
                if result.is_err() {
                    assert_eq!(book.owner_prices, before.owner_prices);
                }
            }
            check(&book);
        }
    }

    #[test]
    fn indexed_self_trade_keeps_crossable_own_order_rule() {
        let mut book = OrderBook::new();
        book.apply_order(order(0, 1, Side::Sell, 100, 5), OrderType::LO)
            .unwrap();
        book.apply_order(order(1, 2, Side::Sell, 101, 5), OrderType::LO)
            .unwrap();
        // Even though the first maker alone could fill this taker, the old
        // semantics reject any crossing own order. The index preserves that.
        assert_eq!(
            book.preview_order(&order(2, 2, Side::Buy, 101, 1), OrderType::IOC)
                .unwrap_err(),
            OrderBookError::SelfTrade
        );
        book.cancel_order(1, Side::Sell, 101, 2.into()).unwrap();
        assert_eq!(
            book.preview_first_fill(&order(2, 2, Side::Buy, 101, 1))
                .unwrap()
                .unwrap()
                .maker_order_id,
            0
        );
        check(&book);
    }
}
