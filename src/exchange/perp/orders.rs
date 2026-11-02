use super::{IsolatedPerpAccount, PerpError, PerpRiskParams, settle_fill};
use crate::exchange::fees::fee_atoms;
use crate::exchange::perp_units::{PerpUnits, USD_DECIMALS};
use crate::exchange::*;

impl ExchangeState {
    pub(in crate::exchange) fn place_perp_order_atomic(
        &mut self,
        account_id: AccountId,
        market_id: MarketId,
        order_type: OrderType,
        side: Side,
        price: u64,
        quantity: u64,
        nonce: u64,
        mark_price: u64,
    ) -> Result<Vec<ExecutedTrade>, ExchangeError> {
        self.place_perp_order_with_flags(
            account_id, market_id, order_type, side, price, quantity, nonce, mark_price, false,
        )
    }

    pub(in crate::exchange) fn place_perp_order_with_flags(
        &mut self,
        account_id: AccountId,
        market_id: MarketId,
        order_type: OrderType,
        side: Side,
        price: u64,
        quantity: u64,
        nonce: u64,
        mark_price: u64,
        reduce_only: bool,
    ) -> Result<Vec<ExecutedTrade>, ExchangeError> {
        self.check_nonce(account_id, nonce)?;

        let market = self
            .markets
            .get(&market_id)
            .ok_or(ExchangeError::MarketNotFound)?;
        let MarketType::Perp { max_leverage } = market.market_type else {
            return Err(ExchangeError::UnsupportedMarketSettlement);
        };
        if max_leverage == 0 || market.quote_asset != USD {
            return Err(ExchangeError::UnsupportedMarketSettlement);
        }
        if !market.price.accepts(price)
            || !market.price.accepts(mark_price)
            || !market.quantity.accepts(quantity)
        {
            return Err(ExchangeError::InvalidOrder);
        }

        let units = PerpUnits::new(market.price, market.quantity, USD_DECIMALS)
            .map_err(|_| ExchangeError::InvalidMarketConfig)?;
        if reduce_only {
            if order_type.rests() {
                return Err(ExchangeError::InvalidOrder);
            }
            let position = self
                .perp_accounts
                .get(&(market_id, account_id))
                .map(|account| account.position().size())
                .unwrap_or(0);
            let requested = i128::from(quantity);
            let reduces = match side {
                Side::Buy => {
                    position < 0 && position.checked_abs().is_some_and(|size| requested <= size)
                }
                Side::Sell => position > 0 && requested <= position,
            };
            if !reduces {
                return Err(ExchangeError::InvalidOrder);
            }
        }
        let initial_bps = 10_000_u64.div_ceil(u64::from(max_leverage)).max(1);
        let risk = PerpRiskParams {
            initial_margin_bps: initial_bps,
            maintenance_margin_bps: market.maintenance_margin_bps,
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

        let original_book = self
            .order_books
            .get(&market_id)
            .ok_or(ExchangeError::OrderBookNotFound)?;
        let preview = original_book
            .preview_order(&order, order_type)
            .map_err(|error| {
                if matches!(error, OrderBookError::WorkLimit | OrderBookError::KilledFOK) {
                    let _ = self.charge_work(limits::MAX_FILLS_PER_ORDER * 4);
                }
                map_order_error(error)
            })?;

        self.charge_work(preview.fills.len() * 4)?;
        if preview.remaining > 0
            && order_type.rests()
            && self
                .perp_owned_orders
                .get(&account_id)
                .is_some_and(|orders| orders.len() >= limits::MAX_PERP_ORDERS_PER_ACCOUNT)
        {
            return Err(ExchangeError::WorkLimit);
        }
        // Risk checks can fail early and consume work. Their iteration order
        // must therefore be deterministic on every validator.
        let mut staged_perps = std::collections::BTreeMap::new();
        let mut staged_resting = HashMap::new();
        let mut staged_cross = HashMap::new();
        for id in
            std::iter::once(account_id).chain(preview.fills.iter().map(|fill| fill.maker_account))
        {
            staged_perps.entry(id).or_insert_with(|| {
                self.perp_accounts
                    .get(&(market_id, id))
                    .cloned()
                    .unwrap_or_else(|| IsolatedPerpAccount::new(units))
            });
            staged_resting.entry(id).or_insert_with(|| {
                self.perp_resting_exposure
                    .get(&(market_id, id))
                    .copied()
                    .unwrap_or_default()
            });
            if self.margin_mode(id) == MarginMode::Cross {
                staged_cross
                    .entry(id)
                    .or_insert_with(|| self.cross_collateral(id));
            }
        }

        let mut trades = Vec::with_capacity(preview.fills.len());
        let mut fee_total = 0u64;
        for fill in &preview.fills {
            // The preview rejects self-trades. Remove and reinsert each maker
            // so a later fill from the same maker sees its updated position.
            let mut maker = staged_perps
                .remove(&fill.maker_account)
                .expect("maker was staged");
            let taker = staged_perps.get_mut(&account_id).expect("taker was staged");
            let maker_side = match side {
                Side::Buy => Side::Sell,
                Side::Sell => Side::Buy,
            };
            if self.margin_mode(fill.maker_account) == MarginMode::Isolated
                && self.margin_mode(account_id) == MarginMode::Isolated
            {
                settle_fill(
                    &mut maker,
                    taker,
                    maker_side,
                    fill.quantity,
                    fill.price,
                    mark_price,
                    &risk,
                )
                .map_err(map_perp_error)?;
            } else {
                if self.margin_mode(fill.maker_account) == MarginMode::Cross {
                    let realized = maker
                        .apply_cross_fill(maker_side, fill.quantity, fill.price)
                        .map_err(map_perp_error)?;
                    let pool = staged_cross
                        .get_mut(&fill.maker_account)
                        .expect("cross maker staged");
                    *pool = pool
                        .checked_add(realized)
                        .ok_or(ExchangeError::ArithmeticOverflow)?;
                } else {
                    maker
                        .apply_fill(maker_side, fill.quantity, fill.price, mark_price, &risk)
                        .map_err(map_perp_error)?;
                }
                if self.margin_mode(account_id) == MarginMode::Cross {
                    let realized = taker
                        .apply_cross_fill(side, fill.quantity, fill.price)
                        .map_err(map_perp_error)?;
                    let pool = staged_cross
                        .get_mut(&account_id)
                        .expect("cross taker staged");
                    *pool = pool
                        .checked_add(realized)
                        .ok_or(ExchangeError::ArithmeticOverflow)?;
                } else {
                    taker
                        .apply_fill(side, fill.quantity, fill.price, mark_price, &risk)
                        .map_err(map_perp_error)?;
                }
            }
            let notional = units
                .notional(fill.price, fill.quantity)
                .map_err(|_| ExchangeError::ArithmeticOverflow)?;
            let notional =
                u64::try_from(notional).map_err(|_| ExchangeError::ArithmeticOverflow)?;
            let maker_fee = fee_atoms(notional, market.fees.maker_bps)?;
            let taker_fee = fee_atoms(notional, market.fees.taker_bps)?;
            fee_total = fee_total
                .checked_add(maker_fee)
                .and_then(|v| v.checked_add(taker_fee))
                .ok_or(ExchangeError::ArithmeticOverflow)?;
            if self.margin_mode(fill.maker_account) == MarginMode::Cross {
                let pool = staged_cross
                    .get_mut(&fill.maker_account)
                    .expect("cross maker staged");
                *pool = pool
                    .checked_sub(i128::from(maker_fee))
                    .ok_or(ExchangeError::ArithmeticOverflow)?;
                if *pool < 0 {
                    return Err(ExchangeError::InsufficientBalance);
                }
            } else {
                maker
                    .debit_fee(i128::from(maker_fee))
                    .map_err(map_perp_error)?;
            }
            if self.margin_mode(account_id) == MarginMode::Cross {
                let pool = staged_cross
                    .get_mut(&account_id)
                    .expect("cross taker staged");
                *pool = pool
                    .checked_sub(i128::from(taker_fee))
                    .ok_or(ExchangeError::ArithmeticOverflow)?;
                if *pool < 0 {
                    return Err(ExchangeError::InsufficientBalance);
                }
            } else {
                taker
                    .debit_fee(i128::from(taker_fee))
                    .map_err(map_perp_error)?;
            }
            staged_perps.insert(fill.maker_account, maker);

            // A maker's resting order lost exactly this filled quantity.
            let maker_resting = staged_resting
                .get_mut(&fill.maker_account)
                .expect("maker resting total was staged");
            maker_resting.change(maker_side, fill.quantity, false)?;

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
        }

        if preview.remaining > 0 && order_type.rests() {
            let taker_resting = staged_resting
                .get_mut(&account_id)
                .expect("taker was staged");
            taker_resting.change(side, preview.remaining, true)?;
        }
        for (&id, account) in &staged_perps {
            if self.margin_mode(id) == MarginMode::Cross {
                let old_pool = self.cross_collateral(id);
                let (_, old_required, _) = self.cross_risk(id, old_pool, None)?;
                let (equity, required, _) = self.cross_risk(
                    id,
                    staged_cross[&id],
                    Some((market_id, account, staged_resting[&id])),
                )?;
                if required > old_required && equity < required {
                    return Err(ExchangeError::InsufficientBalance);
                }
            } else {
                let before = self
                    .perp_accounts
                    .get(&(market_id, id))
                    .cloned()
                    .unwrap_or_else(|| IsolatedPerpAccount::new(units));
                let old_resting = self
                    .perp_resting_exposure
                    .get(&(market_id, id))
                    .copied()
                    .unwrap_or_default();
                check_exposure_change(
                    &before,
                    old_resting,
                    account,
                    staged_resting[&id],
                    mark_price,
                    units,
                    &risk,
                )?;
            }
        }

        let mut staged_wallet = self.stage_accounts([account_id])?;
        staged_wallet.advance_nonce(account_id, nonce);
        self.insurance_balance(USD)
            .checked_add(fee_total)
            .ok_or(ExchangeError::ArithmeticOverflow)?;

        // All fallible matching and settlement is complete. The book has not
        // changed since preview; mutate only the maker orders in that plan.
        self.order_books
            .get_mut(&market_id)
            .expect("previewed book exists")
            .apply_previewed_order(order, order_type, &preview);
        for fill in &preview.fills {
            if fill.maker_filled {
                self.order_locations.remove(&fill.maker_order_id);
                if let Some(owned) = self.perp_owned_orders.get_mut(&fill.maker_account) {
                    owned.remove(&fill.maker_order_id);
                    if owned.is_empty() {
                        self.perp_owned_orders.remove(&fill.maker_account);
                    }
                }
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
            self.perp_owned_orders
                .entry(account_id)
                .or_default()
                .insert(order_id);
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
        self.commit_accounts(staged_wallet);
        for (id, perp_account) in staged_perps {
            self.state_tree.put(
                StateKey::PerpAccount(market_id, id),
                &canonical::encode_perp_account(&perp_account),
            );
            self.store_perp_account((market_id, id), perp_account);
        }
        for (id, exposure) in staged_resting {
            if exposure.is_empty() {
                self.perp_resting_exposure.remove(&(market_id, id));
            } else {
                self.perp_resting_exposure.insert((market_id, id), exposure);
            }
        }
        for (id, collateral) in staged_cross {
            self.cross_collateral.insert(id, collateral);
            self.state_tree
                .put(StateKey::CrossCollateral(id), &collateral.to_be_bytes());
        }
        self.accrue_insurance(USD, fee_total)
            .expect("prechecked insurance balance");

        Ok(trades)
    }

    /// Cancel a resting perp quote and release its cached margin exposure.
    pub(in crate::exchange) fn cancel_perp_order_atomic(
        &mut self,
        account_id: AccountId,
        market_id: MarketId,
        order_id: OrderId,
        nonce: u64,
    ) -> Result<(), ExchangeError> {
        self.check_nonce(account_id, nonce)?;
        let market = self
            .markets
            .get(&market_id)
            .ok_or(ExchangeError::MarketNotFound)?;
        if !matches!(market.market_type, MarketType::Perp { .. }) || market.quote_asset != USD {
            return Err(ExchangeError::UnsupportedMarketSettlement);
        }
        let location = self
            .order_locations
            .get(&order_id)
            .ok_or(ExchangeError::OrderNotFound)?;
        if location.market != market_id {
            return Err(ExchangeError::OrderNotFound);
        }
        let order = self
            .order_books
            .get(&market_id)
            .ok_or(ExchangeError::OrderBookNotFound)?
            .find_order(order_id, location.side, location.price, account_id)
            .ok_or(ExchangeError::OrderNotFound)?;
        let key = (market_id, account_id);
        let next_resting = self
            .perp_resting_exposure
            .get(&key)
            .copied()
            .unwrap_or_default();
        let mut next_resting = next_resting;
        next_resting.change(order.side, order.remaining_quantity, false)?;
        let mut staged_wallet = self.stage_accounts([account_id])?;
        staged_wallet.advance_nonce(account_id, nonce);

        self.order_books
            .get_mut(&market_id)
            .expect("validated book exists")
            .cancel_order(order_id, location.side, location.price, account_id)
            .expect("validated order still rests in book");
        self.order_locations.remove(&order_id);
        if let Some(owned) = self.perp_owned_orders.get_mut(&account_id) {
            owned.remove(&order_id);
            if owned.is_empty() {
                self.perp_owned_orders.remove(&account_id);
            }
        }
        self.remove_order_leaf(order_id);
        self.commit_accounts(staged_wallet);
        if next_resting.is_empty() {
            self.perp_resting_exposure.remove(&key);
        } else {
            self.perp_resting_exposure.insert(key, next_resting);
        }
        Ok(())
    }
}

pub(in crate::exchange) fn worst_case_notional(
    account: &IsolatedPerpAccount,
    resting: RestingExposure,
    mark_price: u64,
    units: PerpUnits,
) -> Result<i128, ExchangeError> {
    let position = account.position().size();
    let buys = position
        .checked_add(i128::from(resting.buy_qty))
        .ok_or(ExchangeError::ArithmeticOverflow)?;
    let sells = position
        .checked_sub(i128::from(resting.sell_qty))
        .ok_or(ExchangeError::ArithmeticOverflow)?;
    let quantity = buys.unsigned_abs().max(sells.unsigned_abs());
    if quantity == 0 {
        return Ok(0);
    }
    let quantity = u64::try_from(quantity).map_err(|_| ExchangeError::ArithmeticOverflow)?;
    units
        .notional(mark_price, quantity)
        .map_err(|_| ExchangeError::ArithmeticOverflow)
}

fn check_exposure_change(
    before: &IsolatedPerpAccount,
    before_resting: RestingExposure,
    after: &IsolatedPerpAccount,
    after_resting: RestingExposure,
    mark_price: u64,
    units: PerpUnits,
    risk: &PerpRiskParams,
) -> Result<(), ExchangeError> {
    let previous = worst_case_notional(before, before_resting, mark_price, units)?;
    let next = worst_case_notional(after, after_resting, mark_price, units)?;
    if next <= previous {
        return Ok(());
    }
    let required = risk.initial_margin_required(next).map_err(map_perp_error)?;
    if after.equity(mark_price).map_err(map_perp_error)? < required {
        return Err(ExchangeError::InsufficientBalance);
    }
    Ok(())
}

pub(in crate::exchange) fn map_perp_error(error: PerpError) -> ExchangeError {
    match error {
        PerpError::InsufficientCollateral | PerpError::InsufficientMargin => {
            ExchangeError::InsufficientBalance
        }
        PerpError::ArithmeticOverflow => ExchangeError::ArithmeticOverflow,
        _ => ExchangeError::InvalidOrder,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERP_MARKET: MarketId = 1;
    const MARK: u64 = 100_000; // $10,000.0

    fn setup() -> ExchangeState {
        let mut state = ExchangeState::default();
        state
            .create_perp_market(
                PERP_MARKET,
                BTC,
                USD,
                DecimalStep::new(1, 1).unwrap(),
                DecimalStep::new(4, 1).unwrap(),
                10,
            )
            .unwrap();
        state
    }

    #[test]
    fn post_only_and_reduction_preserve_perp_exposure_and_priority() {
        for cross in [false, true] {
            let mut state = setup();
            deposit(&mut state, 0, 10_000_000_000);
            deposit(&mut state, 1, 10_000_000_000);
            if cross {
                // Test fixture migration only; production requires flat/empty.
                state.margin_modes.insert(0.into(), MarginMode::Cross);
                state.cross_collateral.insert(0.into(), 10_000_000_000);
                state.oracle_marks.insert(
                    OraclePair::new(BTC, USD),
                    OracleMark {
                        index_e8: 10_000 * 100_000_000,
                        update_digest: [1; 32],
                    },
                );
                state.rebuild_state_tree();
            }
            state
                .place_perp_order_atomic(
                    0.into(),
                    PERP_MARKET,
                    OrderType::PostOnly,
                    Side::Buy,
                    MARK,
                    2000,
                    1,
                    MARK,
                )
                .unwrap();
            let id = state.account_orders(0.into())[0].id;
            let root = state.state_digest();
            assert_eq!(
                state.place_perp_order_atomic(
                    1.into(),
                    PERP_MARKET,
                    OrderType::PostOnly,
                    Side::Sell,
                    MARK,
                    1000,
                    1,
                    MARK
                ),
                Err(ExchangeError::WouldTakeLiquidity)
            );
            assert_eq!(root, state.state_digest());
            state
                .execute(&Transaction::ReduceOrder {
                    account_id: 0.into(),
                    market_id: PERP_MARKET,
                    order_id: id,
                    remaining_quantity: 1000,
                    nonce: 2,
                })
                .unwrap();
            assert_eq!(state.account_orders(0.into())[0].id, id);
            assert_eq!(
                state.perp_resting_exposure[&(PERP_MARKET, 0.into())].buy_qty,
                1000
            );
            assert_resting_index(&state);
            assert!(state.verify_state_tree());
        }
    }

    fn deposit(state: &mut ExchangeState, id: u64, amount: u64) {
        state
            .execute(&Transaction::DepositPerpCollateral {
                account_id: id.into(),
                market_id: PERP_MARKET,
                amount,
                nonce: 0,
            })
            .unwrap();
    }

    fn assert_resting_index(state: &ExchangeState) {
        let mut rebuilt = HashMap::<(MarketId, AccountId), RestingExposure>::new();
        for (&market_id, book) in &state.order_books {
            let market = &state.markets[&market_id];
            if !matches!(market.market_type, MarketType::Perp { .. }) {
                continue;
            }
            for order in book.bids.values().chain(book.asks.values()).flatten() {
                rebuilt
                    .entry((market_id, order.account))
                    .or_default()
                    .change(order.side, order.remaining_quantity, true)
                    .unwrap();
            }
        }
        assert_eq!(state.perp_resting_exposure, rebuilt.into());
    }

    #[test]
    fn fills_from_a_b_a_keep_the_first_makers_updated_position() {
        let mut state = setup();
        for id in [0, 1, 2] {
            deposit(&mut state, id, 10_000_000_000);
        }

        for (id, nonce) in [(0, 1), (1, 1), (0, 2)] {
            state
                .place_perp_order_atomic(
                    id.into(),
                    PERP_MARKET,
                    OrderType::LO,
                    Side::Sell,
                    MARK,
                    2_000,
                    nonce,
                    MARK,
                )
                .unwrap();
        }

        let trades = state
            .place_perp_order_atomic(
                2.into(),
                PERP_MARKET,
                OrderType::FOK,
                Side::Buy,
                MARK,
                6_000,
                1,
                MARK,
            )
            .unwrap();

        assert_eq!(trades.len(), 3);
        assert_eq!(
            trades
                .iter()
                .map(|trade| trade.maker_account)
                .collect::<Vec<_>>(),
            vec![0.into(), 1.into(), 0.into()]
        );
        assert_eq!(
            state.perp_accounts[&(PERP_MARKET, 0.into())]
                .position()
                .size(),
            -4_000
        );
        assert_eq!(
            state.perp_accounts[&(PERP_MARKET, 1.into())]
                .position()
                .size(),
            -2_000
        );
        assert_eq!(
            state.perp_accounts[&(PERP_MARKET, 2.into())]
                .position()
                .size(),
            6_000
        );
        assert!(state.order_books[&PERP_MARKET].asks.is_empty());
        assert_resting_index(&state);
        assert!(state.verify_state_tree());
    }

    #[test]
    fn failed_taker_margin_keeps_book_nonce_positions_and_root() {
        let mut state = setup();
        deposit(&mut state, 0, 10_000_000_000);
        state
            .place_perp_order_atomic(
                0.into(),
                PERP_MARKET,
                OrderType::LO,
                Side::Sell,
                MARK,
                10_000,
                1,
                MARK,
            )
            .unwrap();

        let root = state.state_digest();
        let next_order_id = state.next_order_id();
        let maker_before = state.perp_accounts[&(PERP_MARKET, 0.into())].clone();

        assert_eq!(
            state.place_perp_order_atomic(
                1.into(),
                PERP_MARKET,
                OrderType::FOK,
                Side::Buy,
                MARK,
                10_000,
                0,
                MARK,
            ),
            Err(ExchangeError::InsufficientBalance),
        );
        assert_eq!(state.state_digest(), root);
        assert_eq!(state.next_order_id(), next_order_id);
        assert_eq!(state.get_nonce_for_account(1.into()), Ok(0));
        assert_eq!(state.perp_accounts[&(PERP_MARKET, 0.into())], maker_before);
        assert_eq!(state.order_books[&PERP_MARKET].asks[&MARK].len(), 1);
        assert_resting_index(&state);
        assert!(state.verify_state_tree());
    }

    #[test]
    fn resting_orders_cannot_reuse_the_same_margin() {
        let mut state = setup();
        deposit(&mut state, 0, 1_000_000_000); // $1,000: margin for 1 BTC at 10x.
        state
            .place_perp_order_atomic(
                0.into(),
                PERP_MARKET,
                OrderType::LO,
                Side::Buy,
                MARK,
                10_000,
                1,
                MARK,
            )
            .unwrap();

        let root = state.state_digest();
        assert_eq!(
            state.place_perp_order_atomic(
                0.into(),
                PERP_MARKET,
                OrderType::LO,
                Side::Buy,
                MARK - 1,
                10_000,
                2,
                MARK,
            ),
            Err(ExchangeError::InsufficientBalance),
        );
        assert_eq!(state.state_digest(), root);
        assert_eq!(state.get_nonce_for_account(0.into()), Ok(2));
        assert_eq!(state.order_books[&PERP_MARKET].bids[&MARK].len(), 1);
        assert_resting_index(&state);
        assert!(state.verify_state_tree());
    }

    #[test]
    fn partial_fill_and_cancel_update_resting_notional() {
        let mut state = setup();
        deposit(&mut state, 0, 10_000_000_000);
        deposit(&mut state, 1, 10_000_000_000);
        state
            .place_perp_order_atomic(
                0.into(),
                PERP_MARKET,
                OrderType::LO,
                Side::Sell,
                MARK,
                4_000,
                1,
                MARK,
            )
            .unwrap();
        assert_resting_index(&state);
        let order_id = state.next_order_id() - 1;
        state
            .place_perp_order_atomic(
                1.into(),
                PERP_MARKET,
                OrderType::IOC,
                Side::Buy,
                MARK,
                2_000,
                1,
                MARK,
            )
            .unwrap();
        assert_eq!(
            state.perp_resting_exposure[&(PERP_MARKET, 0.into())].sell_qty,
            2_000
        );
        assert_resting_index(&state);
        state
            .execute(&Transaction::CancelOrder {
                account_id: 0.into(),
                market_id: PERP_MARKET,
                order_id,
                nonce: 2,
            })
            .unwrap();
        assert!(
            !state
                .perp_resting_exposure
                .contains_key(&(PERP_MARKET, 0.into()))
        );
        assert_resting_index(&state);
        assert!(state.verify_state_tree());
    }
}
