//! Prototype funding: a bounded premium between the perp book mid and the
//! newly committed oracle index. One tick is one accepted oracle update for
//! this pair, never a validator-local timer. All arithmetic is integer based.
use super::orders::map_perp_error;
use super::{IsolatedPerpAccount, MarginMode};
use crate::exchange::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FundingClock {
    pub updates: u64,
    pub last_rate_bps: i64,
    pub total_transferred_usd_atoms: u64,
}

impl FundingClock {
    pub(in crate::exchange) fn encode(self) -> Vec<u8> {
        let mut bytes = b"light-dex/funding-clock/v1".to_vec();
        bytes.extend_from_slice(&self.updates.to_be_bytes());
        bytes.extend_from_slice(&self.last_rate_bps.to_be_bytes());
        bytes.extend_from_slice(&self.total_transferred_usd_atoms.to_be_bytes());
        bytes
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct FundingSnapshot {
    pub market_id: MarketId,
    pub updates: u64,
    pub last_rate_bps: i64,
    pub total_transferred_usd_atoms: u64,
}

#[derive(Default)]
pub(in crate::exchange) struct FundingPlan {
    clocks: Vec<(MarketId, FundingClock)>,
    isolated: HashMap<(MarketId, AccountId), IsolatedPerpAccount>,
    cross: HashMap<AccountId, i128>,
}

fn split_amounts(
    due: &[(AccountId, u128)],
    total: u128,
    transfer: u128,
) -> Result<Vec<(AccountId, u128)>, ExchangeError> {
    if total == 0 || transfer == 0 {
        return Ok(Vec::new());
    }
    let mut result = due
        .iter()
        .map(|&(id, amount)| {
            amount
                .checked_mul(transfer)
                .map(|value| (id, value / total))
                .ok_or(ExchangeError::ArithmeticOverflow)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let allocated = result.iter().try_fold(0u128, |sum, (_, amount)| {
        sum.checked_add(*amount)
            .ok_or(ExchangeError::ArithmeticOverflow)
    })?;
    let mut remainder = transfer - allocated;
    for (index, (_, owed)) in due.iter().enumerate() {
        if remainder == 0 {
            break;
        }
        if result[index].1 < *owed {
            result[index].1 += 1;
            remainder -= 1;
        }
    }
    if remainder != 0 {
        return Err(ExchangeError::ArithmeticOverflow);
    }
    Ok(result)
}

impl ExchangeState {
    pub fn funding_snapshots(&self) -> Vec<FundingSnapshot> {
        let mut snapshots = self
            .funding_clocks
            .iter()
            .map(|(&market_id, clock)| FundingSnapshot {
                market_id,
                updates: clock.updates,
                last_rate_bps: clock.last_rate_bps,
                total_transferred_usd_atoms: clock.total_transferred_usd_atoms,
            })
            .collect::<Vec<_>>();
        snapshots.sort_by_key(|snapshot| snapshot.market_id);
        snapshots
    }

    pub(in crate::exchange) fn plan_funding(
        &self,
        pair: OraclePair,
        index_e8: u64,
    ) -> Result<FundingPlan, ExchangeError> {
        let mut plan = FundingPlan::default();
        let mut market_ids = self
            .markets
            .values()
            .filter(|market| {
                market.base_asset == pair.base_asset
                    && market.quote_asset == pair.quote_asset
                    && matches!(market.market_type, MarketType::Perp { .. })
            })
            .map(|market| market.id)
            .collect::<Vec<_>>();
        market_ids.sort_unstable();
        for market_id in market_ids {
            let market = &self.markets[&market_id];
            let mut clock = self
                .funding_clocks
                .get(&market_id)
                .copied()
                .unwrap_or_default();
            clock.updates = clock
                .updates
                .checked_add(1)
                .ok_or(ExchangeError::ArithmeticOverflow)?;
            if clock.updates % market.funding_interval_updates != 0 {
                plan.clocks.push((market_id, clock));
                continue;
            }
            let index = market
                .price
                .round_e8_to_tick(index_e8)
                .map_err(|_| ExchangeError::InvalidMarketConfig)?;
            let mid = self.order_books.get(&market_id).and_then(|book| {
                let bid = *book.bids.keys().next_back()?;
                let ask = *book.asks.keys().next()?;
                Some((u128::from(bid) + u128::from(ask)) / 2)
            });
            let raw_rate = if let Some(mid) = mid {
                let deviation = i128::try_from(mid)
                    .map_err(|_| ExchangeError::ArithmeticOverflow)?
                    .checked_sub(i128::from(index))
                    .ok_or(ExchangeError::ArithmeticOverflow)?;
                deviation
                    .checked_mul(10_000)
                    .ok_or(ExchangeError::ArithmeticOverflow)?
                    / i128::from(index)
            } else {
                0
            };
            let rate = raw_rate.clamp(
                -i128::from(market.max_funding_bps),
                i128::from(market.max_funding_bps),
            );
            clock.last_rate_bps =
                i64::try_from(rate).map_err(|_| ExchangeError::ArithmeticOverflow)?;
            if rate == 0 {
                plan.clocks.push((market_id, clock));
                continue;
            }
            let mut payers = Vec::new();
            let mut receivers = Vec::new();
            for &account in self
                .perp_accounts_by_market
                .get(&market_id)
                .into_iter()
                .flatten()
            {
                let position = &self.perp_accounts[&(market_id, account)];
                if position.position().size() == 0 {
                    continue;
                }
                let notional = position
                    .position()
                    .notional(index)
                    .map_err(map_perp_error)?;
                let due = notional
                    .checked_mul(rate.abs())
                    .ok_or(ExchangeError::ArithmeticOverflow)?
                    / 10_000;
                if due == 0 {
                    continue;
                }
                let due = u128::try_from(due).map_err(|_| ExchangeError::ArithmeticOverflow)?;
                if (position.position().size() > 0) == (rate > 0) {
                    payers.push((account, due));
                } else {
                    receivers.push((account, due));
                }
            }
            payers.sort_by_key(|(id, _)| *id);
            receivers.sort_by_key(|(id, _)| *id);
            let sum = |entries: &[(AccountId, u128)]| {
                entries.iter().try_fold(0u128, |value, (_, amount)| {
                    value
                        .checked_add(*amount)
                        .ok_or(ExchangeError::ArithmeticOverflow)
                })
            };
            let total_pay = sum(&payers)?;
            let total_receive = sum(&receivers)?;
            let transferred = total_pay.min(total_receive);
            for (id, amount) in split_amounts(&payers, total_pay, transferred)? {
                self.stage_funding_delta(
                    &mut plan,
                    market_id,
                    id,
                    -i128::try_from(amount).map_err(|_| ExchangeError::ArithmeticOverflow)?,
                )?;
            }
            for (id, amount) in split_amounts(&receivers, total_receive, transferred)? {
                self.stage_funding_delta(
                    &mut plan,
                    market_id,
                    id,
                    i128::try_from(amount).map_err(|_| ExchangeError::ArithmeticOverflow)?,
                )?;
            }
            clock.total_transferred_usd_atoms = clock
                .total_transferred_usd_atoms
                .checked_add(
                    u64::try_from(transferred).map_err(|_| ExchangeError::ArithmeticOverflow)?,
                )
                .ok_or(ExchangeError::ArithmeticOverflow)?;
            plan.clocks.push((market_id, clock));
        }
        Ok(plan)
    }

    fn stage_funding_delta(
        &self,
        plan: &mut FundingPlan,
        market_id: MarketId,
        account: AccountId,
        delta: i128,
    ) -> Result<(), ExchangeError> {
        if self.margin_mode(account) == MarginMode::Cross {
            let pool = plan
                .cross
                .entry(account)
                .or_insert_with(|| self.cross_collateral(account));
            *pool = pool
                .checked_add(delta)
                .ok_or(ExchangeError::ArithmeticOverflow)?;
        } else {
            let position = plan
                .isolated
                .entry((market_id, account))
                .or_insert_with(|| self.perp_accounts[&(market_id, account)].clone());
            position.adjust_collateral(delta).map_err(map_perp_error)?;
        }
        Ok(())
    }

    pub(in crate::exchange) fn commit_funding(&mut self, plan: FundingPlan) {
        for (id, clock) in plan.clocks {
            self.funding_clocks.insert(id, clock);
            self.state_tree
                .put(StateKey::FundingClock(id), &clock.encode());
        }
        for (key, position) in plan.isolated {
            self.state_tree.put(
                StateKey::PerpAccount(key.0, key.1),
                &canonical::encode_perp_account(&position),
            );
            self.store_perp_account(key, position);
        }
        for (account, pool) in plan.cross {
            self.cross_collateral.insert(account, pool);
            self.state_tree
                .put(StateKey::CrossCollateral(account), &pool.to_be_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::{OracleObservation, OracleUpdate, Venue, index_from_observations};

    #[test]
    fn committed_oracle_tick_transfers_funding_without_creating_usd() {
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
        state.markets.get_mut(&1).unwrap().funding_interval_updates = 1;
        state.markets.get_mut(&1).unwrap().max_funding_bps = 10;
        let pair = OraclePair::new(BTC, USD);
        state.oracle_marks.insert(
            pair,
            OracleMark {
                index_e8: 10_000_000_000,
                update_digest: [1; 32],
            },
        );
        state.rebuild_state_tree();
        for id in 0..2 {
            state
                .execute(&Transaction::DepositPerpCollateral {
                    account_id: id.into(),
                    market_id: 1,
                    amount: 100_000_000,
                    nonce: 0,
                })
                .unwrap();
        }
        for (id, side, order_type, nonce) in [
            (1, Side::Sell, OrderType::LO, 1),
            (0, Side::Buy, OrderType::FOK, 1),
            (0, Side::Buy, OrderType::LO, 2),
            (1, Side::Sell, OrderType::LO, 2),
        ] {
            let price = if nonce == 1 {
                1_000
            } else if id == 0 {
                1_100
            } else {
                1_120
            };
            state
                .execute(&Transaction::PlacePerpOrder {
                    account_id: id.into(),
                    market_id: 1,
                    order_type,
                    side,
                    price,
                    quantity: 10_000,
                    reduce_only: false,
                    nonce,
                })
                .unwrap();
        }
        let before_long = state.perp_accounts[&(1, 0.into())].collateral();
        let before_short = state.perp_accounts[&(1, 1.into())].collateral();
        let observations = [
            (Venue::Binance, "BTCUSDT"),
            (Venue::Okx, "BTC-USDT"),
            (Venue::Coinbase, "BTC-USD"),
        ]
        .map(|(venue, symbol)| OracleObservation {
            venue,
            symbol: symbol.into(),
            bid: "100".into(),
            ask: "102".into(),
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
        let long = state.perp_accounts[&(1, 0.into())].collateral();
        let short = state.perp_accounts[&(1, 1.into())].collateral();
        assert_eq!(long + short, before_long + before_short);
        assert_eq!(before_long - long, 101_000);
        assert_eq!(state.funding_clocks[&1].last_rate_bps, 10);
        assert_eq!(
            state.funding_clocks[&1].total_transferred_usd_atoms,
            101_000
        );
        assert!(state.verify_state_tree());
    }
}
