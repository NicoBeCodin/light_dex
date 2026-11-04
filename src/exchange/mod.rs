pub mod account;
mod account_batch;
pub mod address;
mod canonical;
mod catalog;
mod faucet;
mod fees;
mod indexes;
pub mod limits;
pub mod market;
pub mod market_config;
pub mod order;
pub mod orderbook;
pub mod perp;
pub mod perp_units;
mod quotes;
mod spot;
mod state_tree;

use im::{HashMap as StateMap, OrdSet};
use std::collections::HashMap;

use crate::{
    blockchain::{Digest, Transaction},
    exchange::perp::{IsolatedPerpAccount, MarginMode},
};
use account::{Account, AccountId, AssetId, Balance};
pub use faucet::FaucetConfig;
use market::{BTC, BTC_ATOMS, BTC_USD, Market, MarketId, USD, USD_ATOMS};
use market_config::{DecimalStep, FeeSchedule, MarketConfig, MarketType, btc_usd_spot_config};
use order::{Order, OrderId, OrderLocation, OrderType, Side};
use orderbook::{OrderBook, OrderBookError};
use sha2::{Digest as _, Sha256};
use state_tree::{StateKey, StateTree};
pub(crate) mod prefetch;
use crate::oracle::{OraclePair, OracleUpdate, index_from_observations};

const N_ACCOUNTS: u64 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExchangeError {
    AccountNotFound,
    InvalidNonce,
    InsufficientBalance,
    MarketNotFound,
    MarketAlreadyExists,
    InvalidMarketConfig,
    UnsupportedMarketSettlement,
    OrderNotFound,
    OrderBookNotFound,
    InvalidOrder,
    SelfTrade,
    FokUnfilled,
    ArithmeticOverflow,
    AlreadyClaimed,
    FaucetUnavailable,
    FaucetExhausted,
    InvalidOracleUpdate,
    OracleUnavailable,
    WorkLimit,
    WouldTakeLiquidity,
}

fn map_order_error(error: OrderBookError) -> ExchangeError {
    match error {
        OrderBookError::KilledFOK => ExchangeError::FokUnfilled,
        OrderBookError::SelfTrade => ExchangeError::SelfTrade,
        OrderBookError::WorkLimit => ExchangeError::WorkLimit,
        OrderBookError::WouldTakeLiquidity => ExchangeError::WouldTakeLiquidity,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OracleMark {
    pub index_e8: u64,
    pub update_digest: Digest,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RestingExposure {
    buy_qty: u64,
    sell_qty: u64,
}

impl RestingExposure {
    fn change(&mut self, side: Side, quantity: u64, add: bool) -> Result<(), ExchangeError> {
        let target = match side {
            Side::Buy => &mut self.buy_qty,
            Side::Sell => &mut self.sell_qty,
        };
        *target = if add {
            target.checked_add(quantity)
        } else {
            target.checked_sub(quantity)
        }
        .ok_or(ExchangeError::ArithmeticOverflow)?;
        Ok(())
    }

    fn is_empty(self) -> bool {
        self.buy_qty == 0 && self.sell_qty == 0
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExchangeState {
    #[serde(with = "snapshot::map_entries")]
    accounts: StateMap<AccountId, Account>,
    #[serde(with = "snapshot::map_entries")]
    perp_accounts: StateMap<(MarketId, AccountId), IsolatedPerpAccount>,
    #[serde(with = "snapshot::map_entries")]
    margin_modes: StateMap<AccountId, MarginMode>,
    #[serde(with = "snapshot::map_entries")]
    cross_collateral: StateMap<AccountId, i128>,
    #[serde(with = "snapshot::map_entries")]
    insurance_assets: StateMap<AssetId, u64>,
    bad_debt_usd: u64,
    // Derived from resting perp orders. The orders themselves are committed
    // in the state tree; this index avoids scanning the book on every quote.
    #[serde(skip)]
    perp_resting_exposure: StateMap<(MarketId, AccountId), RestingExposure>,
    /// Rebuildable index of resting perp order IDs by owner.
    #[serde(skip)]
    perp_owned_orders: StateMap<AccountId, OrdSet<OrderId>>,
    #[serde(with = "snapshot::map_entries")]
    markets: StateMap<MarketId, Market>,
    #[serde(with = "snapshot::map_entries")]
    order_books: StateMap<MarketId, OrderBook>,
    #[serde(skip)]
    order_locations: StateMap<OrderId, OrderLocation>,
    next_order_id: OrderId,
    #[serde(skip)]
    state_tree: StateTree,
    faucet: Option<FaucetConfig>,
    faucet_claims: u64,
    #[serde(with = "snapshot::map_entries")]
    oracle_marks: StateMap<OraclePair, OracleMark>,
    #[serde(with = "snapshot::map_entries")]
    funding_clocks: StateMap<MarketId, perp::FundingClock>,
    // Derived lookup indexes, excluded from canonical consensus state.
    #[serde(skip)]
    perp_markets_by_account: StateMap<AccountId, OrdSet<MarketId>>,
    #[serde(skip)]
    perp_accounts_by_market: StateMap<MarketId, OrdSet<AccountId>>,
    #[serde(skip)]
    owned_orders: StateMap<AccountId, OrdSet<OrderId>>,
    #[serde(skip)]
    order_owners: StateMap<OrderId, AccountId>,
    #[serde(skip)]
    execution_work: limits::WorkCounter,
    #[serde(skip)]
    metering: limits::MeterFlag,
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
                    used_nonces: [0; 2],
                    balances: HashMap::from([
                        (
                            USD,
                            Balance {
                                available: 1_000_000 * USD_ATOMS,
                                locked: 0,
                            },
                        ),
                        (
                            BTC,
                            Balance {
                                available: 1_000 * BTC_ATOMS,
                                locked: 0,
                            },
                        ),
                    ])
                    .into_iter()
                    .collect(),
                },
            );
        }
        exchange.rebuild_state_tree();
        exchange
    }
}

impl ExchangeState {
    /// An isolated speculative state sharing immutable collection/tree nodes.
    /// Writes copy only the affected paths (including order-book price queues).
    /// Dropping a rejected fork cannot mutate the committed parent.
    pub fn fork(&self) -> Self {
        self.clone()
    }

    fn empty() -> Self {
        let mut exchange = Self {
            accounts: StateMap::new(),
            perp_accounts: StateMap::new(),
            margin_modes: StateMap::new(),
            cross_collateral: StateMap::new(),
            insurance_assets: StateMap::new(),
            bad_debt_usd: 0,
            perp_resting_exposure: StateMap::new(),
            perp_owned_orders: StateMap::new(),
            markets: StateMap::new(),
            order_books: StateMap::new(),
            order_locations: StateMap::new(),
            next_order_id: 0,
            state_tree: StateTree::new(),
            faucet: None,
            faucet_claims: 0,
            oracle_marks: StateMap::new(),
            funding_clocks: StateMap::new(),
            perp_markets_by_account: StateMap::new(),
            perp_accounts_by_market: StateMap::new(),
            owned_orders: StateMap::new(),
            order_owners: StateMap::new(),
            execution_work: limits::WorkCounter::new(0),
            metering: limits::MeterFlag::new(false),
        };
        exchange
            .register_market(BTC_USD, btc_usd_spot_config())
            .expect("valid genesis market");
        exchange.rebuild_state_tree();
        exchange
    }

    pub fn markets(&self) -> &StateMap<MarketId, Market> {
        &self.markets
    }

    pub fn oracle_marks(&self) -> &StateMap<OraclePair, OracleMark> {
        &self.oracle_marks
    }

    pub fn oracle_mark(&self, pair: OraclePair) -> Option<OracleMark> {
        self.oracle_marks.get(&pair).copied()
    }

    /// Validates and commits one pair's mark together with its state-tree leaf.
    /// This function never consults local WebSocket state or wall-clock time.
    pub fn apply_oracle_update(&mut self, update: &OracleUpdate) -> Result<(), ExchangeError> {
        let pair = update.pair;
        if !self.markets.values().any(|market| {
            market.base_asset == pair.base_asset && market.quote_asset == pair.quote_asset
        }) {
            return Err(ExchangeError::MarketNotFound);
        }
        let calculated = index_from_observations(pair, &update.observations)
            .ok_or(ExchangeError::InvalidOracleUpdate)?;
        if calculated != update.index_e8 {
            return Err(ExchangeError::InvalidOracleUpdate);
        }
        let digest = update.digest();
        if self
            .oracle_mark(pair)
            .is_some_and(|previous| previous.update_digest == digest)
        {
            return Err(ExchangeError::InvalidOracleUpdate);
        }
        let funding = self.plan_funding(pair, calculated)?;
        let mark = OracleMark {
            index_e8: calculated,
            update_digest: digest,
        };
        self.state_tree.put(
            StateKey::OracleMark(pair),
            &canonical::encode_oracle_mark(pair, mark),
        );
        self.oracle_marks.insert(pair, mark);
        self.commit_funding(funding);
        Ok(())
    }

    pub fn create_spot_market(
        &mut self,
        id: MarketId,
        base_asset: AssetId,
        quote_asset: AssetId,
        price: DecimalStep,
        quantity: DecimalStep,
    ) -> Result<(), ExchangeError> {
        self.register_market(
            id,
            MarketConfig {
                base_asset,
                quote_asset,
                price,
                quantity,
                market_type: MarketType::Spot,
                fees: FeeSchedule::default(),
                maintenance_margin_bps: 0,
                funding_interval_updates: 0,
                max_funding_bps: 0,
                max_liquidation_slippage_bps: 0,
            },
        )
    }

    pub fn create_perp_market(
        &mut self,
        id: MarketId,
        base_asset: AssetId,
        quote_asset: AssetId,
        price: DecimalStep,
        quantity: DecimalStep,
        max_leverage: u32,
    ) -> Result<(), ExchangeError> {
        self.register_market(
            id,
            MarketConfig {
                base_asset,
                quote_asset,
                price,
                quantity,
                market_type: MarketType::Perp { max_leverage },
                fees: FeeSchedule::default(),
                maintenance_margin_bps: (10_000_u64.div_ceil(u64::from(max_leverage.max(1))) / 2)
                    .max(1),
                funding_interval_updates: 100,
                max_funding_bps: 10,
                max_liquidation_slippage_bps: 100,
            },
        )
    }

    fn register_market(&mut self, id: MarketId, config: MarketConfig) -> Result<(), ExchangeError> {
        config
            .validate()
            .map_err(|_| ExchangeError::InvalidMarketConfig)?;
        if self.markets.contains_key(&id) {
            return Err(ExchangeError::MarketAlreadyExists);
        }
        let market = Market::from_config(id, config);
        self.state_tree
            .put(StateKey::Market(id), &canonical::encode_market(&market));
        self.markets.insert(id, market);
        self.order_books.insert(id, OrderBook::new());
        Ok(())
    }

    pub fn with_claim_faucet(faucet: FaucetConfig) -> Result<Self, String> {
        faucet.validate()?;
        let mut state = Self::empty();
        state.markets.clear();
        state.order_books.clear();
        for (id, config) in catalog::genesis_markets()? {
            state
                .register_market(id, config)
                .map_err(|error| format!("invalid genesis market: {error:?}"))?;
        }
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
            used_nonces: [0; 2],
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
            ])
            .into_iter()
            .collect(),
        });
        Self::with_accounts(accounts)
    }
    pub fn accounts(&self) -> &StateMap<AccountId, Account> {
        &self.accounts
    }

    pub fn order_books(&self) -> &StateMap<MarketId, OrderBook> {
        &self.order_books
    }

    pub fn next_order_id(&self) -> OrderId {
        self.next_order_id
    }

    /// Construct a funded test/benchmark genesis without exposing mutable maps.
    pub fn with_accounts(accounts: impl IntoIterator<Item = Account>) -> Self {
        let mut state = Self::default();
        state.accounts = accounts
            .into_iter()
            .map(|account| (account.id, account))
            .collect();
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

    /// Apply a transaction atomically. Any failure leaves balances, orders and nonce unchanged.
    pub fn execute(&mut self, tx: &Transaction) -> Result<(), ExchangeError> {
        self.execute_with_receipt(tx).map(|_| ())
    }

    pub fn execute_with_receipt(
        &mut self,
        tx: &Transaction,
    ) -> Result<Vec<ExecutedTrade>, ExchangeError> {
        let profile = crate::profile::enabled();
        let started = profile.then(std::time::Instant::now);
        let stage = if profile {
            match tx.market_id().and_then(|id| self.markets.get(&id)) {
                Some(market) if matches!(market.market_type, MarketType::Perp { .. }) => {
                    "exchange_perp_transaction"
                }
                Some(_) => "exchange_spot_transaction",
                None => "exchange_other_transaction",
            }
        } else {
            ""
        };
        self.execution_work.set(0);
        self.metering.set(true);
        let result = self.execute_inner(tx);
        self.metering.set(false);
        if let Some(started) = started {
            crate::profile::record(stage, started.elapsed(), 1);
        }
        result
    }

    fn execute_inner(&mut self, tx: &Transaction) -> Result<Vec<ExecutedTrade>, ExchangeError> {
        tx.validate_shape().map_err(|_| ExchangeError::WorkLimit)?;
        self.charge_work(1)?;
        match tx {
            Transaction::UpdateQuotes {
                account_id,
                market_id,
                cancel,
                reduce,
                place,
                nonce,
            } => self.update_quotes(*account_id, *market_id, cancel, reduce, place, *nonce),
            Transaction::ReduceOrder {
                account_id,
                market_id,
                order_id,
                remaining_quantity,
                nonce,
            } => {
                self.reduce_order_atomic(
                    *account_id,
                    *market_id,
                    *order_id,
                    *remaining_quantity,
                    *nonce,
                )?;
                Ok(Vec::new())
            }
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
                if *nonce != 0 {
                    return Err(ExchangeError::InvalidNonce);
                }
                if self.accounts.contains_key(account_id) {
                    return Err(ExchangeError::AlreadyClaimed);
                }
                let faucet = self.faucet.ok_or(ExchangeError::FaucetUnavailable)?;
                if self.faucet_claims >= faucet.max_claims {
                    return Err(ExchangeError::FaucetExhausted);
                }
                let mut balances = HashMap::from([
                    (
                        USD,
                        Balance {
                            available: faucet.usd_per_claim,
                            locked: 0,
                        },
                    ),
                    (
                        BTC,
                        Balance {
                            available: faucet.btc_per_claim,
                            locked: 0,
                        },
                    ),
                ]);
                for asset in [market::ETH, market::BNB, market::XRP, market::SOL] {
                    if self
                        .markets
                        .values()
                        .any(|market| market.base_asset == asset)
                    {
                        let atoms =
                            market::asset_atoms(asset).ok_or(ExchangeError::InvalidMarketConfig)?;
                        let grant = u128::from(faucet.btc_per_claim)
                            .checked_mul(u128::from(atoms))
                            .ok_or(ExchangeError::ArithmeticOverflow)?
                            / u128::from(BTC_ATOMS);
                        balances.insert(
                            asset,
                            Balance {
                                available: u64::try_from(grant)
                                    .map_err(|_| ExchangeError::ArithmeticOverflow)?,
                                locked: 0,
                            },
                        );
                    }
                }
                let account = Account {
                    id: *account_id,
                    nonce: 1,
                    used_nonces: [1, 0],
                    balances: balances.into(),
                };
                self.faucet_claims += 1;
                self.state_tree
                    .put(StateKey::FaucetClaims, &self.faucet_claims.to_be_bytes());
                self.state_tree.put(
                    StateKey::Account(*account_id),
                    &canonical::encode_account(&account),
                );
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
            } => {
                let market = self
                    .markets
                    .get(market_id)
                    .ok_or(ExchangeError::MarketNotFound)?;
                let market_type = market.market_type;
                let pair = OraclePair::new(market.base_asset, market.quote_asset);
                let price_units = market.price;
                match market_type {
                    MarketType::Spot => self.place_order_atomic(
                        *account_id,
                        *market_id,
                        *order_type,
                        *side,
                        *price,
                        *quantity,
                        *nonce,
                    ),
                    MarketType::Perp { .. } => {
                        let mark_e8 = self
                            .oracle_mark(pair)
                            .ok_or(ExchangeError::OracleUnavailable)?
                            .index_e8;
                        let mark_price = price_units
                            .round_e8_to_tick(mark_e8)
                            .map_err(|_| ExchangeError::InvalidMarketConfig)?;
                        self.place_perp_order_atomic(
                            *account_id,
                            *market_id,
                            *order_type,
                            *side,
                            *price,
                            *quantity,
                            *nonce,
                            mark_price,
                        )
                    }
                }
            }
            Transaction::PlacePerpOrder {
                account_id,
                market_id,
                order_type,
                side,
                price,
                quantity,
                reduce_only,
                nonce,
            } => {
                let market = self
                    .markets
                    .get(market_id)
                    .ok_or(ExchangeError::MarketNotFound)?;
                if !matches!(market.market_type, MarketType::Perp { .. }) {
                    return Err(ExchangeError::UnsupportedMarketSettlement);
                }
                let pair = OraclePair::new(market.base_asset, market.quote_asset);
                let mark = self
                    .oracle_mark(pair)
                    .ok_or(ExchangeError::OracleUnavailable)?;
                let mark_price = market
                    .price
                    .round_e8_to_tick(mark.index_e8)
                    .map_err(|_| ExchangeError::InvalidMarketConfig)?;
                self.place_perp_order_with_flags(
                    *account_id,
                    *market_id,
                    *order_type,
                    *side,
                    *price,
                    *quantity,
                    *nonce,
                    mark_price,
                    *reduce_only,
                )
            }
            Transaction::CancelOrder {
                account_id,
                market_id,
                order_id,
                nonce,
            } => {
                let market_type = self
                    .markets
                    .get(market_id)
                    .ok_or(ExchangeError::MarketNotFound)?
                    .market_type;
                match market_type {
                    MarketType::Spot => {
                        self.cancel_order_atomic(*account_id, *market_id, *order_id, *nonce)
                    }
                    MarketType::Perp { .. } => {
                        self.cancel_perp_order_atomic(*account_id, *market_id, *order_id, *nonce)
                    }
                }
                .map(|()| Vec::new())
            }
            Transaction::DepositPerpCollateral {
                account_id,
                market_id,
                amount,
                nonce,
            } => self
                .deposit_perp_collateral(*account_id, *market_id, *amount, *nonce)
                .map(|()| Vec::new()),
            Transaction::WithdrawPerpCollateral {
                account_id,
                market_id,
                amount,
                nonce,
            } => self
                .withdraw_perp_collateral(*account_id, *market_id, *amount, *nonce)
                .map(|()| Vec::new()),
            Transaction::SetMarginMode {
                account_id,
                mode,
                nonce,
            } => self
                .set_margin_mode(*account_id, *mode, *nonce)
                .map(|()| Vec::new()),
            Transaction::LiquidatePerp {
                liquidator,
                target,
                market_id,
                quantity,
                nonce,
            } => self.liquidate_perp(*liquidator, *target, *market_id, *quantity, *nonce),
        }
    }

    pub fn last_execution_work(&self) -> usize {
        self.execution_work.get()
    }

    fn charge_work(&self, units: usize) -> Result<(), ExchangeError> {
        if !self.metering.get() {
            return Ok(());
        }
        let next = self.execution_work.get().saturating_add(units);
        self.execution_work.set(next);
        if next > limits::MAX_TRANSACTION_WORK {
            Err(ExchangeError::WorkLimit)
        } else {
            Ok(())
        }
    }

    fn stage_accounts(
        &self,
        ids: impl IntoIterator<Item = AccountId>,
    ) -> Result<account_batch::AccountBatch, ExchangeError> {
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
        Ok(account_batch::AccountBatch { accounts })
    }

    fn commit_accounts(&mut self, staged: account_batch::AccountBatch) {
        for (id, account) in staged.accounts {
            self.state_tree.put_account(&account);
            self.accounts.insert(id, account);
        }
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

    /// Internal block-execution boundary; normal account mutations remain
    /// immediately visible, only their Merkle hashing is deferred.
    pub(crate) fn begin_state_batch(&mut self) {
        self.state_tree.begin_batch();
    }
    pub(crate) fn finish_state_batch(&mut self) {
        self.state_tree.finish_batch();
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
        self.perp_owned_orders.clear();
        self.perp_markets_by_account.clear();
        self.perp_accounts_by_market.clear();
        self.owned_orders.clear();
        self.order_owners.clear();
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
        for (&(market_id, account_id), perp_account) in &self.perp_accounts {
            self.perp_markets_by_account
                .entry(account_id)
                .or_default()
                .insert(market_id);
            self.perp_accounts_by_market
                .entry(market_id)
                .or_default()
                .insert(account_id);
            tree.put(
                StateKey::PerpAccount(market_id, account_id),
                &canonical::encode_perp_account(perp_account),
            );
        }
        for (&account, &mode) in &self.margin_modes {
            tree.put(
                StateKey::MarginMode(account),
                &[match mode {
                    MarginMode::Isolated => 0,
                    MarginMode::Cross => 1,
                }],
            );
        }
        for (&account, &collateral) in &self.cross_collateral {
            tree.put(
                StateKey::CrossCollateral(account),
                &collateral.to_be_bytes(),
            );
        }
        for (&asset, &balance) in &self.insurance_assets {
            tree.put(StateKey::InsuranceAsset(asset), &balance.to_be_bytes());
        }
        if self.bad_debt_usd != 0 {
            tree.put(StateKey::BadDebtUsd, &self.bad_debt_usd.to_be_bytes());
        }
        for (&pair, &mark) in &self.oracle_marks {
            tree.put(
                StateKey::OracleMark(pair),
                &canonical::encode_oracle_mark(pair, mark),
            );
        }
        for (&market_id, clock) in &self.funding_clocks {
            tree.put(StateKey::FundingClock(market_id), &clock.encode());
        }
        for (&market_id, book) in &self.order_books {
            for order in book.bids.values().chain(book.asks.values()).flatten() {
                self.owned_orders
                    .entry(order.account)
                    .or_default()
                    .insert(order.id);
                self.order_owners.insert(order.id, order.account);
                tree.put(StateKey::Order(order.id), &canonical::encode_order(order));
                if self
                    .markets
                    .get(&market_id)
                    .is_some_and(|market| matches!(market.market_type, MarketType::Perp { .. }))
                {
                    self.perp_owned_orders
                        .entry(order.account)
                        .or_default()
                        .insert(order.id);
                }
            }
        }
        self.state_tree = tree;
    }

    fn sync_order_leaf(&mut self, id: OrderId) {
        let Some(location) = self.order_locations.get(&id) else {
            self.remove_order_leaf(id);
            return;
        };
        let book = self
            .order_books
            .get(&location.market)
            .expect("resting order has a book");
        let levels = match location.side {
            Side::Buy => &book.bids,
            Side::Sell => &book.asks,
        };
        let order = levels
            .get(&location.price)
            .and_then(|orders| {
                orders
                    .binary_search_by_key(&id, |order| order.id)
                    .ok()
                    .and_then(|index| orders.get(index))
            })
            .expect("resting order has a book entry");
        self.owned_orders
            .entry(order.account)
            .or_default()
            .insert(id);
        self.order_owners.insert(id, order.account);
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

// The authenticated state tree is independent of spot settlement.

#[cfg(test)]
mod tests;

mod snapshot;
