//! Quantity-only amendments preserve order ID and FIFO priority. Price/side
//! changes still use atomic cancel-and-place. All fallible work precedes writes.
use super::*;

impl ExchangeState {
    pub(in crate::exchange) fn reduce_order_atomic(
        &mut self,
        owner: AccountId,
        market_id: MarketId,
        id: OrderId,
        remaining: u64,
        nonce: u64,
    ) -> Result<(), ExchangeError> {
        self.check_nonce(owner, nonce)?;
        let location = self
            .order_locations
            .get(&id)
            .ok_or(ExchangeError::OrderNotFound)?
            .clone();
        if location.market != market_id {
            return Err(ExchangeError::OrderNotFound);
        }
        let market = self
            .markets
            .get(&market_id)
            .ok_or(ExchangeError::MarketNotFound)?;
        let order = self
            .order_books
            .get(&market_id)
            .ok_or(ExchangeError::OrderBookNotFound)?
            .find_order(id, location.side, location.price, owner)
            .ok_or(ExchangeError::OrderNotFound)?;
        if remaining == 0
            || remaining >= order.remaining_quantity
            || !market.quantity.accepts(remaining)
        {
            return Err(ExchangeError::InvalidOrder);
        }
        let reduction = order.remaining_quantity - remaining;
        let mut wallets = self.stage_accounts([owner])?;
        let perp = matches!(market.market_type, MarketType::Perp { .. });
        let exposure = if perp {
            let mut exposure = self
                .perp_resting_exposure
                .get(&(market_id, owner))
                .copied()
                .unwrap_or_default();
            // worst-case exposure includes both endpoints; reducing one side
            // cannot enlarge that interval. No price-dependent risk waiver.
            exposure.change(order.side, reduction, false)?;
            Some(exposure)
        } else {
            if !market.supports_current_spot_settlement() {
                return Err(ExchangeError::UnsupportedMarketSettlement);
            }
            let (asset, released) = match order.side {
                Side::Sell => (market.base_asset, reduction),
                Side::Buy => {
                    let before = market
                        .quote_atoms(order.price, order.remaining_quantity)
                        .ok_or(ExchangeError::ArithmeticOverflow)?;
                    let after = market
                        .quote_atoms(order.price, remaining)
                        .ok_or(ExchangeError::ArithmeticOverflow)?;
                    (
                        market.quote_asset,
                        before
                            .checked_sub(after)
                            .ok_or(ExchangeError::ArithmeticOverflow)?,
                    )
                }
            };
            wallets.unlock(owner, asset, released)?;
            None
        };
        wallets.advance_nonce(owner, nonce);
        self.order_books
            .get_mut(&market_id)
            .expect("validated book")
            .reduce_order(id, location.side, location.price, owner, remaining)
            .expect("validated reduction");
        if let Some(exposure) = exposure {
            self.perp_resting_exposure
                .insert((market_id, owner), exposure);
        }
        self.sync_order_leaf(id);
        self.commit_accounts(wallets);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn place(
        owner: u64,
        side: Side,
        price: u64,
        quantity: u64,
        nonce: u64,
        order_type: OrderType,
    ) -> Transaction {
        Transaction::PlaceOrder {
            account_id: owner.into(),
            market_id: 0,
            order_type,
            side,
            price,
            quantity,
            nonce,
        }
    }
    #[test]
    fn post_only_rejects_crossing_and_atomic_quote_failure_preserves_old_order() {
        let mut s = ExchangeState::default();
        s.execute(&place(1, Side::Sell, 100, 10, 0, OrderType::LO))
            .unwrap();
        s.execute(&place(0, Side::Buy, 90, 10, 0, OrderType::PostOnly))
            .unwrap();
        let id = s.account_orders(0.into())[0].id;
        let before = s.state_digest();
        let tx = Transaction::UpdateQuotes {
            account_id: 0.into(),
            market_id: 0,
            cancel: vec![id],
            reduce: vec![],
            place: vec![QuoteOrder {
                order_type: OrderType::PostOnly,
                side: Side::Buy,
                price: 100,
                quantity: 1,
                reduce_only: false,
            }],
            nonce: 1,
        };
        assert_eq!(s.execute(&tx), Err(ExchangeError::WouldTakeLiquidity));
        assert_eq!(s.state_digest(), before);
        assert_eq!(s.account_orders(0.into())[0].id, id);
        assert_eq!(
            s.execute(&place(2, Side::Sell, 90, 1, 0, OrderType::PostOnly)),
            Err(ExchangeError::WouldTakeLiquidity)
        );
        assert!(s.verify_state_tree());
    }
    #[test]
    fn reduction_preserves_fifo_releases_reserve_and_rejects_stale_or_foreign_targets() {
        let mut s = ExchangeState::default();
        s.execute(&place(0, Side::Buy, 100, 10, 0, OrderType::PostOnly))
            .unwrap();
        s.execute(&place(1, Side::Buy, 100, 10, 0, OrderType::PostOnly))
            .unwrap();
        let id = s.account_orders(0.into())[0].id;
        let before = s.accounts[&0.into()].balances[&USD].clone();
        let release = s.markets[&0].quote_atoms(100, 6).unwrap();
        let tx = Transaction::ReduceOrder {
            account_id: 0.into(),
            market_id: 0,
            order_id: id,
            remaining_quantity: 4,
            nonce: 1,
        };
        s.execute(&tx).unwrap();
        assert_eq!(
            s.accounts[&0.into()].balances[&USD].available,
            before.available + release
        );
        assert_eq!(
            s.accounts[&0.into()].balances[&USD].locked,
            before.locked - release
        );
        assert_eq!(s.order_books[&0].bids[&100].front().unwrap().id, id);
        let root = s.state_digest();
        assert!(s.execute(&tx.with_nonce(2)).is_err());
        assert!(
            s.execute(&Transaction::ReduceOrder {
                account_id: 1.into(),
                market_id: 0,
                order_id: id,
                remaining_quantity: 2,
                nonce: 1
            })
            .is_err()
        );
        assert_eq!(root, s.state_digest());
        let fills = s
            .execute_with_receipt(&place(2, Side::Sell, 100, 5, 0, OrderType::IOC))
            .unwrap();
        assert_eq!(fills[0].maker_order_id, id);
        assert_eq!(fills[0].quantity, 4);
        assert_eq!(fills.len(), 2);
        assert!(s.verify_state_tree());
    }
    #[test]
    fn reductions_in_batch_consume_one_nonce_and_roll_back_together() {
        let mut s = ExchangeState::default();
        for nonce in 0..2 {
            s.execute(&place(
                0,
                Side::Sell,
                100 + nonce,
                10,
                nonce,
                OrderType::PostOnly,
            ))
            .unwrap();
        }
        let ids: Vec<_> = s.account_orders(0.into()).iter().map(|o| o.id).collect();
        let mut tx = Transaction::UpdateQuotes {
            account_id: 0.into(),
            market_id: 0,
            cancel: vec![],
            place: vec![],
            reduce: vec![
                QuoteReduction {
                    order_id: ids[0],
                    remaining_quantity: 4,
                },
                QuoteReduction {
                    order_id: ids[1],
                    remaining_quantity: 11,
                },
            ],
            nonce: 2,
        };
        let root = s.state_digest();
        assert!(s.execute(&tx).is_err());
        assert_eq!(root, s.state_digest());
        if let Transaction::UpdateQuotes { reduce, .. } = &mut tx {
            reduce[1].remaining_quantity = 4;
        }
        s.execute(&tx).unwrap();
        assert_eq!(s.get_nonce_for_account(0.into()).unwrap(), 3);
        assert!(
            s.account_orders(0.into())
                .iter()
                .all(|o| o.remaining_quantity == 4)
        );
        assert!(s.verify_state_tree());
    }
}
