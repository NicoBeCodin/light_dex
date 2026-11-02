use super::PerpRiskParams;
use super::orders::{map_perp_error, worst_case_notional};
use crate::exchange::perp_units::{PerpUnits, USD_DECIMALS};
use crate::exchange::*;

impl ExchangeState {
    pub(in crate::exchange) fn withdraw_perp_collateral(
        &mut self,
        account_id: AccountId,
        market_id: MarketId,
        amount: u64,
        nonce: u64,
    ) -> Result<(), ExchangeError> {
        self.check_nonce(account_id, nonce)?;
        let market = self
            .markets
            .get(&market_id)
            .ok_or(ExchangeError::MarketNotFound)?;
        let MarketType::Perp { max_leverage } = market.market_type else {
            return Err(ExchangeError::UnsupportedMarketSettlement);
        };
        if market.quote_asset != USD || amount == 0 {
            return Err(ExchangeError::InvalidOrder);
        }
        let units = PerpUnits::new(market.price, market.quantity, USD_DECIMALS)
            .map_err(|_| ExchangeError::InvalidMarketConfig)?;
        let initial_bps = 10_000_u64.div_ceil(u64::from(max_leverage)).max(1);
        let risk = PerpRiskParams {
            initial_margin_bps: initial_bps,
            maintenance_margin_bps: market.maintenance_margin_bps,
        };
        if self.margin_mode(account_id) == MarginMode::Cross {
            let next = self
                .cross_collateral(account_id)
                .checked_sub(i128::from(amount))
                .ok_or(ExchangeError::ArithmeticOverflow)?;
            if next < 0 {
                return Err(ExchangeError::InsufficientBalance);
            }
            let (equity, initial, _) = self.cross_risk(account_id, next, None)?;
            if equity < initial {
                return Err(ExchangeError::InsufficientBalance);
            }
            let mut wallet = self.stage_accounts([account_id])?;
            wallet.credit_available(account_id, USD, amount)?;
            wallet.advance_nonce(account_id, nonce);
            self.commit_accounts(wallet);
            if next == 0 {
                self.cross_collateral.remove(&account_id);
                self.state_tree
                    .remove(StateKey::CrossCollateral(account_id));
            } else {
                self.cross_collateral.insert(account_id, next);
                self.state_tree
                    .put(StateKey::CrossCollateral(account_id), &next.to_be_bytes());
            }
            return Ok(());
        }
        let key = (market_id, account_id);
        let mut perp = self
            .perp_accounts
            .get(&key)
            .cloned()
            .ok_or(ExchangeError::InsufficientBalance)?;
        let resting = self
            .perp_resting_exposure
            .get(&key)
            .copied()
            .unwrap_or_default();
        let mark_price = if perp.position().size() == 0 && resting.is_empty() {
            market.price.step
        } else {
            let pair = OraclePair::new(market.base_asset, market.quote_asset);
            let mark_e8 = self
                .oracle_mark(pair)
                .ok_or(ExchangeError::OracleUnavailable)?
                .index_e8;
            market
                .price
                .round_e8_to_tick(mark_e8)
                .map_err(|_| ExchangeError::InvalidMarketConfig)?
        };
        perp.withdraw(amount, mark_price, &risk)
            .map_err(map_perp_error)?;
        let notional = worst_case_notional(&perp, resting, mark_price, units)?;
        let required = risk
            .initial_margin_required(notional)
            .map_err(map_perp_error)?;
        if perp.equity(mark_price).map_err(map_perp_error)? < required {
            return Err(ExchangeError::InsufficientBalance);
        }
        let mut wallet = self.stage_accounts([account_id])?;
        wallet.credit_available(account_id, USD, amount)?;
        wallet.advance_nonce(account_id, nonce);
        self.commit_accounts(wallet);
        self.state_tree.put(
            StateKey::PerpAccount(market_id, account_id),
            &canonical::encode_perp_account(&perp),
        );
        self.store_perp_account(key, perp);
        Ok(())
    }
}
