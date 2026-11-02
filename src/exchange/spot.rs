use crate::exchange::fees::fee_atoms;
use crate::exchange::perp_units::{PerpUnits, USD_DECIMALS};

use super::*;

impl ExchangeState {
    pub(super) fn place_order_atomic(
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
            .ok_or(ExchangeError::MarketNotFound)?;
        if !market.supports_current_spot_settlement() {
            return Err(ExchangeError::UnsupportedMarketSettlement);
        }
        if !market.price.accepts(price) || !market.quantity.accepts(quantity) {
            return Err(ExchangeError::InvalidOrder);
        }
        let reserve = match side {
            Side::Buy => market
                .quote_atoms(price, quantity)
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
            .map_err(|error| {
                if matches!(error, OrderBookError::WorkLimit | OrderBookError::KilledFOK) {
                    let _ = self.charge_work(limits::MAX_FILLS_PER_ORDER * 4);
                }
                map_order_error(error)
            })?;
        let mut staged = self.stage_accounts(
            std::iter::once(account_id).chain(preview.fills.iter().map(|fill| fill.maker_account)),
        )?;
        self.charge_work(preview.fills.len() * 4)?;
        staged.lock(account_id, reserve_asset, reserve)?;
        let mut trades = Vec::with_capacity(preview.fills.len());
        let mut base_fees = 0u64;
        let mut quote_fees = 0u64;
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
            let trade_value = market
                .quote_atoms(fill.price, fill.quantity)
                .ok_or(ExchangeError::ArithmeticOverflow)?;
            match side {
                Side::Buy => {
                    let taker_fee = fee_atoms(fill.quantity, market.fees.taker_bps)?;
                    let maker_fee = fee_atoms(trade_value, market.fees.maker_bps)?;
                    base_fees = base_fees
                        .checked_add(taker_fee)
                        .ok_or(ExchangeError::ArithmeticOverflow)?;
                    quote_fees = quote_fees
                        .checked_add(maker_fee)
                        .ok_or(ExchangeError::ArithmeticOverflow)?;
                    let reserved = market
                        .quote_atoms(price, fill.quantity)
                        .ok_or(ExchangeError::ArithmeticOverflow)?;
                    staged.spend_locked(account_id, market.quote_asset, reserved)?;
                    staged.credit_available(
                        account_id,
                        market.quote_asset,
                        reserved - trade_value,
                    )?;
                    staged.credit_available(
                        account_id,
                        market.base_asset,
                        fill.quantity - taker_fee,
                    )?;
                    staged.spend_locked(fill.maker_account, market.base_asset, fill.quantity)?;
                    staged.credit_available(
                        fill.maker_account,
                        market.quote_asset,
                        trade_value - maker_fee,
                    )?;
                }
                Side::Sell => {
                    let taker_fee = fee_atoms(trade_value, market.fees.taker_bps)?;
                    let maker_fee = fee_atoms(fill.quantity, market.fees.maker_bps)?;
                    quote_fees = quote_fees
                        .checked_add(taker_fee)
                        .ok_or(ExchangeError::ArithmeticOverflow)?;
                    base_fees = base_fees
                        .checked_add(maker_fee)
                        .ok_or(ExchangeError::ArithmeticOverflow)?;
                    staged.spend_locked(account_id, market.base_asset, fill.quantity)?;
                    staged.credit_available(
                        account_id,
                        market.quote_asset,
                        trade_value - taker_fee,
                    )?;
                    staged.spend_locked(fill.maker_account, market.quote_asset, trade_value)?;
                    staged.credit_available(
                        fill.maker_account,
                        market.base_asset,
                        fill.quantity - maker_fee,
                    )?;
                }
            }
        }
        if preview.remaining > 0 && !order_type.rests() {
            let release = match side {
                Side::Buy => market
                    .quote_atoms(price, preview.remaining)
                    .ok_or(ExchangeError::ArithmeticOverflow)?,
                Side::Sell => preview.remaining,
            };
            staged.unlock(account_id, reserve_asset, release)?;
        }
        staged.advance_nonce(account_id, nonce);
        self.insurance_balance(market.base_asset)
            .checked_add(base_fees)
            .ok_or(ExchangeError::ArithmeticOverflow)?;
        self.insurance_balance(market.quote_asset)
            .checked_add(quote_fees)
            .ok_or(ExchangeError::ArithmeticOverflow)?;
        let base_asset = market.base_asset;
        let quote_asset = market.quote_asset;

        // All fallible settlement happened on touched-account copies. Apply
        // the exact previewed fills; do not traverse and match the book again.
        self.order_books
            .get_mut(&market_id)
            .expect("previewed book exists")
            .apply_previewed_order(order, order_type, &preview);
        for fill in &preview.fills {
            if fill.maker_filled {
                self.order_locations.remove(&fill.maker_order_id);
            }
        }
        if preview.remaining > 0 && order_type.rests() {
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
        for fill in &preview.fills {
            self.sync_order_leaf(fill.maker_order_id);
        }
        if preview.remaining > 0 && order_type.rests() {
            self.sync_order_leaf(order_id);
        }
        self.commit_accounts(staged);
        self.accrue_insurance(base_asset, base_fees)
            .expect("prechecked insurance balance");
        self.accrue_insurance(quote_asset, quote_fees)
            .expect("prechecked insurance balance");
        Ok(trades)
    }

    pub(super) fn cancel_order_atomic(
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
            .ok_or(ExchangeError::MarketNotFound)?;
        if !market.supports_current_spot_settlement() {
            return Err(ExchangeError::UnsupportedMarketSettlement);
        }
        let order = self
            .order_books
            .get(&market_id)
            .ok_or(ExchangeError::OrderBookNotFound)?
            .find_order(order_id, location.side, location.price, account_id)
            .ok_or(ExchangeError::OrderNotFound)?;
        let (asset, amount) = match order.side {
            Side::Buy => (
                market.quote_asset,
                market
                    .quote_atoms(order.price, order.remaining_quantity)
                    .ok_or(ExchangeError::ArithmeticOverflow)?,
            ),
            Side::Sell => (market.base_asset, order.remaining_quantity),
        };
        let mut staged = self.stage_accounts([account_id])?;
        staged.unlock(account_id, asset, amount)?;
        staged.advance_nonce(account_id, nonce);
        self.order_books
            .get_mut(&market_id)
            .expect("validated book exists")
            .cancel_order(order_id, location.side, location.price, account_id)
            .map_err(|_| ExchangeError::OrderNotFound)?;
        self.order_locations.remove(&order_id);
        self.remove_order_leaf(order_id);
        self.commit_accounts(staged);
        Ok(())
    }
    pub(super) fn deposit_perp_collateral(
        &mut self,
        account_id: AccountId,
        market_id: MarketId,
        amount: u64,
        nonce: u64,
    ) -> Result<(), ExchangeError> {
        self.check_nonce(account_id, nonce)?;
        if amount == 0 {
            return Err(ExchangeError::InvalidOrder);
        }
        let (quote_asset, units) = {
            let market = self
                .markets
                .get(&market_id)
                .ok_or(ExchangeError::MarketNotFound)?;
            if !matches!(market.market_type, MarketType::Perp { .. }) || market.quote_asset != USD {
                return Err(ExchangeError::UnsupportedMarketSettlement);
            }
            let units = PerpUnits::new(market.price, market.quantity, USD_DECIMALS)
                .map_err(|_| ExchangeError::InvalidMarketConfig)?;
            (market.quote_asset, units)
        };

        let mut staged_wallet = self.stage_accounts([account_id])?;
        staged_wallet.debit_available(account_id, quote_asset, amount)?;
        if self.margin_mode(account_id) == MarginMode::Cross {
            let next = self
                .cross_collateral(account_id)
                .checked_add(i128::from(amount))
                .ok_or(ExchangeError::ArithmeticOverflow)?;
            staged_wallet.advance_nonce(account_id, nonce);
            self.commit_accounts(staged_wallet);
            self.cross_collateral.insert(account_id, next);
            self.state_tree
                .put(StateKey::CrossCollateral(account_id), &next.to_be_bytes());
            return Ok(());
        }
        let key = (market_id, account_id);
        let mut staged_perp = self
            .perp_accounts
            .get(&key)
            .cloned()
            .unwrap_or_else(|| IsolatedPerpAccount::new(units));
        staged_perp
            .deposit(amount)
            .map_err(|_| ExchangeError::ArithmeticOverflow)?;
        staged_wallet.advance_nonce(account_id, nonce);
        let encoded = canonical::encode_perp_account(&staged_perp);
        self.commit_accounts(staged_wallet);
        self.state_tree
            .put(StateKey::PerpAccount(market_id, account_id), &encoded);
        self.store_perp_account(key, staged_perp);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn balance_mut(
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

    #[cfg(test)]
    pub(super) fn debit_available(
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

    #[cfg(test)]
    pub(super) fn credit_available(
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

    #[cfg(test)]
    pub(super) fn lock(
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

    #[cfg(test)]
    pub(super) fn spend_locked(
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

    #[cfg(test)]
    pub(super) fn unlock(
        &mut self,
        account: AccountId,
        asset: AssetId,
        amount: u64,
    ) -> Result<(), ExchangeError> {
        self.spend_locked(account, asset, amount)?;
        self.credit_available(account, asset, amount)
    }

    pub(super) fn check_nonce(&self, account: AccountId, nonce: u64) -> Result<(), ExchangeError> {
        let account = self
            .accounts
            .get(&account)
            .ok_or(ExchangeError::AccountNotFound)?;
        if !account.accepts_nonce(nonce) {
            return Err(ExchangeError::InvalidNonce);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn advance_nonce(&mut self, account: AccountId, nonce: u64) {
        self.accounts
            .get_mut(&account)
            .expect("validated account")
            .consume_nonce(nonce);
    }

    #[cfg(test)]
    pub(super) fn transfer(
        &mut self,
        from: AccountId,
        to: AccountId,
        asset: AssetId,
        amount: u64,
        nonce: u64,
    ) -> Result<(), ExchangeError> {
        self.check_nonce(from, nonce)?;
        to.public_key()
            .map_err(|_| ExchangeError::AccountNotFound)?;
        if amount == 0 {
            return Err(ExchangeError::InvalidOrder);
        }
        self.debit_available(from, asset, amount)?;
        self.accounts.entry(to).or_insert_with(|| Account {
            id: to,
            nonce: 0,
            used_nonces: [0; 2],
            balances: Default::default(),
        });
        self.credit_available(to, asset, amount)?;
        self.advance_nonce(from, nonce);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn place_order(
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
        if !market.supports_current_spot_settlement() {
            return Err(ExchangeError::UnsupportedMarketSettlement);
        }
        if !market.price.accepts(price) || !market.quantity.accepts(quantity) {
            return Err(ExchangeError::InvalidOrder);
        }
        let reserve = match side {
            Side::Buy => market
                .quote_atoms(price, quantity)
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
            let trade_value = market
                .quote_atoms(fill.price, fill.quantity)
                .ok_or(ExchangeError::ArithmeticOverflow)?;
            match side {
                Side::Buy => {
                    let reserved = market
                        .quote_atoms(price, fill.quantity)
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
            if order_type.rests() {
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
                    Side::Buy => market
                        .quote_atoms(price, result.remaining)
                        .ok_or(ExchangeError::ArithmeticOverflow)?,
                    Side::Sell => result.remaining,
                };
                self.unlock(account_id, reserve_asset, release)?;
            }
        }
        self.advance_nonce(account_id, nonce);
        Ok(trades)
    }

    #[cfg(test)]
    pub(super) fn cancel_order(
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
        if !market.supports_current_spot_settlement() {
            return Err(ExchangeError::UnsupportedMarketSettlement);
        }
        let order = self
            .order_books
            .get_mut(&market_id)
            .ok_or(ExchangeError::OrderBookNotFound)?
            .cancel_order(order_id, location.side, location.price, account_id)
            .map_err(|_| ExchangeError::OrderNotFound)?;
        let (asset, amount) = match order.side {
            Side::Buy => (
                market.quote_asset,
                market
                    .quote_atoms(order.price, order.remaining_quantity)
                    .ok_or(ExchangeError::ArithmeticOverflow)?,
            ),
            Side::Sell => (market.base_asset, order.remaining_quantity),
        };
        self.unlock(account_id, asset, amount)?;
        self.order_locations.remove(&order_id);
        self.advance_nonce(account_id, nonce);
        Ok(())
    }
}
