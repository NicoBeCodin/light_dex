use super::orders::{map_perp_error, worst_case_notional};
use crate::exchange::perp_units::{PerpUnits, USD_DECIMALS};
use crate::exchange::*;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
pub enum MarginMode {
    #[default]
    Isolated,
    Cross,
}

impl ExchangeState {
    pub fn cross_collateral(&self, account: AccountId) -> i128 {
        self.cross_collateral.get(&account).copied().unwrap_or(0)
    }

    /// Portfolio USD atoms: equity, initial requirement including resting
    /// orders, maintenance requirement on positions only.
    pub(in crate::exchange) fn cross_risk(
        &self,
        account: AccountId,
        pool: i128,
        override_market: Option<(MarketId, &IsolatedPerpAccount, RestingExposure)>,
    ) -> Result<(i128, i128, i128), ExchangeError> {
        let mut equity = pool;
        let mut initial = 0i128;
        let mut maintenance = 0i128;
        let mut active = self
            .perp_markets_by_account
            .get(&account)
            .cloned()
            .unwrap_or_default();
        if let Some((market, _, _)) = override_market {
            active.insert(market);
        }
        for market_id in active {
            self.charge_work(1)?;
            let market = &self.markets[&market_id];
            let MarketType::Perp { max_leverage } = market.market_type else {
                continue;
            };
            let staged = override_market.filter(|(id, _, _)| *id == market_id);
            let original = self.perp_accounts.get(&(market_id, account));
            let perp = staged.map(|(_, perp, _)| perp).or(original);
            let resting = staged.map(|(_, _, resting)| resting).unwrap_or_else(|| {
                self.perp_resting_exposure
                    .get(&(market_id, account))
                    .copied()
                    .unwrap_or_default()
            });
            if perp.is_none() && resting.is_empty() {
                continue;
            }
            let units = PerpUnits::new(market.price, market.quantity, USD_DECIMALS)
                .map_err(|_| ExchangeError::InvalidMarketConfig)?;
            let empty = IsolatedPerpAccount::new(units);
            let perp = perp.unwrap_or(&empty);
            if perp.position().size() == 0 && resting.is_empty() {
                continue;
            }
            let pair = OraclePair::new(market.base_asset, market.quote_asset);
            let mark = self
                .oracle_mark(pair)
                .ok_or(ExchangeError::OracleUnavailable)?;
            let price = market
                .price
                .round_e8_to_tick(mark.index_e8)
                .map_err(|_| ExchangeError::InvalidMarketConfig)?;
            let initial_bps = 10_000_u64.div_ceil(u64::from(max_leverage));
            let (pnl, im, mm) = contribution(
                account,
                market_id,
                perp,
                resting,
                price,
                units,
                initial_bps,
                market.maintenance_margin_bps,
            )?;
            equity = equity
                .checked_add(pnl)
                .ok_or(ExchangeError::ArithmeticOverflow)?;
            initial = initial
                .checked_add(im)
                .ok_or(ExchangeError::ArithmeticOverflow)?;
            maintenance = maintenance
                .checked_add(mm)
                .ok_or(ExchangeError::ArithmeticOverflow)?;
        }
        Ok((equity, initial, maintenance))
    }
    pub fn margin_mode(&self, account: AccountId) -> MarginMode {
        self.margin_modes.get(&account).copied().unwrap_or_default()
    }

    pub(in crate::exchange) fn set_margin_mode(
        &mut self,
        account: AccountId,
        mode: MarginMode,
        nonce: u64,
    ) -> Result<(), ExchangeError> {
        self.check_nonce(account, nonce)?;
        if self
            .perp_markets_by_account
            .get(&account)
            .into_iter()
            .flatten()
            .any(|&market| {
                let position = &self.perp_accounts[&(market, account)];
                position.position().size() != 0
                    || position.collateral() != 0
                    || self.perp_resting_exposure.contains_key(&(market, account))
            })
            || self.cross_collateral.get(&account).copied().unwrap_or(0) != 0
        {
            return Err(ExchangeError::InvalidOrder);
        }
        let mut wallet = self.stage_accounts([account])?;
        wallet.advance_nonce(account, nonce);
        self.commit_accounts(wallet);
        if mode == MarginMode::Isolated {
            self.margin_modes.remove(&account);
            self.state_tree.remove(StateKey::MarginMode(account));
        } else {
            self.margin_modes.insert(account, mode);
            self.state_tree.put(StateKey::MarginMode(account), &[1]);
        }
        Ok(())
    }
}

// Fixed-size, thread-local acceleration cache. Exact inputs (including mark,
// units, position, exposure and margin parameters) MUST match on every hit.
// Consequently speculative forks, funding and oracle changes cannot reuse a
// stale contribution. Collateral is added outside the cached calculation.
#[derive(Clone, PartialEq, Eq)]
struct ContributionKey {
    account: AccountId,
    market: MarketId,
    perp: IsolatedPerpAccount,
    resting: RestingExposure,
    price: u64,
    units: PerpUnits,
    initial_bps: u64,
    maintenance_bps: u64,
}
type Contribution = (i128, i128, i128);
thread_local! {
    static CONTRIBUTIONS: std::cell::RefCell<Vec<Option<(ContributionKey, Contribution)>>> =
        std::cell::RefCell::new(vec![None; 4096]);
}
fn contribution(
    account: AccountId,
    market: MarketId,
    perp: &IsolatedPerpAccount,
    resting: RestingExposure,
    price: u64,
    units: PerpUnits,
    initial_bps: u64,
    maintenance_bps: u64,
) -> Result<Contribution, ExchangeError> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (account, market).hash(&mut hasher);
    let slot = hasher.finish() as usize % 4096;
    let key = ContributionKey {
        account,
        market,
        perp: perp.clone(),
        resting,
        price,
        units,
        initial_bps,
        maintenance_bps,
    };
    CONTRIBUTIONS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some((cached, value)) = &cache[slot] {
            if *cached == key {
                return Ok(*value);
            }
        }
        let margin = |amount: i128, bps: u64| {
            amount
                .checked_mul(i128::from(bps))
                .and_then(|v| v.checked_add(9_999))
                .map(|v| v / 10_000)
                .ok_or(ExchangeError::ArithmeticOverflow)
        };
        let value = (
            perp.position()
                .unrealized_pnl(price)
                .map_err(map_perp_error)?,
            margin(
                worst_case_notional(perp, resting, price, units)?,
                initial_bps,
            )?,
            margin(
                perp.position().notional(price).map_err(map_perp_error)?,
                maintenance_bps,
            )?,
        );
        cache[slot] = Some((key, value));
        Ok(value)
    })
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    #[test]
    fn exact_cache_key_tracks_marks_exposure_parameters_and_speculative_positions() {
        let units = PerpUnits::new(
            DecimalStep::new(1, 1).unwrap(),
            DecimalStep::new(4, 1).unwrap(),
            USD_DECIMALS,
        )
        .unwrap();
        let mut perp = IsolatedPerpAccount::new(units);
        perp.deposit(1_000_000_000).unwrap();
        perp.apply_fill(
            Side::Buy,
            10_000,
            1000,
            1000,
            &super::super::PerpRiskParams {
                initial_margin_bps: 1000,
                maintenance_margin_bps: 500,
            },
        )
        .unwrap();
        let base = contribution(
            0.into(),
            1,
            &perp,
            RestingExposure::default(),
            1000,
            units,
            1000,
            500,
        )
        .unwrap();
        assert_eq!(
            base,
            contribution(
                0.into(),
                1,
                &perp,
                RestingExposure::default(),
                1000,
                units,
                1000,
                500
            )
            .unwrap()
        );
        for (price, resting, initial, maintenance) in [
            (1100, RestingExposure::default(), 1000, 500),
            (
                1000,
                RestingExposure {
                    buy_qty: 10_000,
                    sell_qty: 0,
                },
                1000,
                500,
            ),
            (1000, RestingExposure::default(), 2000, 600),
        ] {
            let next = contribution(
                0.into(),
                1,
                &perp,
                resting,
                price,
                units,
                initial,
                maintenance,
            )
            .unwrap();
            assert_ne!(base, next);
            assert_eq!(
                base,
                contribution(
                    0.into(),
                    1,
                    &perp,
                    RestingExposure::default(),
                    1000,
                    units,
                    1000,
                    500
                )
                .unwrap()
            );
        }
        let mut fork = perp.clone();
        fork.apply_fill(
            Side::Sell,
            5000,
            1000,
            1000,
            &super::super::PerpRiskParams {
                initial_margin_bps: 1000,
                maintenance_margin_bps: 500,
            },
        )
        .unwrap();
        assert_ne!(
            base,
            contribution(
                0.into(),
                1,
                &fork,
                RestingExposure::default(),
                1000,
                units,
                1000,
                500
            )
            .unwrap()
        );
        assert_eq!(
            base,
            contribution(
                0.into(),
                1,
                &perp,
                RestingExposure::default(),
                1000,
                units,
                1000,
                500
            )
            .unwrap()
        );
    }
}
