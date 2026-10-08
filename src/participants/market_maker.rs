use std::collections::HashSet;

use crate::{exchange::{address::Address, order::Side}, node::OpenOrderSnapshot};

/// Prices for one BTC per level. The feed adapter supplies prices in the
/// chain's integer USD ticks; strategy decisions do not depend on Tauri.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesiredBook {
    pub bids: Vec<u64>,
    pub asks: Vec<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteAction {
    Cancel { order_id: u64 },
    Place { side: Side, price: u64 },
}

/// Plan all currently actionable quote changes. Pending placements reserve
/// their prices; pending cancellations reserve their order IDs. A placement
/// cannot be cancelled until its real order ID appears in a node snapshot.
pub fn plan_actions(
    account_id: Address,
    open_orders: &[OpenOrderSnapshot],
    desired: &DesiredBook,
    active: bool,
    pending: &[QuoteAction],
) -> Vec<QuoteAction> {
    let pending_cancels: HashSet<_> = pending.iter().filter_map(|action| match action {
        QuoteAction::Cancel { order_id } => Some(*order_id),
        _ => None,
    }).collect();
    let own = open_orders
        .iter()
        .filter(|order| order.account_id == account_id && order.market_id == 0);
    let mut retained = HashSet::new();
    let mut actions = Vec::new();
    for order in own {
        if pending_cancels.contains(&order.order_id) { continue; }
        let wanted = active
            && order.remaining_quantity == 1
            && match order.side {
                Side::Buy => desired.bids.contains(&order.price),
                Side::Sell => desired.asks.contains(&order.price),
            };
        if !wanted || !retained.insert((order.side == Side::Buy, order.price)) {
            actions.push(QuoteAction::Cancel {
                order_id: order.order_id,
            });
        }
    }
    if !active {
        return actions;
    }
    for action in pending {
        if let QuoteAction::Place { side, price } = action {
            retained.insert((*side == Side::Buy, *price));
        }
    }
    for (&price, side) in desired
        .bids
        .iter()
        .map(|price| (price, Side::Buy))
        .chain(desired.asks.iter().map(|price| (price, Side::Sell)))
    {
        if price > 0 && retained.insert((side == Side::Buy, price)) {
            actions.push(QuoteAction::Place { side, price });
        }
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(id: u64, side: Side, price: u64) -> OpenOrderSnapshot {
        OpenOrderSnapshot {
            order_id: id,
            account_id: 90.into(),
            market_id: 0,
            side,
            price,
            remaining_quantity: 1,
        }
    }

    #[test]
    fn cancels_stale_quote_before_placing_new_one() {
        let desired = DesiredBook {
            bids: vec![100],
            asks: vec![102],
        };
        let orders = vec![order(7, Side::Buy, 99), order(8, Side::Sell, 102)];
        assert_eq!(
            plan_actions(90.into(), &orders, &desired, true, &[]),
            vec![QuoteAction::Cancel { order_id: 7 }, QuoteAction::Place { side: Side::Buy, price: 100 }]
        );
        assert_eq!(
            plan_actions(90.into(), &orders[1..], &desired, true, &[]),
            vec![QuoteAction::Place {
                side: Side::Buy,
                price: 100
            }]
        );
        assert_eq!(
            plan_actions(90.into(), &orders, &desired, false, &[]),
            vec![QuoteAction::Cancel { order_id: 7 }, QuoteAction::Cancel { order_id: 8 }]
        );
    }

    #[test]
    fn pending_quotes_do_not_get_duplicated() {
        let desired = DesiredBook { bids: vec![100, 99], asks: vec![102] };
        let pending = vec![QuoteAction::Place { side: Side::Buy, price: 100 }];
        assert_eq!(plan_actions(90.into(), &[], &desired, true, &pending), vec![
            QuoteAction::Place { side: Side::Buy, price: 99 },
            QuoteAction::Place { side: Side::Sell, price: 102 },
        ]);
    }
}
