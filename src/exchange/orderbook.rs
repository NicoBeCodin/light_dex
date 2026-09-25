use std::collections::{BTreeMap, VecDeque};

use crate::exchange::account::AccountId;
use crate::exchange::order::{Order, OrderId, OrderType};
use crate::exchange::{ExchangeError, Side};

//In the future we could store something smaller than whole Order struct
#[derive(Debug)]
pub struct OrderBook {
    pub asks: BTreeMap<u64, VecDeque<Order>>,
    pub bids: BTreeMap<u64, VecDeque<Order>>,
}

#[derive(Debug)]
pub enum OrderBookError {
    InvalidOrder,
    InvalidLimitOrderPrice,
    KilledFOK,
    InexistentOrder,
}

impl OrderBook {
    pub fn new() -> Self {
        OrderBook {
            asks: BTreeMap::new(),
            bids: BTreeMap::new(),
        }
    }

    //For fill or kill
    pub fn fillable(&self, order: &Order) -> bool {
        let mut remaining = order.remaining_quantity;
        match order.side {
            Side::Buy => {
                for (&price, asks) in self.asks.iter() {
                    if price > order.price {
                        break;
                    }
                    for ask in asks {
                        if ask.remaining_quantity >= remaining {
                            return true;
                        } else {
                            remaining -= ask.remaining_quantity;
                        }
                    }
                }
                false
            }
            Side::Sell => {
                for (&price, bids) in self.asks.iter().rev() {
                    if price < order.price {
                        break;
                    }
                    for bid in bids {
                        if bid.remaining_quantity >= remaining {
                            return true;
                        } else {
                            remaining -= bid.remaining_quantity;
                        }
                    }
                }
                false
            }
        }
    }
    pub fn cancel_order(&mut self, order_id: OrderId, side: Side, price: u64, account_id: AccountId)->Result<(), OrderBookError>{
        match side {
            Side::Buy => {
                let Some(bids) = self.asks.get_mut(&price)  else {
                    return Err(OrderBookError::InexistentOrder);
                    
                };
                if let Some(index) = bids.iter().position(|bid| order_id == bid.id && bid.account == account_id) {
                    
                    let _ = bids.remove(index);
                    return Ok(())
                } else {
                    return Err(OrderBookError::InexistentOrder);
                }
                
                
                
            },
            Side::Sell => {
                
                let Some(asks) = self.asks.get_mut(&price)  else {
                    return Err(OrderBookError::InexistentOrder);
                    
                };
                if let Some(index) = asks.iter().position(|ask| order_id == ask.id && ask.id == account_id) {
                    let _ = asks.remove(index);
                    return Ok(())
                } else {
                    return Err(OrderBookError::InexistentOrder);
                }
            }
            
        }
    }

    pub fn match_order(&mut self, order: Order, order_type: OrderType) {
        let mut matched_order = order;
        match matched_order.side {
            Side::Buy => {
                loop {
                    if matched_order.remaining_quantity == 0 {
                        break;
                    }
                    let Some((&best_ask, _)) = self.asks.first_key_value() else {
                        break;
                    };
                    if best_ask > matched_order.price {
                        break;
                    }

                    let mut remove_level = false;
                    {
                        let orders = self.asks.get_mut(&best_ask).unwrap();
                        let ask_order = orders.front_mut().unwrap();

                        if matched_order.remaining_quantity >= ask_order.remaining_quantity {
                            matched_order.remaining_quantity -= ask_order.remaining_quantity;
                            orders.pop_front();
                        } else {
                            ask_order.remaining_quantity -= matched_order.remaining_quantity;
                            matched_order.remaining_quantity = 0;
                        }
                        if orders.is_empty() {
                            remove_level = true;
                        }
                    }
                    if remove_level {
                        self.asks.remove(&best_ask);
                    }
                }
                if matched_order.remaining_quantity > 0 && order_type != OrderType::IOC {
                    self.asks
                        .entry(matched_order.price)
                        .or_default()
                        .push_back(matched_order);
                }
            }
            Side::Sell => {
                loop {
                    if matched_order.remaining_quantity == 0 {
                        break;
                    }
                    let Some((&best_bid, _)) = self.asks.last_key_value() else {
                        break;
                    };
                    if best_bid < matched_order.price {
                        break;
                    }
                    let mut remove_level = false;
                    {
                        let orders = self.asks.get_mut(&best_bid).unwrap();
                        let bid_order = orders.front_mut().unwrap();

                        if matched_order.remaining_quantity >= bid_order.remaining_quantity {
                            matched_order.remaining_quantity -= bid_order.remaining_quantity;
                            orders.pop_front();
                        } else {
                            bid_order.remaining_quantity -= matched_order.remaining_quantity;
                            matched_order.remaining_quantity = 0;
                        }
                        if orders.is_empty() {
                            remove_level = true;
                        }
                    }
                    if remove_level {
                        self.asks.remove(&best_bid);
                    }
                }
                if matched_order.remaining_quantity > 0 && order_type != OrderType::IOC {
                    self.asks
                        .entry(matched_order.price)
                        .or_default()
                        .push_back(matched_order);
                }
            }
        };
    }

    pub fn apply_order(&mut self, order: Order, order_type: OrderType) -> Result<(), OrderBookError> {
        match order.side {
            Side::Buy => {
                if let Some((&best_ask, _)) = self.asks.first_key_value() {
                    if order.price >= best_ask {
                        if order_type == OrderType::FOK && !self.fillable(&order){
                            return Err(OrderBookError::KilledFOK);
                        }
                        self.match_order(order, order_type);
                    } else {
                        self.asks.entry(order.price).or_default().push_back(order);
                    }
                }
            }
            Side::Sell => {
                if let Some((&best_bid, _)) = self.asks.last_key_value() {
                    if order.price <= best_bid {
                        if order_type == OrderType::FOK && !self.fillable(&order){
                            return Err(OrderBookError::KilledFOK);
                        }
                        self.match_order(order, order_type);
                    } else {
                        self.asks.entry(order.price).or_default().push_back(order);
                    }
                }
            }
        };

        Ok(())
    }
}
