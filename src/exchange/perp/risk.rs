//! Deterministic risk views derived from committed marks and positions.
use super::orders::{map_perp_error, worst_case_notional};
use super::{MarginMode, PerpRiskParams};
use crate::exchange::perp_units::{PerpUnits, USD_DECIMALS};
use crate::exchange::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use ts_rs::TS;

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct PerpRiskSnapshot {
    pub account_id: AccountId,
    pub mode: MarginMode,
    pub market_id: Option<MarketId>,
    pub collateral_usd_atoms: i128,
    pub equity_usd_atoms: i128,
    pub initial_required_usd_atoms: i128,
    pub maintenance_required_usd_atoms: i128,
    pub liquidatable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct PerpPositionSnapshot {
    pub account_id: AccountId,
    pub market_id: MarketId,
    pub size_atoms: i128,
    pub entry_price: u64,
    pub realized_pnl_usd_atoms: i128,
}

impl ExchangeState {
    pub fn perp_position_snapshots(&self) -> Vec<PerpPositionSnapshot> {
        let mut snapshots = self
            .perp_accounts
            .iter()
            .filter_map(|(&(market_id, account_id), account)| {
                let position = account.position();
                (position.size() != 0).then_some(PerpPositionSnapshot {
                    account_id,
                    market_id,
                    size_atoms: position.size(),
                    entry_price: position.entry_price(),
                    realized_pnl_usd_atoms: position.realized_pnl(),
                })
            })
            .collect::<Vec<_>>();
        snapshots.sort_by_key(|position| (position.account_id, position.market_id));
        snapshots
    }
    pub fn perp_risk_snapshots(&self) -> Vec<PerpRiskSnapshot> {
        let accounts = self
            .perp_markets_by_account
            .keys()
            .chain(self.cross_collateral.keys())
            .chain(self.margin_modes.keys())
            .copied()
            .collect::<BTreeSet<_>>();
        self.risk_for_accounts(accounts.into_iter())
    }

    pub fn perp_positions_for(&self, account_id: AccountId) -> Vec<PerpPositionSnapshot> {
        self.perp_markets_by_account
            .get(&account_id)
            .into_iter()
            .flatten()
            .filter_map(|&market_id| {
                let position = self.perp_accounts[&(market_id, account_id)].position();
                (position.size() != 0).then_some(PerpPositionSnapshot {
                    account_id,
                    market_id,
                    size_atoms: position.size(),
                    entry_price: position.entry_price(),
                    realized_pnl_usd_atoms: position.realized_pnl(),
                })
            })
            .collect()
    }

    pub fn perp_risk_for(&self, account: AccountId) -> Vec<PerpRiskSnapshot> {
        self.risk_for_accounts(std::iter::once(account))
    }

    /// Owners with a position/account row in this market, without scanning
    /// unrelated markets. Cross risk still evaluates each owner's portfolio.
    pub fn perp_accounts_for_market(&self, market: MarketId) -> Vec<AccountId> {
        self.perp_accounts_by_market
            .get(&market)
            .into_iter()
            .flatten()
            .copied()
            .collect()
    }

    fn risk_for_accounts(
        &self,
        accounts: impl Iterator<Item = AccountId>,
    ) -> Vec<PerpRiskSnapshot> {
        let mut snapshots = Vec::new();
        for account_id in accounts {
            if self.margin_mode(account_id) == MarginMode::Cross {
                if let Ok((equity, initial, maintenance)) =
                    self.cross_risk(account_id, self.cross_collateral(account_id), None)
                {
                    snapshots.push(PerpRiskSnapshot {
                        account_id,
                        mode: MarginMode::Cross,
                        market_id: None,
                        collateral_usd_atoms: self.cross_collateral(account_id),
                        equity_usd_atoms: equity,
                        initial_required_usd_atoms: initial,
                        maintenance_required_usd_atoms: maintenance,
                        liquidatable: maintenance > 0 && equity < maintenance,
                    });
                }
                continue;
            }
            for &market_id in self
                .perp_markets_by_account
                .get(&account_id)
                .into_iter()
                .flatten()
            {
                let position = &self.perp_accounts[&(market_id, account_id)];
                let Some(market) = self.markets.get(&market_id) else {
                    continue;
                };
                let MarketType::Perp { max_leverage } = market.market_type else {
                    continue;
                };
                let resting = self
                    .perp_resting_exposure
                    .get(&(market_id, account_id))
                    .copied()
                    .unwrap_or_default();
                if position.position().size() == 0 && resting.is_empty() {
                    snapshots.push(PerpRiskSnapshot {
                        account_id,
                        mode: MarginMode::Isolated,
                        market_id: Some(market_id),
                        collateral_usd_atoms: position.collateral(),
                        equity_usd_atoms: position.collateral(),
                        initial_required_usd_atoms: 0,
                        maintenance_required_usd_atoms: 0,
                        liquidatable: false,
                    });
                    continue;
                }
                let Some(mark) =
                    self.oracle_mark(OraclePair::new(market.base_asset, market.quote_asset))
                else {
                    continue;
                };
                let Ok(price) = market.price.round_e8_to_tick(mark.index_e8) else {
                    continue;
                };
                let Ok(units) = PerpUnits::new(market.price, market.quantity, USD_DECIMALS) else {
                    continue;
                };
                let Ok(notional) = worst_case_notional(position, resting, price, units) else {
                    continue;
                };
                let risk = PerpRiskParams {
                    initial_margin_bps: 10_000_u64.div_ceil(u64::from(max_leverage)),
                    maintenance_margin_bps: market.maintenance_margin_bps,
                };
                let Ok(equity) = position.equity(price).map_err(map_perp_error) else {
                    continue;
                };
                let Ok(initial) = risk
                    .initial_margin_required(notional)
                    .map_err(map_perp_error)
                else {
                    continue;
                };
                let Ok(current) = position.position().notional(price).map_err(map_perp_error)
                else {
                    continue;
                };
                let Ok(maintenance) = risk
                    .maintenance_margin_required(current)
                    .map_err(map_perp_error)
                else {
                    continue;
                };
                snapshots.push(PerpRiskSnapshot {
                    account_id,
                    mode: MarginMode::Isolated,
                    market_id: Some(market_id),
                    collateral_usd_atoms: position.collateral(),
                    equity_usd_atoms: equity,
                    initial_required_usd_atoms: initial,
                    maintenance_required_usd_atoms: maintenance,
                    liquidatable: current > 0 && equity < maintenance,
                });
            }
        }
        snapshots.sort_by_key(|entry| (entry.account_id, entry.market_id));
        snapshots
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_collateral_is_visible_without_an_oracle_mark() {
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
        state
            .execute(&Transaction::DepositPerpCollateral {
                account_id: 0.into(),
                market_id: 1,
                amount: 100_000_000,
                nonce: 0,
            })
            .unwrap();
        let risk = state.perp_risk_snapshots();
        assert_eq!(risk.len(), 1);
        assert_eq!(risk[0].equity_usd_atoms, 100_000_000);
        assert_eq!(risk[0].initial_required_usd_atoms, 0);
    }
}
