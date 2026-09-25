pub mod account;
pub mod market;
pub mod order;
pub mod orderbook;

use crate::exchange::{
    account::{AssetId, Balance},
    market::{BTC, BTC_USD, Market, MarketId, USD},
    order::{Order, OrderId, OrderLocation, OrderType, Side},
    orderbook::OrderBook,
};
use crate::{
    blockchain::Transaction,
    exchange::account::{Account, AccountId},
};
use std::collections::HashMap;

const N_ACCOUNTS: u64 = 100;

pub enum ExchangeError {
    AccountNotFound,
    InvalidNonce,
    InsufficientBalance,
    MarketNotFound,
    OrderNotFound,
    NotImplemented,
    OrderBookNotFound,
}

#[derive(Debug)]
pub struct ExchangeState {
    pub accounts: HashMap<AccountId, Account>,
    pub markets: HashMap<MarketId, Market>,
    pub order_books: HashMap<MarketId, OrderBook>,
    pub order_locations: HashMap<OrderId, OrderLocation>,
    pub next_order_id: OrderId,    
}

pub fn create_dummies_and_set_balances(exchange: &mut ExchangeState, n_accounts: u64) {
    for account_id in 0..n_accounts {
        exchange.create_account(account_id);

        exchange.set_balance(account_id, USD, 1_000_000);
        exchange.set_balance(account_id, BTC, 1_000);
    }
}
impl Default for ExchangeState {
    fn default() -> Self {
        let mut exchange = ExchangeState {
            accounts: HashMap::new(),
            markets: HashMap::new(),
            order_books: HashMap::new(),
            order_locations: HashMap::new(),
            next_order_id: 0,
        };
        let btc_usd_market = Market {
            id: BTC_USD,
            base_asset: BTC,
            quote_asset: USD,
        };
        exchange.markets.insert(BTC_USD, btc_usd_market);
        exchange.order_books.insert(BTC_USD, OrderBook::new());
        
        create_dummies_and_set_balances(&mut exchange, N_ACCOUNTS);
        exchange
        
    }
}

impl ExchangeState {
    fn create_account(&mut self, account_id: AccountId) {
        self.accounts.insert(
            account_id,
            Account {
                id: account_id,
                nonce: 0,
                balances: HashMap::new(),
            },
        );
    }
    fn set_balance(&mut self, account_id: AccountId, asset_id: AssetId, amount: u64) {
        let account = self
            .accounts
            .get_mut(&account_id)
            .expect("Failed getting account");
        account.balances.insert(
            asset_id,
            Balance {
                available: amount,
                locked: 0,
            },
        );
    }

    fn execute_transfer(
        &mut self,
        from: AccountId,
        to: AccountId,
        asset: AssetId,
        amount: u64,
        nonce: u64,
    ) -> Result<(), ExchangeError> {
        //Validity checks without mut
        {
            let sender = self
                .accounts
                .get(&from)
                .ok_or(ExchangeError::AccountNotFound)?;
            if sender.nonce != nonce {
                return Err(ExchangeError::InvalidNonce);
            }
            let balance = sender
                .balances
                .get(&asset)
                .ok_or(ExchangeError::InsufficientBalance)?;
            if balance.available < amount {
                return Err(ExchangeError::InsufficientBalance);
            }
        }
        //transfer the funds after all the checks
        {
            let sender = self
                .accounts
                .get_mut(&from)
                .ok_or(ExchangeError::AccountNotFound)?;
            sender.balances.get_mut(&asset).unwrap().available -= amount;
            sender.nonce += 1;
        }
        let receiver = self
            .accounts
            .get_mut(&to)
            .ok_or(ExchangeError::AccountNotFound)?;

        let balance = receiver.balances.entry(asset).or_insert(Balance {
            available: 0,
            locked: 0,
        });
        balance.available += amount;

        Ok(())
    }

    fn handle_place_order(
        &mut self,
        account_id: AccountId,
        market_id: MarketId,
        order_type: OrderType,
        side: Side,
        price: u64,
        quantity: u64,
        nonce: u64,
    ) -> Result<(), ExchangeError> {
        //Preflight checks

        let account = self
            .accounts
            .get_mut(&account_id)
            .ok_or(ExchangeError::AccountNotFound)?;
        let market = self
            .markets
            .get(&market_id)
            .ok_or(ExchangeError::MarketNotFound)?;
        if nonce != account.nonce {
            return Err(ExchangeError::InvalidNonce);
        }
        let asset_id_needed = match side {
            Side::Buy => market.quote_asset,
            Side::Sell => market.base_asset,
        };
        let balance = account
            .balances
            .get(&asset_id_needed)
            .ok_or(ExchangeError::InsufficientBalance)?;
        if balance.available < price * quantity {
            return Err(ExchangeError::InsufficientBalance);
        }

        let order_id = self.next_order_id;
        let order = Order {
            id: order_id,
            account: account_id,
            market: market_id,
            side,
            price,
            original_quantity: quantity,
            remaining_quantity: quantity,
        };

        //Maybe remove the clone in the future, just for prints now
        let result = {
            let ob = self
                .order_books
                .get_mut(&market_id)
                .ok_or(ExchangeError::OrderBookNotFound)?;
            ob.apply_order(order.clone(), order_type)
        };
        match result {
            Ok(_) => {
                self.order_locations.insert(order_id, OrderLocation { side, price });
                self.next_order_id+=1;
                account.nonce +=1;
                
            }
            Err(error) => {
                eprintln!("Error applying order: {error:?} ORDER: {order:?}");
            }
        }

        Ok(())
    }

    fn handle_cancel_order(
        &mut self,
        account_id: AccountId,
        market_id: MarketId,
        order_id: OrderId,
        nonce: u64,
    ) -> Result<(), ExchangeError> {
        let account = self
            .accounts
            .get_mut(&account_id)
            .ok_or(ExchangeError::AccountNotFound)?;
        if nonce != account.nonce {
            return Err(ExchangeError::InvalidNonce);
        }

        if let Some(location) = self.order_locations.get(&order_id) {
            let ob = self.order_books.get_mut(&market_id).ok_or(ExchangeError::OrderBookNotFound)?;
            ob.cancel_order(order_id, location.side, location.price, account_id).map_err(|_| ExchangeError::OrderNotFound)?;
            account.nonce +=1;
        } else {
            return Err(ExchangeError::OrderNotFound)
        }
        self.order_locations.remove(&order_id);
        Ok(())
    }

    pub fn execute(&mut self, tx: &Transaction) -> Result<(), ExchangeError> {
        let tx_result = match tx {
            Transaction::Transfer {
                from,
                to,
                asset,
                amount,
                nonce,
            } => self.execute_transfer(*from, *to, *asset, *amount, *nonce),
            Transaction::PlaceOrder {
                account_id,
                market_id,
                order_type,
                side,
                price,
                quantity,
                nonce,
            } => self.handle_place_order(*account_id, *market_id, *order_type, *side, *price, *quantity, *nonce),
            Transaction::CancelOrder {
                account_id,
                market_id,
                order_id,
                nonce,
            } => self.handle_cancel_order(*account_id, *market_id, *order_id, *nonce),
        };
        tx_result
    }
}

