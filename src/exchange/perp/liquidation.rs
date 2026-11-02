//! Deterministic book-first partial liquidation. Validation runs against staged
//! positions and the affected books; unrelated exchange accounts are never cloned.
use super::orders::map_perp_error;
use super::{IsolatedPerpAccount, MarginMode, PerpRiskParams};
use crate::exchange::fees::fee_atoms;
use crate::exchange::perp_units::{PerpUnits, USD_DECIMALS};
use crate::exchange::*;
use std::collections::{BTreeSet, HashMap};

impl ExchangeState {
    pub fn bad_debt_usd(&self) -> u64 {
        self.bad_debt_usd
    }

    pub(in crate::exchange) fn liquidate_perp(
        &mut self,
        liquidator: AccountId,
        target: AccountId,
        market_id: MarketId,
        quantity: u64,
        nonce: u64,
    ) -> Result<Vec<ExecutedTrade>, ExchangeError> {
        if liquidator == target {
            return Err(ExchangeError::InvalidOrder);
        }
        self.check_nonce(liquidator, nonce)?;
        let market = self
            .markets
            .get(&market_id)
            .ok_or(ExchangeError::MarketNotFound)?
            .clone();
        let MarketType::Perp { max_leverage } = market.market_type else {
            return Err(ExchangeError::UnsupportedMarketSettlement);
        };
        if market.quote_asset != USD {
            return Err(ExchangeError::UnsupportedMarketSettlement);
        }
        let mark = self
            .oracle_mark(OraclePair::new(market.base_asset, market.quote_asset))
            .ok_or(ExchangeError::OracleUnavailable)?;
        let price = market
            .price
            .round_e8_to_tick(mark.index_e8)
            .map_err(|_| ExchangeError::InvalidMarketConfig)?;
        let units = PerpUnits::new(market.price, market.quantity, USD_DECIMALS)
            .map_err(|_| ExchangeError::InvalidMarketConfig)?;
        let mut target_perp = self
            .perp_accounts
            .get(&(market_id, target))
            .cloned()
            .ok_or(ExchangeError::InvalidOrder)?;
        let position = target_perp.position().size();
        if position == 0 || !market.quantity.accepts(quantity) {
            return Err(ExchangeError::InvalidOrder);
        }
        let abs = u64::try_from(position.unsigned_abs())
            .map_err(|_| ExchangeError::ArithmeticOverflow)?;
        let lot = market.quantity.step;
        let half = ((abs / 2) / lot) * lot;
        if quantity > if half == 0 { abs } else { half } {
            return Err(ExchangeError::InvalidOrder);
        }
        let risk = PerpRiskParams {
            initial_margin_bps: 10_000_u64.div_ceil(u64::from(max_leverage)),
            maintenance_margin_bps: market.maintenance_margin_bps,
        };
        let liquidatable = if self.margin_mode(target) == MarginMode::Cross {
            let (equity, _, maintenance) =
                self.cross_risk(target, self.cross_collateral(target), None)?;
            maintenance > 0 && equity < maintenance
        } else {
            target_perp
                .below_maintenance(price, &risk)
                .map_err(map_perp_error)?
        };
        if !liquidatable {
            return Err(ExchangeError::InvalidOrder);
        }

        // The owner index avoids a full order-book scan. Books are staged only
        // for markets that contain target orders or the liquidation market.
        let owned = self
            .perp_owned_orders
            .get(&target)
            .cloned()
            .unwrap_or_default();
        if owned.len() > limits::MAX_PERP_ORDERS_PER_ACCOUNT {
            return Err(ExchangeError::WorkLimit);
        }
        self.charge_work(owned.len())?;
        let mut books = HashMap::<MarketId, OrderBook>::new();
        books.insert(
            market_id,
            self.order_books
                .get(&market_id)
                .ok_or(ExchangeError::OrderBookNotFound)?
                .clone(),
        );
        let mut resting = HashMap::<(MarketId, AccountId), RestingExposure>::new();
        let mut changed_orders = BTreeSet::<OrderId>::new();
        let mut removed_orders = Vec::<(OrderId, AccountId)>::new();
        for order_id in owned {
            let location = self
                .order_locations
                .get(&order_id)
                .ok_or(ExchangeError::OrderNotFound)?;
            let book = books
                .entry(location.market)
                .or_insert_with(|| self.order_books[&location.market].clone());
            let order = book
                .find_order(order_id, location.side, location.price, target)
                .ok_or(ExchangeError::OrderNotFound)?
                .clone();
            let exposure = resting.entry((location.market, target)).or_insert_with(|| {
                self.perp_resting_exposure
                    .get(&(location.market, target))
                    .copied()
                    .unwrap_or_default()
            });
            exposure.change(order.side, order.remaining_quantity, false)?;
            book.cancel_order(order_id, location.side, location.price, target)
                .map_err(map_order_error)?;
            changed_orders.insert(order_id);
            removed_orders.push((order_id, target));
        }

        let target_side = if position > 0 { Side::Sell } else { Side::Buy };
        let maker_side = if position > 0 { Side::Buy } else { Side::Sell };
        let mark_u128 = u128::from(price);
        let step = u128::from(market.price.step);
        let slippage = u128::from(market.max_liquidation_slippage_bps);
        let cap_numerator = if target_side == Side::Sell {
            mark_u128.checked_mul(10_000 - slippage)
        } else {
            mark_u128.checked_mul(10_000 + slippage)
        }
        .ok_or(ExchangeError::ArithmeticOverflow)?;
        let cap = if target_side == Side::Sell {
            let denominator = 10_000_u128
                .checked_mul(step)
                .ok_or(ExchangeError::ArithmeticOverflow)?;
            let rounded = cap_numerator
                .checked_add(denominator - 1)
                .ok_or(ExchangeError::ArithmeticOverflow)?;
            (rounded / denominator)
                .checked_mul(step)
                .ok_or(ExchangeError::ArithmeticOverflow)?
        } else {
            (cap_numerator / (10_000 * step)) * step
        };
        let limit = u64::try_from(cap.max(step)).map_err(|_| ExchangeError::ArithmeticOverflow)?;
        let taker_order_id = self.next_order_id;
        let next_order_id = taker_order_id
            .checked_add(1)
            .ok_or(ExchangeError::ArithmeticOverflow)?;
        let mut remaining = quantity;
        let mut trades = Vec::new();
        let mut maker_fee_total = 0u64;
        let mut executed_notional = 0u64;
        let mut target_pool = self.cross_collateral(target);
        let mut staged_perps = HashMap::<AccountId, IsolatedPerpAccount>::new();
        let mut staged_pools = HashMap::<AccountId, i128>::new();

        let mut examined = 0usize;
        let mut budget_limited = false;
        while remaining > 0 {
            if examined >= limits::MAX_FILLS_PER_ORDER {
                budget_limited = true;
                break;
            }
            let order = Order {
                id: taker_order_id,
                account: target,
                market: market_id,
                side: target_side,
                price: limit,
                original_quantity: remaining,
                remaining_quantity: remaining,
            };
            let Some(fill) = books[&market_id]
                .preview_first_fill(&order)
                .map_err(map_order_error)?
            else {
                break;
            };
            let maker = fill.maker_account;
            let risk_work = if self.margin_mode(maker) == MarginMode::Cross {
                self.perp_markets_by_account
                    .get(&maker)
                    .map_or(0, |markets| markets.len())
                    + 1
            } else {
                0
            };
            if self.execution_work.get().saturating_add(4 + risk_work)
                > limits::MAX_TRANSACTION_WORK
            {
                budget_limited = true;
                break;
            }
            self.charge_work(4)?;
            examined += 1;
            let maker_before = staged_perps.get(&maker).cloned().unwrap_or_else(|| {
                self.perp_accounts
                    .get(&(market_id, maker))
                    .cloned()
                    .unwrap_or_else(|| IsolatedPerpAccount::new(units))
            });
            let old_exposure = resting
                .get(&(market_id, maker))
                .copied()
                .unwrap_or_else(|| {
                    self.perp_resting_exposure
                        .get(&(market_id, maker))
                        .copied()
                        .unwrap_or_default()
                });
            let mut next_exposure = old_exposure;
            next_exposure.change(maker_side, fill.quantity, false)?;
            let mut next_maker = maker_before.clone();
            let mut next_target = target_perp.clone();
            let mut next_target_pool = target_pool;
            let mut next_maker_pool = staged_pools
                .get(&maker)
                .copied()
                .unwrap_or_else(|| self.cross_collateral(maker));
            let notional = u64::try_from(
                units
                    .notional(fill.price, fill.quantity)
                    .map_err(|_| ExchangeError::ArithmeticOverflow)?,
            )
            .map_err(|_| ExchangeError::ArithmeticOverflow)?;
            let maker_fee = fee_atoms(notional, market.fees.maker_bps)?;
            let maker_result = (|| -> Result<(), ExchangeError> {
                if self.margin_mode(maker) == MarginMode::Cross {
                    let realized = next_maker
                        .apply_cross_fill(maker_side, fill.quantity, fill.price)
                        .map_err(map_perp_error)?;
                    next_maker_pool = next_maker_pool
                        .checked_add(realized)
                        .and_then(|pool| pool.checked_sub(i128::from(maker_fee)))
                        .ok_or(ExchangeError::ArithmeticOverflow)?;
                    let (equity, initial, _) = self.cross_risk(
                        maker,
                        next_maker_pool,
                        Some((market_id, &next_maker, next_exposure)),
                    )?;
                    if next_maker_pool < 0 || equity < initial {
                        return Err(ExchangeError::InsufficientBalance);
                    }
                } else {
                    next_maker
                        .apply_fill(maker_side, fill.quantity, fill.price, price, &risk)
                        .map_err(map_perp_error)?;
                    next_maker
                        .debit_fee(i128::from(maker_fee))
                        .map_err(map_perp_error)?;
                }
                Ok(())
            })();
            if let Err(error) = maker_result {
                if !matches!(error, ExchangeError::InsufficientBalance) {
                    return Err(error);
                }
                // A stale under-margined maker must not block liquidation.
                let location = self
                    .order_locations
                    .get(&fill.maker_order_id)
                    .ok_or(ExchangeError::OrderNotFound)?;
                let leftover = books[&market_id]
                    .find_order(fill.maker_order_id, location.side, location.price, maker)
                    .ok_or(ExchangeError::OrderNotFound)?
                    .remaining_quantity;
                let exposure = resting.entry((market_id, maker)).or_insert(old_exposure);
                exposure.change(maker_side, leftover, false)?;
                books
                    .get_mut(&market_id)
                    .expect("staged book")
                    .cancel_order(fill.maker_order_id, location.side, location.price, maker)
                    .map_err(map_order_error)?;
                changed_orders.insert(fill.maker_order_id);
                removed_orders.push((fill.maker_order_id, maker));
                continue;
            }
            if self.margin_mode(target) == MarginMode::Cross {
                let realized = next_target
                    .apply_cross_fill(target_side, fill.quantity, fill.price)
                    .map_err(map_perp_error)?;
                next_target_pool = next_target_pool
                    .checked_add(realized)
                    .ok_or(ExchangeError::ArithmeticOverflow)?;
            } else {
                next_target
                    .apply_fill(target_side, fill.quantity, fill.price, price, &risk)
                    .map_err(map_perp_error)?;
            }
            let one = orderbook::MatchResult {
                fills: vec![fill.clone()],
                remaining: 0,
            };
            let one_order = Order {
                original_quantity: fill.quantity,
                remaining_quantity: fill.quantity,
                ..order
            };
            books
                .get_mut(&market_id)
                .expect("staged book")
                .apply_previewed_order(one_order, OrderType::IOC, &one);
            target_perp = next_target;
            target_pool = next_target_pool;
            staged_perps.insert(maker, next_maker);
            if self.margin_mode(maker) == MarginMode::Cross {
                staged_pools.insert(maker, next_maker_pool);
            }
            resting.insert((market_id, maker), next_exposure);
            if fill.maker_filled {
                removed_orders.push((fill.maker_order_id, maker));
            }
            changed_orders.insert(fill.maker_order_id);
            maker_fee_total = maker_fee_total
                .checked_add(maker_fee)
                .ok_or(ExchangeError::ArithmeticOverflow)?;
            executed_notional = executed_notional
                .checked_add(notional)
                .ok_or(ExchangeError::ArithmeticOverflow)?;
            remaining -= fill.quantity;
            trades.push(ExecutedTrade {
                market_id,
                price: fill.price,
                quantity: fill.quantity,
                aggressor_side: target_side,
                maker_order_id: fill.maker_order_id,
                maker_account: maker,
                taker_order_id,
                taker_account: target,
            });
        }

        // Direct transfer is the backstop. It is optional if the book already
        // made progress and the keeper lacks margin for the remainder.
        if remaining > 0 && !budget_limited {
            let risk_work = if self.margin_mode(liquidator) == MarginMode::Cross {
                self.perp_markets_by_account
                    .get(&liquidator)
                    .map_or(0, |markets| markets.len())
                    + 1
            } else {
                0
            };
            if self.execution_work.get().saturating_add(risk_work) > limits::MAX_TRANSACTION_WORK {
                budget_limited = true;
            }
        }
        if remaining > 0 && !budget_limited {
            let mut next_liquidator = staged_perps.get(&liquidator).cloned().unwrap_or_else(|| {
                self.perp_accounts
                    .get(&(market_id, liquidator))
                    .cloned()
                    .unwrap_or_else(|| IsolatedPerpAccount::new(units))
            });
            let mut next_target = target_perp.clone();
            let mut next_target_pool = target_pool;
            let mut liquidator_pool = staged_pools
                .get(&liquidator)
                .copied()
                .unwrap_or_else(|| self.cross_collateral(liquidator));
            let backstop = (|| -> Result<(), ExchangeError> {
                if self.margin_mode(liquidator) == MarginMode::Cross {
                    let realized = next_liquidator
                        .apply_cross_fill(maker_side, remaining, price)
                        .map_err(map_perp_error)?;
                    liquidator_pool = liquidator_pool
                        .checked_add(realized)
                        .ok_or(ExchangeError::ArithmeticOverflow)?;
                    let (equity, initial, _) = self.cross_risk(
                        liquidator,
                        liquidator_pool,
                        Some((
                            market_id,
                            &next_liquidator,
                            resting
                                .get(&(market_id, liquidator))
                                .copied()
                                .unwrap_or_else(|| {
                                    self.perp_resting_exposure
                                        .get(&(market_id, liquidator))
                                        .copied()
                                        .unwrap_or_default()
                                }),
                        )),
                    )?;
                    if liquidator_pool < 0 || equity < initial {
                        return Err(ExchangeError::InsufficientBalance);
                    }
                } else {
                    next_liquidator
                        .apply_fill(maker_side, remaining, price, price, &risk)
                        .map_err(map_perp_error)?;
                }
                if self.margin_mode(target) == MarginMode::Cross {
                    let realized = next_target
                        .apply_cross_fill(target_side, remaining, price)
                        .map_err(map_perp_error)?;
                    next_target_pool = next_target_pool
                        .checked_add(realized)
                        .ok_or(ExchangeError::ArithmeticOverflow)?;
                } else {
                    next_target
                        .apply_fill(target_side, remaining, price, price, &risk)
                        .map_err(map_perp_error)?;
                }
                Ok(())
            })();
            match backstop {
                Ok(()) => {
                    let notional = u64::try_from(
                        units
                            .notional(price, remaining)
                            .map_err(|_| ExchangeError::ArithmeticOverflow)?,
                    )
                    .map_err(|_| ExchangeError::ArithmeticOverflow)?;
                    executed_notional = executed_notional
                        .checked_add(notional)
                        .ok_or(ExchangeError::ArithmeticOverflow)?;
                    target_perp = next_target;
                    target_pool = next_target_pool;
                    staged_perps.insert(liquidator, next_liquidator);
                    if self.margin_mode(liquidator) == MarginMode::Cross {
                        staged_pools.insert(liquidator, liquidator_pool);
                    }
                }
                Err(error)
                    if !trades.is_empty()
                        && matches!(error, ExchangeError::InsufficientBalance) => {}
                Err(error) => return Err(error),
            }
        }
        // Preserve bounded cleanup/progress so a wall of invalid makers
        // cannot force every keeper attempt to roll back forever. No fee or
        // reward is earned without executed notional; retry in the next tx.
        if executed_notional == 0 && !(budget_limited && !removed_orders.is_empty()) {
            return Err(ExchangeError::InvalidOrder);
        }
        let insurance_fee = fee_atoms(executed_notional, market.fees.liquidation_bps)?;
        let reward_fee = fee_atoms(executed_notional, market.fees.keeper_reward_bps)?;
        let available = if self.margin_mode(target) == MarginMode::Cross {
            target_pool.max(0)
        } else {
            target_perp.collateral().max(0)
        };
        let insurance_collected = available.min(i128::from(insurance_fee));
        let reward_collected = (available - insurance_collected).min(i128::from(reward_fee));
        let charge = insurance_collected
            .checked_add(reward_collected)
            .ok_or(ExchangeError::ArithmeticOverflow)?;
        if self.margin_mode(target) == MarginMode::Cross {
            target_pool = target_pool
                .checked_sub(charge)
                .ok_or(ExchangeError::ArithmeticOverflow)?;
        } else {
            target_perp
                .adjust_collateral(-charge)
                .map_err(map_perp_error)?;
        }
        let insurance_next = self
            .insurance_balance(USD)
            .checked_add(maker_fee_total)
            .and_then(|value| value.checked_add(u64::try_from(insurance_collected).ok()?))
            .ok_or(ExchangeError::ArithmeticOverflow)?;
        let mut debt_next = self.bad_debt_usd;
        let target_flat = if self.margin_mode(target) == MarginMode::Cross {
            self.perp_markets_by_account
                .get(&target)
                .into_iter()
                .flatten()
                .all(|&id| {
                    id == market_id || self.perp_accounts[&(id, target)].position().size() == 0
                })
                && target_perp.position().size() == 0
        } else {
            target_perp.position().size() == 0
        };
        let mut insurance_next = insurance_next;
        if target_flat {
            let deficit = if self.margin_mode(target) == MarginMode::Cross {
                target_pool.saturating_neg().max(0)
            } else {
                target_perp.collateral().saturating_neg().max(0)
            };
            if deficit > 0 {
                let deficit =
                    u64::try_from(deficit).map_err(|_| ExchangeError::ArithmeticOverflow)?;
                let covered = insurance_next.min(deficit);
                insurance_next -= covered;
                debt_next = debt_next
                    .checked_add(deficit - covered)
                    .ok_or(ExchangeError::ArithmeticOverflow)?;
                if self.margin_mode(target) == MarginMode::Cross {
                    target_pool = 0;
                } else {
                    target_perp
                        .adjust_collateral(i128::from(deficit))
                        .map_err(map_perp_error)?;
                }
            }
        }
        let mut wallet = self.stage_accounts([liquidator])?;
        wallet.advance_nonce(liquidator, nonce);
        let reward =
            u64::try_from(reward_collected).map_err(|_| ExchangeError::ArithmeticOverflow)?;
        let liquidator_wallet = wallet.accounts.get_mut(&liquidator).expect("staged wallet");
        let balance = liquidator_wallet.balances.entry(USD).or_insert(Balance {
            available: 0,
            locked: 0,
        });
        balance.available = balance
            .available
            .checked_add(reward)
            .ok_or(ExchangeError::ArithmeticOverflow)?;

        // No fallible validation follows this point. Replace only affected
        // maps and recompute their corresponding Merkle leaves.
        for (id, book) in books {
            self.order_books.insert(id, book);
        }
        for (id, owner) in removed_orders {
            self.order_locations.remove(&id);
            if let Some(orders) = self.perp_owned_orders.get_mut(&owner) {
                orders.remove(&id);
                if orders.is_empty() {
                    self.perp_owned_orders.remove(&owner);
                }
            }
        }
        for id in changed_orders {
            self.sync_order_leaf(id);
        }
        if !trades.is_empty() {
            self.next_order_id = next_order_id;
            self.state_tree.put(
                StateKey::NextOrderId,
                &canonical::encode_next_order_id(self.next_order_id),
            );
        }
        self.store_perp_account((market_id, target), target_perp.clone());
        self.state_tree.put(
            StateKey::PerpAccount(market_id, target),
            &canonical::encode_perp_account(&target_perp),
        );
        for (owner, account) in staged_perps {
            self.state_tree.put(
                StateKey::PerpAccount(market_id, owner),
                &canonical::encode_perp_account(&account),
            );
            self.store_perp_account((market_id, owner), account);
        }
        for ((id, owner), exposure) in resting {
            if exposure.is_empty() {
                self.perp_resting_exposure.remove(&(id, owner));
            } else {
                self.perp_resting_exposure.insert((id, owner), exposure);
            }
        }
        if self.margin_mode(target) == MarginMode::Cross {
            staged_pools.insert(target, target_pool);
        }
        for (owner, pool) in staged_pools {
            self.cross_collateral.insert(owner, pool);
            self.state_tree
                .put(StateKey::CrossCollateral(owner), &pool.to_be_bytes());
        }
        if insurance_next == 0 {
            self.insurance_assets.remove(&USD);
            self.state_tree.remove(StateKey::InsuranceAsset(USD));
        } else {
            self.insurance_assets.insert(USD, insurance_next);
            self.state_tree
                .put(StateKey::InsuranceAsset(USD), &insurance_next.to_be_bytes());
        }
        self.bad_debt_usd = debt_next;
        if debt_next != 0 {
            self.state_tree
                .put(StateKey::BadDebtUsd, &debt_next.to_be_bytes());
        }
        self.commit_accounts(wallet);
        Ok(trades)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::{OracleObservation, OracleUpdate, Venue, index_from_observations};

    fn mark(state: &mut ExchangeState, bid: &str, ask: &str) {
        let pair = OraclePair::new(BTC, USD);
        let observations = [
            (Venue::Binance, "BTCUSDT"),
            (Venue::Okx, "BTC-USDT"),
            (Venue::Coinbase, "BTC-USD"),
        ]
        .map(|(venue, symbol)| OracleObservation {
            venue,
            symbol: symbol.into(),
            bid: bid.into(),
            ask: ask.into(),
            source_sequence: None,
            source_time: None,
        });
        state
            .apply_oracle_update(&OracleUpdate {
                pair,
                index_e8: index_from_observations(pair, &observations).unwrap(),
                observations: observations.to_vec(),
            })
            .unwrap();
    }

    fn setup(cross_target: bool) -> ExchangeState {
        let mut state = ExchangeState::default();
        state
            .create_perp_market(
                1,
                BTC,
                USD,
                DecimalStep::new(1, 1).unwrap(),
                DecimalStep::new(4, 1).unwrap(),
                10,
            )
            .unwrap();
        mark(&mut state, "99", "101");
        if cross_target {
            state
                .execute(&Transaction::SetMarginMode {
                    account_id: 0.into(),
                    mode: MarginMode::Cross,
                    nonce: 0,
                })
                .unwrap();
        }
        for id in 0..3 {
            state
                .execute(&Transaction::DepositPerpCollateral {
                    account_id: id.into(),
                    market_id: 1,
                    amount: if id == 2 { 100_000_000 } else { 20_000_000 },
                    nonce: if cross_target && id == 0 { 1 } else { 0 },
                })
                .unwrap();
        }
        state
            .execute(&Transaction::PlacePerpOrder {
                account_id: 1.into(),
                market_id: 1,
                order_type: OrderType::LO,
                side: Side::Sell,
                price: 1_000,
                quantity: 10_000,
                reduce_only: false,
                nonce: 1,
            })
            .unwrap();
        state
            .execute(&Transaction::PlacePerpOrder {
                account_id: 0.into(),
                market_id: 1,
                order_type: OrderType::FOK,
                side: Side::Buy,
                price: 1_000,
                quantity: 10_000,
                reduce_only: false,
                nonce: if cross_target { 2 } else { 1 },
            })
            .unwrap();
        state
    }

    #[test]
    fn liquidation_requires_maintenance_breach_and_transfers_half_position() {
        let mut state = setup(false);
        let tx = Transaction::LiquidatePerp {
            liquidator: 2.into(),
            target: 0.into(),
            market_id: 1,
            quantity: 5_000,
            nonce: 1,
        };
        let root = state.state_digest();
        assert_eq!(state.execute(&tx), Err(ExchangeError::InvalidOrder));
        assert_eq!(root, state.state_digest());
        mark(&mut state, "79", "81");
        state.execute(&tx).unwrap();
        assert_eq!(state.perp_accounts[&(1, 0.into())].position().size(), 5_000);
        assert_eq!(state.perp_accounts[&(1, 2.into())].position().size(), 5_000);
        assert_eq!(state.get_nonce_for_account(2.into()).unwrap(), 2);
        assert_eq!(state.execute(&tx), Err(ExchangeError::InvalidNonce));
        assert!(state.verify_state_tree());
    }

    #[test]
    fn cross_pool_is_liquidatable_without_touching_spot_wallet() {
        let mut state = setup(true);
        let wallet_before = state.accounts[&AccountId::from(0)].balances[&USD].available;
        mark(&mut state, "79", "81");
        state
            .execute(&Transaction::LiquidatePerp {
                liquidator: 2.into(),
                target: 0.into(),
                market_id: 1,
                quantity: 5_000,
                nonce: 1,
            })
            .unwrap();
        assert_eq!(
            state.accounts[&AccountId::from(0)].balances[&USD].available,
            wallet_before
        );
        assert_eq!(state.cross_collateral(0.into()), 10_000_000);
        assert!(state.verify_state_tree());
    }

    #[test]
    fn book_fill_precedes_keeper_backstop_and_collects_reward() {
        let mut state = setup(false);
        state.markets.get_mut(&1).unwrap().fees.keeper_reward_bps = 5;
        state.rebuild_state_tree();
        mark(&mut state, "79", "81");
        state
            .execute(&Transaction::DepositPerpCollateral {
                account_id: 3.into(),
                market_id: 1,
                amount: 100_000_000,
                nonce: 0,
            })
            .unwrap();
        state
            .execute(&Transaction::PlacePerpOrder {
                account_id: 3.into(),
                market_id: 1,
                order_type: OrderType::LO,
                side: Side::Buy,
                price: 800,
                quantity: 2_000,
                reduce_only: false,
                nonce: 1,
            })
            .unwrap();
        let reward_before = state.accounts[&AccountId::from(2)].balances[&USD].available;
        let trades = state
            .execute_with_receipt(&Transaction::LiquidatePerp {
                liquidator: 2.into(),
                target: 0.into(),
                market_id: 1,
                quantity: 5_000,
                nonce: 1,
            })
            .unwrap();
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].quantity, 2_000);
        assert_eq!(
            state.perp_accounts[&(1, AccountId::from(3))]
                .position()
                .size(),
            2_000
        );
        assert_eq!(
            state.perp_accounts[&(1, AccountId::from(2))]
                .position()
                .size(),
            3_000
        );
        assert_eq!(
            state.perp_accounts[&(1, AccountId::from(0))]
                .position()
                .size(),
            5_000
        );
        assert!(state.accounts[&AccountId::from(2)].balances[&USD].available > reward_before);
        assert!(state.verify_state_tree());
    }

    #[test]
    fn bounded_liquidation_preserves_progress_through_fills_and_invalid_makers() {
        let mut base = setup(false);
        mark(&mut base, "79", "81");
        for owner in [3u64, 4] {
            base.execute(&Transaction::DepositPerpCollateral {
                account_id: owner.into(),
                market_id: 1,
                amount: 100_000_000,
                nonce: 0,
            })
            .unwrap();
            for nonce in 1..=if owner == 3 { 512 } else { 8 } {
                base.execute(&Transaction::PlacePerpOrder {
                    account_id: owner.into(),
                    market_id: 1,
                    order_type: OrderType::LO,
                    side: Side::Buy,
                    price: 800,
                    quantity: 1,
                    reduce_only: false,
                    nonce,
                })
                .unwrap();
            }
        }
        let root = base.state_digest();
        assert_eq!(
            base.execute(&Transaction::PlacePerpOrder {
                account_id: 3.into(),
                market_id: 1,
                order_type: OrderType::LO,
                side: Side::Buy,
                price: 800,
                quantity: 1,
                reduce_only: false,
                nonce: 513
            }),
            Err(ExchangeError::WorkLimit)
        );
        assert_eq!(base.state_digest(), root);
        for invalid_makers in [false, true] {
            let mut state = base.fork();
            if invalid_makers {
                // Test-only corrupt margin situation: a mark move can likewise
                // leave previously valid resting liquidity undercollateralized.
                state
                    .perp_accounts
                    .get_mut(&(1, 3.into()))
                    .unwrap()
                    .adjust_collateral(-100_000_000)
                    .unwrap();
                state.rebuild_state_tree();
            }
            let fills = state
                .execute_with_receipt(&Transaction::LiquidatePerp {
                    liquidator: 2.into(),
                    target: 0.into(),
                    market_id: 1,
                    quantity: 1000,
                    nonce: 1,
                })
                .unwrap();
            assert_eq!(fills.len(), if invalid_makers { 0 } else { 512 });
            assert!(state.account_orders(3.into()).is_empty());
            assert_eq!(state.account_orders(4.into()).len(), 8);
            assert_eq!(state.get_nonce_for_account(2.into()).unwrap(), 2);
            assert!(state.last_execution_work() <= limits::MAX_TRANSACTION_WORK);
            assert!(state.verify_state_tree());
            state
                .execute(&Transaction::LiquidatePerp {
                    liquidator: 2.into(),
                    target: 0.into(),
                    market_id: 1,
                    quantity: 1000,
                    nonce: 2,
                })
                .unwrap();
            assert!(state.account_orders(4.into()).is_empty());
            assert!(state.verify_state_tree());
        }
    }
}
