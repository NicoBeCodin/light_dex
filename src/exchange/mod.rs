pub mod account;
pub mod market;
pub mod order;
pub mod orderbook;

use std::collections::HashMap;

use sha2::{Digest as _, Sha256};

use crate::blockchain::Transaction;
use account::{Account, AccountId, AssetId, Balance};
use market::{Market, MarketId, BTC, BTC_USD, USD};
use order::{Order, OrderId, OrderLocation, OrderType, Side};
use orderbook::{OrderBook, OrderBookError};

const N_ACCOUNTS: u64 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExchangeError {
    AccountNotFound,
    InvalidNonce,
    InsufficientBalance,
    MarketNotFound,
    OrderNotFound,
    OrderBookNotFound,
    InvalidOrder,
    SelfTrade,
    FokUnfilled,
    ArithmeticOverflow,
}

#[derive(Debug, Clone)]
pub struct ExchangeState {
    pub accounts: HashMap<AccountId, Account>,
    pub markets: HashMap<MarketId, Market>,
    pub order_books: HashMap<MarketId, OrderBook>,
    pub order_locations: HashMap<OrderId, OrderLocation>,
    pub next_order_id: OrderId,
}

impl Default for ExchangeState {
    fn default() -> Self {
        let mut exchange = Self {
            accounts: HashMap::new(), markets: HashMap::new(), order_books: HashMap::new(),
            order_locations: HashMap::new(), next_order_id: 0,
        };
        exchange.markets.insert(BTC_USD, Market { id: BTC_USD, base_asset: BTC, quote_asset: USD });
        exchange.order_books.insert(BTC_USD, OrderBook::new());
        for account_id in 0..N_ACCOUNTS {
            exchange.accounts.insert(account_id, Account {
                id: account_id, nonce: 0,
                balances: HashMap::from([
                    (USD, Balance { available: 1_000_000, locked: 0 }),
                    (BTC, Balance { available: 1_000, locked: 0 }),
                ]),
            });
        }
        exchange
    }
}

impl ExchangeState {
    fn balance_mut(&mut self, account: AccountId, asset: AssetId) -> Result<&mut Balance, ExchangeError> {
        Ok(self.accounts.get_mut(&account).ok_or(ExchangeError::AccountNotFound)?
            .balances.entry(asset).or_insert(Balance { available: 0, locked: 0 }))
    }

    fn debit_available(&mut self, account: AccountId, asset: AssetId, amount: u64) -> Result<(), ExchangeError> {
        let balance = self.balance_mut(account, asset)?;
        balance.available = balance.available.checked_sub(amount).ok_or(ExchangeError::InsufficientBalance)?;
        Ok(())
    }

    fn credit_available(&mut self, account: AccountId, asset: AssetId, amount: u64) -> Result<(), ExchangeError> {
        let balance = self.balance_mut(account, asset)?;
        balance.available = balance.available.checked_add(amount).ok_or(ExchangeError::ArithmeticOverflow)?;
        Ok(())
    }

    fn lock(&mut self, account: AccountId, asset: AssetId, amount: u64) -> Result<(), ExchangeError> {
        self.debit_available(account, asset, amount)?;
        let balance = self.balance_mut(account, asset)?;
        balance.locked = balance.locked.checked_add(amount).ok_or(ExchangeError::ArithmeticOverflow)?;
        Ok(())
    }

    fn spend_locked(&mut self, account: AccountId, asset: AssetId, amount: u64) -> Result<(), ExchangeError> {
        let balance = self.balance_mut(account, asset)?;
        balance.locked = balance.locked.checked_sub(amount).ok_or(ExchangeError::InsufficientBalance)?;
        Ok(())
    }

    fn unlock(&mut self, account: AccountId, asset: AssetId, amount: u64) -> Result<(), ExchangeError> {
        self.spend_locked(account, asset, amount)?;
        self.credit_available(account, asset, amount)
    }

    fn check_nonce(&self, account: AccountId, nonce: u64) -> Result<(), ExchangeError> {
        let current = self.accounts.get(&account).ok_or(ExchangeError::AccountNotFound)?.nonce;
        if current != nonce { return Err(ExchangeError::InvalidNonce); }
        current.checked_add(1).ok_or(ExchangeError::ArithmeticOverflow)?;
        Ok(())
    }

    fn advance_nonce(&mut self, account: AccountId) {
        self.accounts.get_mut(&account).expect("validated account").nonce += 1;
    }

    fn transfer(&mut self, from: AccountId, to: AccountId, asset: AssetId, amount: u64, nonce: u64) -> Result<(), ExchangeError> {
        self.check_nonce(from, nonce)?;
        if !self.accounts.contains_key(&to) { return Err(ExchangeError::AccountNotFound); }
        if amount == 0 { return Err(ExchangeError::InvalidOrder); }
        self.debit_available(from, asset, amount)?;
        self.credit_available(to, asset, amount)?;
        self.advance_nonce(from);
        Ok(())
    }

    fn place_order(&mut self, account_id: AccountId, market_id: MarketId, order_type: OrderType,
        side: Side, price: u64, quantity: u64, nonce: u64) -> Result<(), ExchangeError> {
        self.check_nonce(account_id, nonce)?;
        if price == 0 || quantity == 0 { return Err(ExchangeError::InvalidOrder); }
        let market = self.markets.get(&market_id).ok_or(ExchangeError::MarketNotFound)?.clone();
        let reserve = match side {
            Side::Buy => price.checked_mul(quantity).ok_or(ExchangeError::ArithmeticOverflow)?,
            Side::Sell => quantity,
        };
        let reserve_asset = match side { Side::Buy => market.quote_asset, Side::Sell => market.base_asset };
        self.lock(account_id, reserve_asset, reserve)?;
        let order_id = self.next_order_id;
        self.next_order_id = self.next_order_id.checked_add(1).ok_or(ExchangeError::ArithmeticOverflow)?;
        let order = Order {
            id: order_id, account: account_id, market: market_id, side, price,
            original_quantity: quantity, remaining_quantity: quantity,
        };
        let result = self.order_books.get_mut(&market_id).ok_or(ExchangeError::OrderBookNotFound)?
            .apply_order(order, order_type).map_err(|error| match error {
                OrderBookError::KilledFOK => ExchangeError::FokUnfilled,
                OrderBookError::SelfTrade => ExchangeError::SelfTrade,
                _ => ExchangeError::InvalidOrder,
            })?;

        for fill in result.fills {
            let trade_value = fill.price.checked_mul(fill.quantity).ok_or(ExchangeError::ArithmeticOverflow)?;
            match side {
                Side::Buy => {
                    let reserved = price.checked_mul(fill.quantity).ok_or(ExchangeError::ArithmeticOverflow)?;
                    self.spend_locked(account_id, market.quote_asset, reserved)?;
                    self.credit_available(account_id, market.quote_asset, reserved - trade_value)?;
                    self.credit_available(account_id, market.base_asset, fill.quantity)?;
                    self.spend_locked(fill.maker_account, market.base_asset, fill.quantity)?;
                    self.credit_available(fill.maker_account, market.quote_asset, trade_value)?;
                }
                Side::Sell => {
                    self.spend_locked(account_id, market.base_asset, fill.quantity)?;
                    self.credit_available(account_id, market.quote_asset, trade_value)?;
                    self.spend_locked(fill.maker_account, market.quote_asset, trade_value)?;
                    self.credit_available(fill.maker_account, market.base_asset, fill.quantity)?;
                }
            }
            if fill.maker_filled { self.order_locations.remove(&fill.maker_order_id); }
        }
        if result.remaining > 0 {
            if order_type == OrderType::LO {
                self.order_locations.insert(order_id, OrderLocation { market: market_id, side, price });
            } else {
                let release = match side {
                    Side::Buy => price.checked_mul(result.remaining).ok_or(ExchangeError::ArithmeticOverflow)?,
                    Side::Sell => result.remaining,
                };
                self.unlock(account_id, reserve_asset, release)?;
            }
        }
        self.advance_nonce(account_id);
        Ok(())
    }

    fn cancel_order(&mut self, account_id: AccountId, market_id: MarketId, order_id: OrderId, nonce: u64) -> Result<(), ExchangeError> {
        self.check_nonce(account_id, nonce)?;
        let location = self.order_locations.get(&order_id).ok_or(ExchangeError::OrderNotFound)?.clone();
        if location.market != market_id { return Err(ExchangeError::OrderNotFound); }
        let market = self.markets.get(&market_id).ok_or(ExchangeError::MarketNotFound)?.clone();
        let order = self.order_books.get_mut(&market_id).ok_or(ExchangeError::OrderBookNotFound)?
            .cancel_order(order_id, location.side, location.price, account_id)
            .map_err(|_| ExchangeError::OrderNotFound)?;
        let (asset, amount) = match order.side {
            Side::Buy => (market.quote_asset, order.price.checked_mul(order.remaining_quantity).ok_or(ExchangeError::ArithmeticOverflow)?),
            Side::Sell => (market.base_asset, order.remaining_quantity),
        };
        self.unlock(account_id, asset, amount)?;
        self.order_locations.remove(&order_id);
        self.advance_nonce(account_id);
        Ok(())
    }

    /// Apply a transaction atomically. Any failure leaves balances, orders and nonce unchanged.
    pub fn execute(&mut self, tx: &Transaction) -> Result<(), ExchangeError> {
        let mut staged = self.clone();
        match tx {
            Transaction::Transfer { from, to, asset, amount, nonce } => staged.transfer(*from, *to, *asset, *amount, *nonce)?,
            Transaction::PlaceOrder { account_id, market_id, order_type, side, price, quantity, nonce } =>
                staged.place_order(*account_id, *market_id, *order_type, *side, *price, *quantity, *nonce)?,
            Transaction::CancelOrder { account_id, market_id, order_id, nonce } =>
                staged.cancel_order(*account_id, *market_id, *order_id, *nonce)?,
        }
        *self = staged;
        Ok(())
    }

    pub fn get_nonce_for_account(&self, account_id: AccountId) -> Result<u64, ExchangeError> {
        Ok(self.accounts.get(&account_id).ok_or(ExchangeError::AccountNotFound)?.nonce)
    }

    /// Canonical digest of the execution state, independent of HashMap iteration order.
    pub fn state_digest(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(self.next_order_id.to_le_bytes());
        let mut accounts = self.accounts.iter().collect::<Vec<_>>();
        accounts.sort_by_key(|(id, _)| **id);
        for (&id, account) in accounts {
            hash.update(id.to_le_bytes()); hash.update(account.nonce.to_le_bytes());
            let mut balances = account.balances.iter().collect::<Vec<_>>();
            balances.sort_by_key(|(asset, _)| **asset);
            for (&asset, balance) in balances {
                hash.update(asset.to_le_bytes()); hash.update(balance.available.to_le_bytes()); hash.update(balance.locked.to_le_bytes());
            }
        }
        let mut books = self.order_books.iter().collect::<Vec<_>>();
        books.sort_by_key(|(market, _)| **market);
        for (&market_id, book) in books {
            hash.update(market_id.to_le_bytes());
            for (side, levels) in [(Side::Buy, &book.bids), (Side::Sell, &book.asks)] {
                hash.update([if side == Side::Buy { 0 } else { 1 }]);
                for (&price, orders) in levels {
                    hash.update(price.to_le_bytes());
                    for order in orders {
                        hash.update(order.id.to_le_bytes()); hash.update(order.account.to_le_bytes());
                        hash.update(order.remaining_quantity.to_le_bytes());
                    }
                }
            }
        }
        hash.finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(account_id: u64, side: Side, price: u64, quantity: u64, nonce: u64, order_type: OrderType) -> Transaction {
        Transaction::PlaceOrder { account_id, market_id: BTC_USD, side, price, quantity, nonce, order_type }
    }
    fn balance(exchange: &ExchangeState, account: u64, asset: u64) -> (u64, u64) {
        let b = &exchange.accounts[&account].balances[&asset]; (b.available, b.locked)
    }

    #[test]
    fn failed_transfer_keeps_sender_nonce_and_balance() {
        let mut exchange = ExchangeState::default();
        let initial = balance(&exchange, 0, USD);
        assert!(exchange.execute(&Transaction::Transfer { from: 0, to: 10_000, asset: USD, amount: 100, nonce: 0 }).is_err());
        assert_eq!(exchange.accounts[&0].nonce, 0);
        assert_eq!(balance(&exchange, 0, USD), initial);
    }

    #[test]
    fn resting_match_and_cancel_conserve_balances() {
        let mut exchange = ExchangeState::default();
        exchange.execute(&place(0, Side::Sell, 100, 3, 0, OrderType::LO)).unwrap();
        assert_eq!(balance(&exchange, 0, BTC), (997, 3));
        assert_eq!(exchange.order_books[&BTC_USD].asks[&100][0].remaining_quantity, 3);
        exchange.execute(&place(1, Side::Buy, 120, 2, 0, OrderType::LO)).unwrap();
        assert_eq!(balance(&exchange, 0, BTC), (997, 1));
        assert_eq!(balance(&exchange, 0, USD), (1_000_200, 0));
        assert_eq!(balance(&exchange, 1, BTC), (1002, 0));
        assert_eq!(balance(&exchange, 1, USD), (999_800, 0));
        exchange.execute(&Transaction::CancelOrder { account_id: 0, market_id: BTC_USD, order_id: 0, nonce: 1 }).unwrap();
        assert_eq!(balance(&exchange, 0, BTC), (998, 0));
        assert!(exchange.order_books[&BTC_USD].asks.is_empty());
    }

    #[test]
    fn buy_reservation_partial_fill_and_ioc_release() {
        let mut exchange = ExchangeState::default();
        exchange.execute(&place(0, Side::Buy, 100, 4, 0, OrderType::LO)).unwrap();
        assert_eq!(balance(&exchange, 0, USD), (999_600, 400));
        exchange.execute(&place(1, Side::Sell, 90, 2, 0, OrderType::IOC)).unwrap();
        assert_eq!(balance(&exchange, 0, USD), (999_600, 200));
        assert_eq!(balance(&exchange, 0, BTC), (1002, 0));
        assert_eq!(balance(&exchange, 1, USD), (1_000_200, 0));
        assert_eq!(balance(&exchange, 1, BTC), (998, 0));
        exchange.execute(&place(2, Side::Buy, 90, 1, 0, OrderType::IOC)).unwrap();
        assert_eq!(balance(&exchange, 2, USD), (1_000_000, 0));
        assert!(exchange.order_books[&BTC_USD].asks.is_empty());
    }

    #[test]
    fn fok_failure_is_atomic() {
        let mut exchange = ExchangeState::default();
        let before = exchange.state_digest();
        assert_eq!(exchange.execute(&place(0, Side::Buy, 100, 1, 0, OrderType::FOK)), Err(ExchangeError::FokUnfilled));
        assert_eq!(exchange.state_digest(), before);
        assert_eq!(exchange.accounts[&0].nonce, 0);
    }

    #[test]
    fn sell_reserves_base_only_and_rejects_self_trade() {
        let mut exchange = ExchangeState::default();
        exchange.execute(&place(0, Side::Sell, 100_000, 1, 0, OrderType::LO)).unwrap();
        assert_eq!(balance(&exchange, 0, BTC), (999, 1));
        let before = exchange.state_digest();
        assert_eq!(exchange.execute(&place(0, Side::Buy, 100_000, 1, 1, OrderType::LO)), Err(ExchangeError::SelfTrade));
        assert_eq!(exchange.state_digest(), before);
    }
}
