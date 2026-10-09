pub mod account;
pub mod address;
mod canonical;
pub mod market;
pub mod order;
pub mod orderbook;

use std::{collections::HashMap, sync::OnceLock};

use sha2::{Digest as _, Sha256};
use serde::{Deserialize, Serialize};

use crate::blockchain::{Digest, Transaction};
use account::{Account, AccountId, AssetId, Balance};
use market::{Market, MarketId, BTC, BTC_USD, USD};
use order::{Order, OrderId, OrderLocation, OrderType, Side};
use orderbook::{OrderBook, OrderBookError};

const N_ACCOUNTS: u64 = 100;

/// Session-bound, finite mint allowance for one-time account claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaucetConfig {
    pub max_claims: u64,
    pub usd_per_claim: u64,
    pub btc_per_claim: u64,
}

impl FaucetConfig {
    pub fn validate(self) -> Result<(), String> {
        if self.max_claims == 0 || self.usd_per_claim == 0 || self.btc_per_claim == 0 {
            return Err("faucet claims and balances must be positive".into());
        }
        self.max_claims.checked_mul(self.usd_per_claim).ok_or("maximum mintable USD overflow")?;
        self.max_claims.checked_mul(self.btc_per_claim).ok_or("maximum mintable BTC overflow")?;
        Ok(())
    }
}

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
    AlreadyClaimed,
    FaucetUnavailable,
    FaucetExhausted,
}

fn map_order_error(error: OrderBookError) -> ExchangeError {
    match error {
        OrderBookError::KilledFOK => ExchangeError::FokUnfilled,
        OrderBookError::SelfTrade => ExchangeError::SelfTrade,
        _ => ExchangeError::InvalidOrder,
    }
}

/// A fill emitted by deterministic exchange execution. Block coordinates are
/// assigned by BlockchainState when the containing transaction commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutedTrade {
    pub market_id: MarketId,
    pub price: u64,
    pub quantity: u64,
    pub aggressor_side: Side,
    pub maker_order_id: OrderId,
    pub maker_account: AccountId,
    pub taker_order_id: OrderId,
    pub taker_account: AccountId,
}

#[derive(Debug, Clone)]
pub struct ExchangeState {
    accounts: HashMap<AccountId, Account>,
    markets: HashMap<MarketId, Market>,
    order_books: HashMap<MarketId, OrderBook>,
    order_locations: HashMap<OrderId, OrderLocation>,
    next_order_id: OrderId,
    state_tree: StateTree,
    faucet: Option<FaucetConfig>,
    faucet_claims: u64,
}

impl Default for ExchangeState {
    fn default() -> Self {
        let mut exchange = Self::empty();
        // Legacy unsigned fixtures still use deterministic funded accounts.
        // Signed sessions start from `with_claim_faucet` instead.
        for account_id in 0..N_ACCOUNTS {
            exchange.accounts.insert(
                account_id.into(),
                Account {
                    id: account_id.into(),
                    nonce: 0,
                    balances: HashMap::from([
                        (
                            USD,
                            Balance {
                                available: 1_000_000,
                                locked: 0,
                            },
                        ),
                        (
                            BTC,
                            Balance {
                                available: 1_000,
                                locked: 0,
                            },
                        ),
                    ]),
                },
            );
        }
        exchange.rebuild_state_tree();
        exchange
    }
}

impl ExchangeState {
    fn empty() -> Self {
        let mut exchange = Self {
            accounts: HashMap::new(),
            markets: HashMap::new(),
            order_books: HashMap::new(),
            order_locations: HashMap::new(),
            next_order_id: 0,
            state_tree: StateTree::new(),
            faucet: None,
            faucet_claims: 0,
        };
        exchange.markets.insert(
            BTC_USD,
            Market {
                id: BTC_USD,
                base_asset: BTC,
                quote_asset: USD,
            },
        );
        exchange.order_books.insert(BTC_USD, OrderBook::new());
        exchange.rebuild_state_tree();
        exchange
    }

    pub fn with_claim_faucet(faucet: FaucetConfig) -> Result<Self, String> {
        faucet.validate()?;
        let mut state = Self::empty();
        state.faucet = Some(faucet);
        state.rebuild_state_tree();
        Ok(state)
    }

    pub fn faucet_remaining_claims(&self) -> Option<u64> {
        Some(self.faucet?.max_claims.saturating_sub(self.faucet_claims))
    }

    pub fn with_funded_addresses(addresses: impl IntoIterator<Item = AccountId>) -> Self {
        let accounts = addresses.into_iter().map(|id| Account {
            id,
            nonce: 0,
            balances: HashMap::from([
                (USD, Balance { available: 1_000_000, locked: 0 }),
                (BTC, Balance { available: 1_000, locked: 0 }),
            ]),
        });
        Self::with_accounts(accounts)
    }
    pub fn accounts(&self) -> &HashMap<AccountId, Account> {
        &self.accounts
    }

    pub fn order_books(&self) -> &HashMap<MarketId, OrderBook> {
        &self.order_books
    }

    pub fn next_order_id(&self) -> OrderId {
        self.next_order_id
    }

    /// Construct a funded test/benchmark genesis without exposing mutable maps.
    pub fn with_accounts(accounts: impl IntoIterator<Item = Account>) -> Self {
        let mut state = Self::default();
        state.accounts = accounts.into_iter().map(|account| (account.id, account)).collect();
        state.rebuild_state_tree();
        state
    }

    /// Replace an account and its commitment together. Intended for genesis
    /// construction; transactions must still use `execute`.
    pub fn upsert_genesis_account(&mut self, account: Account) {
        self.state_tree.put(
            StateKey::Account(account.id),
            &canonical::encode_account(&account),
        );
        self.accounts.insert(account.id, account);
    }

    fn balance_mut(
        &mut self,
        account: AccountId,
        asset: AssetId,
    ) -> Result<&mut Balance, ExchangeError> {
        Ok(self
            .accounts
            .get_mut(&account)
            .ok_or(ExchangeError::AccountNotFound)?
            .balances
            .entry(asset)
            .or_insert(Balance {
                available: 0,
                locked: 0,
            }))
    }

    fn debit_available(
        &mut self,
        account: AccountId,
        asset: AssetId,
        amount: u64,
    ) -> Result<(), ExchangeError> {
        let balance = self.balance_mut(account, asset)?;
        balance.available = balance
            .available
            .checked_sub(amount)
            .ok_or(ExchangeError::InsufficientBalance)?;
        Ok(())
    }

    fn credit_available(
        &mut self,
        account: AccountId,
        asset: AssetId,
        amount: u64,
    ) -> Result<(), ExchangeError> {
        let balance = self.balance_mut(account, asset)?;
        balance.available = balance
            .available
            .checked_add(amount)
            .ok_or(ExchangeError::ArithmeticOverflow)?;
        Ok(())
    }

    fn lock(
        &mut self,
        account: AccountId,
        asset: AssetId,
        amount: u64,
    ) -> Result<(), ExchangeError> {
        self.debit_available(account, asset, amount)?;
        let balance = self.balance_mut(account, asset)?;
        balance.locked = balance
            .locked
            .checked_add(amount)
            .ok_or(ExchangeError::ArithmeticOverflow)?;
        Ok(())
    }

    fn spend_locked(
        &mut self,
        account: AccountId,
        asset: AssetId,
        amount: u64,
    ) -> Result<(), ExchangeError> {
        let balance = self.balance_mut(account, asset)?;
        balance.locked = balance
            .locked
            .checked_sub(amount)
            .ok_or(ExchangeError::InsufficientBalance)?;
        Ok(())
    }

    fn unlock(
        &mut self,
        account: AccountId,
        asset: AssetId,
        amount: u64,
    ) -> Result<(), ExchangeError> {
        self.spend_locked(account, asset, amount)?;
        self.credit_available(account, asset, amount)
    }

    fn check_nonce(&self, account: AccountId, nonce: u64) -> Result<(), ExchangeError> {
        let current = self
            .accounts
            .get(&account)
            .ok_or(ExchangeError::AccountNotFound)?
            .nonce;
        if current != nonce {
            return Err(ExchangeError::InvalidNonce);
        }
        current
            .checked_add(1)
            .ok_or(ExchangeError::ArithmeticOverflow)?;
        Ok(())
    }

    fn advance_nonce(&mut self, account: AccountId) {
        self.accounts
            .get_mut(&account)
            .expect("validated account")
            .nonce += 1;
    }

    fn transfer(
        &mut self,
        from: AccountId,
        to: AccountId,
        asset: AssetId,
        amount: u64,
        nonce: u64,
    ) -> Result<(), ExchangeError> {
        self.check_nonce(from, nonce)?;
        to.public_key().map_err(|_| ExchangeError::AccountNotFound)?;
        if amount == 0 {
            return Err(ExchangeError::InvalidOrder);
        }
        self.debit_available(from, asset, amount)?;
        self.accounts.entry(to).or_insert_with(|| Account { id: to, nonce: 0, balances: HashMap::new() });
        self.credit_available(to, asset, amount)?;
        self.advance_nonce(from);
        Ok(())
    }

    #[cfg(test)]
    fn place_order(
        &mut self,
        account_id: AccountId,
        market_id: MarketId,
        order_type: OrderType,
        side: Side,
        price: u64,
        quantity: u64,
        nonce: u64,
    ) -> Result<Vec<ExecutedTrade>, ExchangeError> {
        self.check_nonce(account_id, nonce)?;
        if price == 0 || quantity == 0 {
            return Err(ExchangeError::InvalidOrder);
        }
        let market = self
            .markets
            .get(&market_id)
            .ok_or(ExchangeError::MarketNotFound)?
            .clone();
        let reserve = match side {
            Side::Buy => price
                .checked_mul(quantity)
                .ok_or(ExchangeError::ArithmeticOverflow)?,
            Side::Sell => quantity,
        };
        let reserve_asset = match side {
            Side::Buy => market.quote_asset,
            Side::Sell => market.base_asset,
        };
        self.lock(account_id, reserve_asset, reserve)?;
        let order_id = self.next_order_id;
        self.next_order_id = self
            .next_order_id
            .checked_add(1)
            .ok_or(ExchangeError::ArithmeticOverflow)?;
        let order = Order {
            id: order_id,
            account: account_id,
            market: market_id,
            side,
            price,
            original_quantity: quantity,
            remaining_quantity: quantity,
        };
        let result = self
            .order_books
            .get_mut(&market_id)
            .ok_or(ExchangeError::OrderBookNotFound)?
            .apply_order(order, order_type)
            .map_err(|error| match error {
                OrderBookError::KilledFOK => ExchangeError::FokUnfilled,
                OrderBookError::SelfTrade => ExchangeError::SelfTrade,
                _ => ExchangeError::InvalidOrder,
            })?;

        let mut trades = Vec::with_capacity(result.fills.len());
        for fill in result.fills {
            trades.push(ExecutedTrade {
                market_id,
                price: fill.price,
                quantity: fill.quantity,
                aggressor_side: side,
                maker_order_id: fill.maker_order_id,
                maker_account: fill.maker_account,
                taker_order_id: order_id,
                taker_account: account_id,
            });
            let trade_value = fill
                .price
                .checked_mul(fill.quantity)
                .ok_or(ExchangeError::ArithmeticOverflow)?;
            match side {
                Side::Buy => {
                    let reserved = price
                        .checked_mul(fill.quantity)
                        .ok_or(ExchangeError::ArithmeticOverflow)?;
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
            if fill.maker_filled {
                self.order_locations.remove(&fill.maker_order_id);
            }
        }
        if result.remaining > 0 {
            if order_type == OrderType::LO {
                self.order_locations.insert(
                    order_id,
                    OrderLocation {
                        market: market_id,
                        side,
                        price,
                    },
                );
            } else {
                let release = match side {
                    Side::Buy => price
                        .checked_mul(result.remaining)
                        .ok_or(ExchangeError::ArithmeticOverflow)?,
                    Side::Sell => result.remaining,
                };
                self.unlock(account_id, reserve_asset, release)?;
            }
        }
        self.advance_nonce(account_id);
        Ok(trades)
    }

    #[cfg(test)]
    fn cancel_order(
        &mut self,
        account_id: AccountId,
        market_id: MarketId,
        order_id: OrderId,
        nonce: u64,
    ) -> Result<(), ExchangeError> {
        self.check_nonce(account_id, nonce)?;
        let location = self
            .order_locations
            .get(&order_id)
            .ok_or(ExchangeError::OrderNotFound)?
            .clone();
        if location.market != market_id {
            return Err(ExchangeError::OrderNotFound);
        }
        let market = self
            .markets
            .get(&market_id)
            .ok_or(ExchangeError::MarketNotFound)?
            .clone();
        let order = self
            .order_books
            .get_mut(&market_id)
            .ok_or(ExchangeError::OrderBookNotFound)?
            .cancel_order(order_id, location.side, location.price, account_id)
            .map_err(|_| ExchangeError::OrderNotFound)?;
        let (asset, amount) = match order.side {
            Side::Buy => (
                market.quote_asset,
                order
                    .price
                    .checked_mul(order.remaining_quantity)
                    .ok_or(ExchangeError::ArithmeticOverflow)?,
            ),
            Side::Sell => (market.base_asset, order.remaining_quantity),
        };
        self.unlock(account_id, asset, amount)?;
        self.order_locations.remove(&order_id);
        self.advance_nonce(account_id);
        Ok(())
    }

    /// Apply a transaction atomically. Any failure leaves balances, orders and nonce unchanged.
    pub fn execute(&mut self, tx: &Transaction) -> Result<(), ExchangeError> {
        self.execute_with_receipt(tx).map(|_| ())
    }

    pub fn execute_with_receipt(
        &mut self,
        tx: &Transaction,
    ) -> Result<Vec<ExecutedTrade>, ExchangeError> {
        match tx {
            Transaction::Transfer {
                from,
                to,
                asset,
                amount,
                nonce,
            } => {
                let mut staged = self.stage_accounts([*from])?;
                if *to != *from {
                    if let Some(account) = self.accounts.get(to) {
                        staged.accounts.insert(*to, account.clone());
                    }
                }
                staged.transfer(*from, *to, *asset, *amount, *nonce)?;
                self.commit_accounts(staged);
                Ok(Vec::new())
            }
            Transaction::ClaimAirdrop { account_id, nonce } => {
                if *nonce != 0 { return Err(ExchangeError::InvalidNonce); }
                if self.accounts.contains_key(account_id) { return Err(ExchangeError::AlreadyClaimed); }
                let faucet = self.faucet.ok_or(ExchangeError::FaucetUnavailable)?;
                if self.faucet_claims >= faucet.max_claims { return Err(ExchangeError::FaucetExhausted); }
                let account = Account {
                    id: *account_id,
                    nonce: 1,
                    balances: HashMap::from([
                        (USD, Balance { available: faucet.usd_per_claim, locked: 0 }),
                        (BTC, Balance { available: faucet.btc_per_claim, locked: 0 }),
                    ]),
                };
                self.faucet_claims += 1;
                self.state_tree.put(StateKey::FaucetClaims, &self.faucet_claims.to_be_bytes());
                self.state_tree.put(StateKey::Account(*account_id), &canonical::encode_account(&account));
                self.accounts.insert(*account_id, account);
                Ok(Vec::new())
            }
            Transaction::PlaceOrder {
                account_id,
                market_id,
                order_type,
                side,
                price,
                quantity,
                nonce,
            } => self.place_order_atomic(
                *account_id,
                *market_id,
                *order_type,
                *side,
                *price,
                *quantity,
                *nonce,
            ),
            Transaction::CancelOrder {
                account_id,
                market_id,
                order_id,
                nonce,
            } => self
                .cancel_order_atomic(*account_id, *market_id, *order_id, *nonce)
                .map(|()| Vec::new()),
        }
    }

    fn stage_accounts(
        &self,
        ids: impl IntoIterator<Item = AccountId>,
    ) -> Result<Self, ExchangeError> {
        let mut accounts = HashMap::new();
        for id in ids {
            if let std::collections::hash_map::Entry::Vacant(entry) = accounts.entry(id) {
                entry.insert(
                    self.accounts
                        .get(&id)
                        .ok_or(ExchangeError::AccountNotFound)?
                        .clone(),
                );
            }
        }
        Ok(Self {
            accounts,
            markets: HashMap::new(),
            order_books: HashMap::new(),
            order_locations: HashMap::new(),
            next_order_id: 0,
            state_tree: StateTree::new(),
            faucet: None,
            faucet_claims: 0,
        })
    }

    fn commit_accounts(&mut self, staged: Self) {
        for (id, account) in staged.accounts {
            self.state_tree
                .put(StateKey::Account(id), &canonical::encode_account(&account));
            self.accounts.insert(id, account);
        }
    }

    fn place_order_atomic(
        &mut self,
        account_id: AccountId,
        market_id: MarketId,
        order_type: OrderType,
        side: Side,
        price: u64,
        quantity: u64,
        nonce: u64,
    ) -> Result<Vec<ExecutedTrade>, ExchangeError> {
        self.check_nonce(account_id, nonce)?;
        if price == 0 || quantity == 0 {
            return Err(ExchangeError::InvalidOrder);
        }
        let market = self
            .markets
            .get(&market_id)
            .ok_or(ExchangeError::MarketNotFound)?
            .clone();
        let reserve = match side {
            Side::Buy => price
                .checked_mul(quantity)
                .ok_or(ExchangeError::ArithmeticOverflow)?,
            Side::Sell => quantity,
        };
        let reserve_asset = match side {
            Side::Buy => market.quote_asset,
            Side::Sell => market.base_asset,
        };
        let order_id = self.next_order_id;
        let next_order_id = order_id
            .checked_add(1)
            .ok_or(ExchangeError::ArithmeticOverflow)?;
        let order = Order {
            id: order_id,
            account: account_id,
            market: market_id,
            side,
            price,
            original_quantity: quantity,
            remaining_quantity: quantity,
        };
        let preview = self
            .order_books
            .get(&market_id)
            .ok_or(ExchangeError::OrderBookNotFound)?
            .preview_order(&order, order_type)
            .map_err(map_order_error)?;
        let mut staged = self.stage_accounts(
            std::iter::once(account_id).chain(preview.fills.iter().map(|fill| fill.maker_account)),
        )?;
        staged.lock(account_id, reserve_asset, reserve)?;
        let mut trades = Vec::with_capacity(preview.fills.len());
        for fill in &preview.fills {
            trades.push(ExecutedTrade {
                market_id,
                price: fill.price,
                quantity: fill.quantity,
                aggressor_side: side,
                maker_order_id: fill.maker_order_id,
                maker_account: fill.maker_account,
                taker_order_id: order_id,
                taker_account: account_id,
            });
            let trade_value = fill
                .price
                .checked_mul(fill.quantity)
                .ok_or(ExchangeError::ArithmeticOverflow)?;
            match side {
                Side::Buy => {
                    let reserved = price
                        .checked_mul(fill.quantity)
                        .ok_or(ExchangeError::ArithmeticOverflow)?;
                    staged.spend_locked(account_id, market.quote_asset, reserved)?;
                    staged.credit_available(
                        account_id,
                        market.quote_asset,
                        reserved - trade_value,
                    )?;
                    staged.credit_available(account_id, market.base_asset, fill.quantity)?;
                    staged.spend_locked(fill.maker_account, market.base_asset, fill.quantity)?;
                    staged.credit_available(fill.maker_account, market.quote_asset, trade_value)?;
                }
                Side::Sell => {
                    staged.spend_locked(account_id, market.base_asset, fill.quantity)?;
                    staged.credit_available(account_id, market.quote_asset, trade_value)?;
                    staged.spend_locked(fill.maker_account, market.quote_asset, trade_value)?;
                    staged.credit_available(
                        fill.maker_account,
                        market.base_asset,
                        fill.quantity,
                    )?;
                }
            }
        }
        if preview.remaining > 0 && order_type != OrderType::LO {
            let release = match side {
                Side::Buy => price
                    .checked_mul(preview.remaining)
                    .ok_or(ExchangeError::ArithmeticOverflow)?,
                Side::Sell => preview.remaining,
            };
            staged.unlock(account_id, reserve_asset, release)?;
        }
        staged.advance_nonce(account_id);

        // All fallible settlement happened on the touched-account copies. The
        // book cannot change between preview and apply in this single-threaded
        // execution path, and apply has no fallible operations after validation.
        let actual = self
            .order_books
            .get_mut(&market_id)
            .expect("previewed book exists")
            .apply_order(order, order_type)
            .map_err(map_order_error)?;
        debug_assert_eq!(actual.remaining, preview.remaining);
        debug_assert_eq!(actual.fills, preview.fills);
        for fill in &actual.fills {
            if fill.maker_filled {
                self.order_locations.remove(&fill.maker_order_id);
            }
        }
        if actual.remaining > 0 && order_type == OrderType::LO {
            self.order_locations.insert(
                order_id,
                OrderLocation {
                    market: market_id,
                    side,
                    price,
                },
            );
        }
        self.next_order_id = next_order_id;
        self.state_tree.put(
            StateKey::NextOrderId,
            &canonical::encode_next_order_id(next_order_id),
        );
        for fill in &actual.fills {
            self.sync_order_leaf(fill.maker_order_id);
        }
        if actual.remaining > 0 && order_type == OrderType::LO {
            self.sync_order_leaf(order_id);
        }
        self.commit_accounts(staged);
        Ok(trades)
    }

    fn cancel_order_atomic(
        &mut self,
        account_id: AccountId,
        market_id: MarketId,
        order_id: OrderId,
        nonce: u64,
    ) -> Result<(), ExchangeError> {
        self.check_nonce(account_id, nonce)?;
        let location = self
            .order_locations
            .get(&order_id)
            .ok_or(ExchangeError::OrderNotFound)?
            .clone();
        if location.market != market_id {
            return Err(ExchangeError::OrderNotFound);
        }
        let market = self
            .markets
            .get(&market_id)
            .ok_or(ExchangeError::MarketNotFound)?
            .clone();
        let order = self
            .order_books
            .get(&market_id)
            .ok_or(ExchangeError::OrderBookNotFound)?
            .find_order(order_id, location.side, location.price, account_id)
            .ok_or(ExchangeError::OrderNotFound)?;
        let (asset, amount) = match order.side {
            Side::Buy => (
                market.quote_asset,
                order
                    .price
                    .checked_mul(order.remaining_quantity)
                    .ok_or(ExchangeError::ArithmeticOverflow)?,
            ),
            Side::Sell => (market.base_asset, order.remaining_quantity),
        };
        let mut staged = self.stage_accounts([account_id])?;
        staged.unlock(account_id, asset, amount)?;
        staged.advance_nonce(account_id);
        self.order_books
            .get_mut(&market_id)
            .expect("validated book exists")
            .cancel_order(order_id, location.side, location.price, account_id)
            .map_err(|_| ExchangeError::OrderNotFound)?;
        self.order_locations.remove(&order_id);
        self.state_tree.remove(StateKey::Order(order_id));
        self.commit_accounts(staged);
        Ok(())
    }

    pub fn get_nonce_for_account(&self, account_id: AccountId) -> Result<u64, ExchangeError> {
        Ok(self
            .accounts
            .get(&account_id)
            .ok_or(ExchangeError::AccountNotFound)?
            .nonce)
    }

    /// Incremental consensus root. Only leaves touched by successful
    /// transactions are rehashed; reading the root is constant-time.
    pub fn state_digest(&self) -> Digest {
        self.state_tree.root()
    }

    /// Rebuild after constructing a fixture or otherwise directly mutating
    /// the public state maps. Normal transaction execution maintains the tree.
    pub fn verify_state_tree(&self) -> bool {
        let mut rebuilt = self.clone();
        rebuilt.rebuild_state_tree();
        self.state_digest() == rebuilt.state_digest()
    }

    fn rebuild_state_tree(&mut self) {
        let mut tree = StateTree::new();
        tree.put(
            StateKey::NextOrderId,
            &canonical::encode_next_order_id(self.next_order_id),
        );
        if self.faucet.is_some() {
            tree.put(StateKey::FaucetClaims, &self.faucet_claims.to_be_bytes());
        }
        for (&id, account) in &self.accounts {
            tree.put(StateKey::Account(id), &canonical::encode_account(account));
        }
        for (&id, market) in &self.markets {
            tree.put(StateKey::Market(id), &canonical::encode_market(market));
        }
        for book in self.order_books.values() {
            for order in book.bids.values().chain(book.asks.values()).flatten() {
                tree.put(StateKey::Order(order.id), &canonical::encode_order(order));
            }
        }
        self.state_tree = tree;
    }

    fn sync_order_leaf(&mut self, id: OrderId) {
        let Some(location) = self.order_locations.get(&id) else {
            self.state_tree.remove(StateKey::Order(id));
            return;
        };
        let book = self.order_books.get(&location.market).expect("resting order has a book");
        let levels = match location.side {
            Side::Buy => &book.bids,
            Side::Sell => &book.asks,
        };
        let order = levels
            .get(&location.price)
            .and_then(|orders| orders.iter().find(|order| order.id == id))
            .expect("resting order has a book entry");
        self.state_tree
            .put(StateKey::Order(id), &canonical::encode_order(order));
    }

    /// Previous full scan, retained for benchmarking and migration checks.
    pub fn full_state_digest_legacy(&self) -> Digest {
        let mut hash = Sha256::new();
        hash.update(self.next_order_id.to_le_bytes());
        let mut accounts = self.accounts.iter().collect::<Vec<_>>();
        accounts.sort_by_key(|(id, _)| **id);
        for (&id, account) in accounts {
            hash.update(id.bytes());
            hash.update(account.nonce.to_le_bytes());
            let mut balances = account.balances.iter().collect::<Vec<_>>();
            balances.sort_by_key(|(asset, _)| **asset);
            for (&asset, balance) in balances {
                hash.update(asset.to_le_bytes());
                hash.update(balance.available.to_le_bytes());
                hash.update(balance.locked.to_le_bytes());
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
                        hash.update(order.id.to_le_bytes());
                        hash.update(order.account.bytes());
                        hash.update(order.remaining_quantity.to_le_bytes());
                    }
                }
            }
        }
        // println!("Time for state digest: {}µs", difference.as_micros());

        hash.finalize().into()
    }
}

//StateTree

type StateRoot = [u8; 32];
#[derive(Clone, Copy)]
enum StateKey {
    Account(AccountId),
    Order(OrderId),
    Market(MarketId),
    NextOrderId,
    FaucetClaims,
}

use std::sync::Arc;

#[derive(Debug, Clone, Default)]
struct StateTree {
    root: Option<Arc<Node>>,
}

#[derive(Debug)]
enum Node {
    Leaf {
        key_hash: Digest,
        value_hash: Digest,
        hash: Digest,
    },
    Branch {
        split_bit: u16,
        min_key: Digest,
        left: Arc<Node>,
        right: Arc<Node>,
        hash: Digest,
    },
}

impl Node {
    fn hash(&self) -> Digest {
        match self {
            Node::Leaf { hash, .. } | Node::Branch { hash, .. } => *hash,
        }
    }
    fn min_key(&self) -> Digest {
        match self {
            Node::Leaf { key_hash, .. } => *key_hash,
            Node::Branch { min_key, .. } => *min_key,
        }
    }
}

impl StateTree {
    fn new() -> Self {
        Self::default()
    }
    fn root(&self) -> StateRoot {
        static EMPTY_ROOT: OnceLock<Digest> = OnceLock::new();
        self.root
            .as_ref()
            .map(|node| node.hash())
            .unwrap_or_else(|| *EMPTY_ROOT.get_or_init(|| hash_parts(&[b"light-dex/state/empty/v1"])))
    }

    fn get_value_hash(&self, key: StateKey) -> Option<Digest> {
        let key_hash = hash_key(key);
        let mut node = self.root.as_deref()?;
        loop {
            match node {
                Node::Leaf {
                    key_hash: stored_key,
                    value_hash,
                    ..
                } => {
                    return (*stored_key == key_hash).then_some(*value_hash);
                }
                Node::Branch {
                    split_bit,
                    left: left_node,
                    right: right_node,
                    ..
                } => {
                    node = if bit_at(&key_hash, *split_bit) == 0 {
                        left_node
                    } else {
                        right_node
                    };
                }
            }
        }
    }
    fn put(&mut self, key: StateKey, value: &[u8]) {
        let key_hash = hash_key(key);
        let value_hash = hash_value(value);
        self.root = Some(match &self.root {
            Some(root) => insert(root, key_hash, value_hash),
            None => leaf(key_hash, value_hash),
        });
    }

    fn remove(&mut self, key: StateKey) -> bool {
        let Some(root) = &self.root else {
            return false;
        };
        let (next, removed) = remove(root, &hash_key(key));
        self.root = next;
        removed
    }
}

fn leaf(key_hash: Digest, value_hash: Digest) -> Arc<Node> {
    let hash = hash_parts(&[b"light-dex/state/leaf/v1", &key_hash, &value_hash]);
    Arc::new(Node::Leaf {
        key_hash,
        value_hash,
        hash,
    })
}

fn branch(split_bit: u16, left: Arc<Node>, right: Arc<Node>) -> Arc<Node> {
    let min_key = left.min_key();
    let hash = hash_parts(&[
        b"light-dex/state/branch/v1",
        &split_bit.to_be_bytes(),
        &left.hash(),
        &right.hash(),
    ]);
    Arc::new(Node::Branch {
        split_bit,
        min_key,
        left,
        right,
        hash,
    })
}

fn first_differing_bit(a: &Digest, b: &Digest) -> Option<u16> {
    a.iter().zip(b).enumerate().find_map(|(index, (&x, &y))| {
        let difference = x ^ y;
        (difference != 0).then(|| (index * 8 + difference.leading_zeros() as usize) as u16)
    })
}

fn join(new_leaf: Arc<Node>, existing: Arc<Node>, split_bit: u16) -> Arc<Node> {
    if bit_at(&new_leaf.min_key(), split_bit) == 0 {
        branch(split_bit, new_leaf, existing)
    } else {
        branch(split_bit, existing, new_leaf)
    }
}

fn insert(node: &Arc<Node>, key_hash: Digest, value_hash: Digest) -> Arc<Node> {
    match node.as_ref() {
        Node::Leaf {
            key_hash: existing_key,
            value_hash: existing_value,
            ..
        } => {
            if *existing_key == key_hash {
                if *existing_value == value_hash {
                    Arc::clone(node)
                } else {
                    leaf(key_hash, value_hash)
                }
            } else {
                let split_bit = first_differing_bit(existing_key, &key_hash)
                    .expect("Different keys must differ in a bit");
                join(leaf(key_hash, value_hash), Arc::clone(node), split_bit)
            }
        }
        Node::Branch {
            split_bit,
            min_key,
            left: left_node,
            right: right_node,
            ..
        } => {
            if let Some(difference) = first_differing_bit(min_key, &key_hash) {
                if difference < *split_bit {
                    return join(leaf(key_hash, value_hash), Arc::clone(node), difference);
                }
            }
            if bit_at(&key_hash, *split_bit) == 0 {
                let replacement = insert(left_node, key_hash, value_hash);
                if Arc::ptr_eq(left_node, &replacement) {
                    Arc::clone(node)
                } else {
                    branch(*split_bit, replacement, Arc::clone(right_node))
                }
            } else {
                let replacement = insert(right_node, key_hash, value_hash);
                if Arc::ptr_eq(right_node, &replacement) {
                    Arc::clone(node)
                } else {
                    branch(*split_bit, Arc::clone(left_node), replacement)
                }
            }
        }
    }
}

fn remove(node: &Arc<Node>, key_hash: &Digest) -> (Option<Arc<Node>>, bool) {
    match node.as_ref() {
        Node::Leaf {
            key_hash: existing_key,
            ..
        } => {
            if existing_key == key_hash {
                (None, true)
            } else {
                (Some(Arc::clone(node)), false)
            }
        }

        Node::Branch {
            split_bit,
            left,
            right,
            ..
        } => {
            if bit_at(key_hash, *split_bit) == 0 {
                let (replacement, removed) = remove(left, key_hash);

                if !removed {
                    return (Some(Arc::clone(node)), false);
                }

                // If the left subtree disappeared, this branch now
                // has only one child and must collapse.
                let next = match replacement {
                    Some(left) => branch(*split_bit, left, Arc::clone(right)),
                    None => Arc::clone(right),
                };

                (Some(next), true)
            } else {
                let (replacement, removed) = remove(right, key_hash);

                if !removed {
                    return (Some(Arc::clone(node)), false);
                }

                let next = match replacement {
                    Some(right) => branch(*split_bit, Arc::clone(left), right),
                    None => Arc::clone(left),
                };

                (Some(next), true)
            }
        }
    }
}

fn bit_at(hash: &Digest, index: u16) -> u8 {
    debug_assert!(index < 256);
    (hash[(index / 8) as usize] >> (7 - (index % 8))) & 1
}

fn hash_parts(parts: &[&[u8]]) -> Digest {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn hash_key(key: StateKey) -> Digest {
    const DOMAIN: &[u8] = b"light-dex/state/key/v2";

    match key {
        StateKey::Account(id) => hash_parts(&[DOMAIN, &[0], &id.bytes()]),
        StateKey::Order(id) => hash_parts(&[DOMAIN, &[1], &id.to_be_bytes()]),
        StateKey::Market(id) => hash_parts(&[DOMAIN, &[2], &id.to_be_bytes()]),
        StateKey::NextOrderId => hash_parts(&[DOMAIN, &[3]]),
        StateKey::FaucetClaims => hash_parts(&[DOMAIN, &[4]]),
    }
}

fn hash_value(value: &[u8]) -> Digest {
    hash_parts(&[
        b"light-dex/state/value/v1",
        &(value.len() as u64).to_be_bytes(),
        value,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    fn place(
        account_id: u64,
        side: Side,
        price: u64,
        quantity: u64,
        nonce: u64,
        order_type: OrderType,
    ) -> Transaction {
        Transaction::PlaceOrder {
            account_id: account_id.into(),
            market_id: BTC_USD,
            side,
            price,
            quantity,
            nonce,
            order_type,
        }
    }
    fn balance(exchange: &ExchangeState, account: u64, asset: u64) -> (u64, u64) {
        let b = &exchange.accounts[&account.into()].balances[&asset];
        (b.available, b.locked)
    }

    fn execute_reference(
        state: &mut ExchangeState,
        tx: &Transaction,
    ) -> Result<Vec<ExecutedTrade>, ExchangeError> {
        let mut staged = state.clone();
        let trades = match tx {
            Transaction::ClaimAirdrop { .. } => staged.execute_with_receipt(tx)?,
            Transaction::Transfer {
                from,
                to,
                asset,
                amount,
                nonce,
            } => {
                staged.transfer(*from, *to, *asset, *amount, *nonce)?;
                Vec::new()
            }
            Transaction::PlaceOrder {
                account_id,
                market_id,
                order_type,
                side,
                price,
                quantity,
                nonce,
            } => staged.place_order(
                *account_id,
                *market_id,
                *order_type,
                *side,
                *price,
                *quantity,
                *nonce,
            )?,
            Transaction::CancelOrder {
                account_id,
                market_id,
                order_id,
                nonce,
            } => {
                staged.cancel_order(*account_id, *market_id, *order_id, *nonce)?;
                Vec::new()
            }
        };
        *state = staged;
        state.rebuild_state_tree();
        Ok(trades)
    }

    #[test]
    fn staged_execution_matches_whole_state_clone_reference() {
        let mut rng = StdRng::seed_from_u64(0x5eed_2026);
        let mut fast = ExchangeState::default();
        let mut reference = ExchangeState::default();
        for _ in 0..600 {
            let account_id = rng.random_range(0..20);
            let nonce = reference.accounts[&account_id.into()].nonce + u64::from(rng.random_ratio(1, 8));
            let tx = match rng.random_range(0..5) {
                0 => Transaction::Transfer {
                    from: account_id.into(),
                    to: rng.random_range(0..20).into(),
                    asset: rng.random_range(0..2),
                    amount: rng.random_range(1..500),
                    nonce,
                },
                1 => {
                    let owned = reference.order_books[&BTC_USD]
                        .bids
                        .values()
                        .chain(reference.order_books[&BTC_USD].asks.values())
                        .flat_map(|level| level.iter())
                        .find(|order| order.account == account_id.into())
                        .map(|order| order.id);
                    Transaction::CancelOrder {
                        account_id: account_id.into(),
                        market_id: BTC_USD,
                        order_id: owned
                            .unwrap_or_else(|| rng.random_range(0..reference.next_order_id + 2)),
                        nonce,
                    }
                }
                _ => place(
                    account_id,
                    if rng.random_bool(0.5) {
                        Side::Buy
                    } else {
                        Side::Sell
                    },
                    rng.random_range(90..110),
                    rng.random_range(1..4),
                    nonce,
                    match rng.random_range(0..3) {
                        0 => OrderType::LO,
                        1 => OrderType::IOC,
                        _ => OrderType::FOK,
                    },
                ),
            };
            assert_eq!(
                fast.execute_with_receipt(&tx),
                execute_reference(&mut reference, &tx),
                "{tx:?}"
            );
            assert_eq!(fast.state_digest(), reference.state_digest(), "{tx:?}");
            assert_eq!(
                fast.order_locations.len(),
                reference.order_locations.len(),
                "{tx:?}"
            );
            let mut rebuilt = fast.clone();
            rebuilt.rebuild_state_tree();
            assert_eq!(fast.state_digest(), rebuilt.state_digest(), "{tx:?}");
        }
    }

    #[test]
    fn state_tree_tracks_resting_orders_fills_and_cancellation() {
        let mut state = ExchangeState::default();
        let initial = state.state_digest();
        assert!(state.state_tree.get_value_hash(StateKey::Account(0.into())).is_some());

        state.execute(&place(1, Side::Sell, 100, 2, 0, OrderType::LO)).unwrap();
        assert!(state.state_tree.get_value_hash(StateKey::Order(0)).is_some());
        let after_rest = state.state_digest();
        assert_ne!(initial, after_rest);

        state.execute(&place(2, Side::Buy, 100, 1, 0, OrderType::IOC)).unwrap();
        let partial = state.state_digest();
        assert_ne!(after_rest, partial);
        assert!(state.state_tree.get_value_hash(StateKey::Order(0)).is_some());

        state.execute(&Transaction::CancelOrder {
            account_id: 1.into(),
            market_id: BTC_USD,
            order_id: 0,
            nonce: 1,
        }).unwrap();
        assert!(state.state_tree.get_value_hash(StateKey::Order(0)).is_none());

        let mut rebuilt = state.clone();
        rebuilt.rebuild_state_tree();
        assert_eq!(state.state_digest(), rebuilt.state_digest());
    }

    #[test]
    fn state_tree_root_is_independent_of_insertion_order() {
        let mut forward = StateTree::new();
        let mut reverse = StateTree::new();
        for id in 0..64 {
            forward.put(StateKey::Account(id.into()), &id.to_be_bytes());
        }
        for id in (0u64..64).rev() {
            reverse.put(StateKey::Account(id.into()), &id.to_be_bytes());
        }
        assert_eq!(forward.root(), reverse.root());
        for id in 0..64 {
            assert!(forward.remove(StateKey::Account(id.into())));
        }
        assert_eq!(forward.root(), StateTree::new().root());
    }

    #[test]
    fn genesis_account_update_keeps_commitment_in_sync() {
        let mut state = ExchangeState::default();
        let before = state.state_digest();
        let mut account = state.accounts()[&0.into()].clone();
        account.balances.get_mut(&USD).unwrap().available += 1;
        state.upsert_genesis_account(account);
        assert_ne!(state.state_digest(), before);
        assert!(state.verify_state_tree());
    }

    #[test]
    fn claim_faucet_mints_with_a_committed_claim_limit() {
        let faucet = FaucetConfig {
            max_claims: 2,
            usd_per_claim: 50,
            btc_per_claim: 3,
        };
        let mut state = ExchangeState::with_claim_faucet(faucet).unwrap();
        assert!(state.accounts.is_empty());
        assert_eq!(state.faucet_remaining_claims(), Some(2));
        let first = address::Address::fixture(901);
        let second = address::Address::fixture(902);
        state.execute(&Transaction::ClaimAirdrop { account_id: first, nonce: 0 }).unwrap();
        assert_eq!(state.accounts[&first].nonce, 1);
        assert_eq!(state.accounts[&first].balances[&USD].available, 50);
        assert_eq!(state.accounts[&first].balances[&BTC].available, 3);
        assert_eq!(state.faucet_remaining_claims(), Some(1));
        let root = state.state_digest();
        assert_eq!(state.execute(&Transaction::ClaimAirdrop { account_id: first, nonce: 0 }), Err(ExchangeError::AlreadyClaimed));
        assert_eq!(state.state_digest(), root);
        state.execute(&Transaction::ClaimAirdrop { account_id: second, nonce: 0 }).unwrap();
        assert_eq!(state.faucet_remaining_claims(), Some(0));
        assert_eq!(state.execute(&Transaction::ClaimAirdrop { account_id: address::Address::fixture(903), nonce: 0 }), Err(ExchangeError::FaucetExhausted));
        assert!(state.verify_state_tree());
    }

    #[test]
    fn failed_transfer_keeps_sender_nonce_and_balance() {
        let mut exchange = ExchangeState::default();
        let initial = balance(&exchange, 0, USD);
        assert!(exchange
            .execute(&Transaction::Transfer {
                from: 0.into(),
                to: 10_000.into(),
                asset: USD,
                amount: 1_000_001,
                nonce: 0
            })
            .is_err());
        assert_eq!(exchange.accounts[&0.into()].nonce, 0);
        assert_eq!(balance(&exchange, 0, USD), initial);
    }

    #[test]
    fn transfer_creates_address_account_only_after_success() {
        let mut exchange = ExchangeState::default();
        let recipient = crate::exchange::address::Address::fixture(10_000);
        assert!(!exchange.accounts.contains_key(&recipient));
        let transfer = Transaction::Transfer { from: 0.into(), to: recipient, asset: USD, amount: 25, nonce: 0 };
        exchange.execute(&transfer).unwrap();
        assert_eq!(exchange.accounts[&recipient].nonce, 0);
        assert_eq!(exchange.accounts[&recipient].balances[&USD].available, 25);
        assert_eq!(exchange.accounts[&0.into()].nonce, 1);
        assert!(exchange.verify_state_tree());
        assert!(exchange.execute(&transfer).is_err());
        assert_eq!(exchange.accounts[&recipient].balances[&USD].available, 25);
    }

    #[test]
    fn resting_match_and_cancel_conserve_balances() {
        let mut exchange = ExchangeState::default();
        exchange
            .execute(&place(0, Side::Sell, 100, 3, 0, OrderType::LO))
            .unwrap();
        assert_eq!(balance(&exchange, 0, BTC), (997, 3));
        assert_eq!(
            exchange.order_books[&BTC_USD].asks[&100][0].remaining_quantity,
            3
        );
        exchange
            .execute(&place(1, Side::Buy, 120, 2, 0, OrderType::LO))
            .unwrap();
        assert_eq!(balance(&exchange, 0, BTC), (997, 1));
        assert_eq!(balance(&exchange, 0, USD), (1_000_200, 0));
        assert_eq!(balance(&exchange, 1, BTC), (1002, 0));
        assert_eq!(balance(&exchange, 1, USD), (999_800, 0));
        exchange
            .execute(&Transaction::CancelOrder {
                account_id: 0.into(),
                market_id: BTC_USD,
                order_id: 0,
                nonce: 1,
            })
            .unwrap();
        assert_eq!(balance(&exchange, 0, BTC), (998, 0));
        assert!(exchange.order_books[&BTC_USD].asks.is_empty());
    }

    #[test]
    fn buy_reservation_partial_fill_and_ioc_release() {
        let mut exchange = ExchangeState::default();
        exchange
            .execute(&place(0, Side::Buy, 100, 4, 0, OrderType::LO))
            .unwrap();
        assert_eq!(balance(&exchange, 0, USD), (999_600, 400));
        exchange
            .execute(&place(1, Side::Sell, 90, 2, 0, OrderType::IOC))
            .unwrap();
        assert_eq!(balance(&exchange, 0, USD), (999_600, 200));
        assert_eq!(balance(&exchange, 0, BTC), (1002, 0));
        assert_eq!(balance(&exchange, 1, USD), (1_000_200, 0));
        assert_eq!(balance(&exchange, 1, BTC), (998, 0));
        exchange
            .execute(&place(2, Side::Buy, 90, 1, 0, OrderType::IOC))
            .unwrap();
        assert_eq!(balance(&exchange, 2, USD), (1_000_000, 0));
        assert!(exchange.order_books[&BTC_USD].asks.is_empty());
    }

    #[test]
    fn fok_failure_is_atomic() {
        let mut exchange = ExchangeState::default();
        let before = exchange.state_digest();
        assert_eq!(
            exchange.execute(&place(0, Side::Buy, 100, 1, 0, OrderType::FOK)),
            Err(ExchangeError::FokUnfilled)
        );
        assert_eq!(exchange.state_digest(), before);
        assert_eq!(exchange.accounts[&0.into()].nonce, 0);
    }

    #[test]
    fn sell_reserves_base_only_and_rejects_self_trade() {
        let mut exchange = ExchangeState::default();
        exchange
            .execute(&place(0, Side::Sell, 100_000, 1, 0, OrderType::LO))
            .unwrap();
        assert_eq!(balance(&exchange, 0, BTC), (999, 1));
        let before = exchange.state_digest();
        assert_eq!(
            exchange.execute(&place(0, Side::Buy, 100_000, 1, 1, OrderType::LO)),
            Err(ExchangeError::SelfTrade)
        );
        assert_eq!(exchange.state_digest(), before);
    }
}
