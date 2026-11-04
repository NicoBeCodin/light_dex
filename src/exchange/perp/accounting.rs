use crate::exchange::order::Side;
use crate::exchange::perp_units::{PerpUnits, UnitError};

/// One linear perp position. `size` is in market quantity atoms; cost basis and
/// PnL are in collateral/quote atoms. `entry_price` is display-only.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PerpPosition {
    units: PerpUnits,
    size: i128,
    entry_price: u64,
    realized_pnl: i128,
    open_cost_basis: i128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerpError {
    ZeroQuantity,
    ZeroPrice,
    ArithmeticOverflow,
    InvalidUnits,
    InvalidRiskParams,
    InvalidNotional,
    InsufficientCollateral,
    InsufficientMargin,
}

impl From<UnitError> for PerpError {
    fn from(error: UnitError) -> Self {
        match error {
            UnitError::Overflow => Self::ArithmeticOverflow,
            _ => Self::InvalidUnits,
        }
    }
}

impl PerpPosition {
    pub fn new(units: PerpUnits) -> Self {
        Self {
            units,
            size: 0,
            entry_price: 0,
            realized_pnl: 0,
            open_cost_basis: 0,
        }
    }

    pub fn open_cost_basis(&self) -> i128 {
        self.open_cost_basis
    }

    pub fn units(&self) -> PerpUnits {
        self.units
    }

    pub fn size(&self) -> i128 {
        self.size
    }

    pub fn entry_price(&self) -> u64 {
        self.entry_price
    }

    pub fn realized_pnl(&self) -> i128 {
        self.realized_pnl
    }

    /// Apply a fill and return the PnL realized by the quantity closed.
    /// No field changes if arithmetic fails.
    pub fn apply_fill(&mut self, side: Side, quantity: u64, price: u64) -> Result<i128, PerpError> {
        if quantity == 0 {
            return Err(PerpError::ZeroQuantity);
        }
        if price == 0 {
            return Err(PerpError::ZeroPrice);
        }

        let fill_value = self.units.notional(price, quantity)?;
        let quantity = i128::from(quantity);
        let old_size = self.size;
        let old_quantity = old_size
            .checked_abs()
            .ok_or(PerpError::ArithmeticOverflow)?;
        let signed_fill = if side == Side::Buy {
            quantity
        } else {
            -quantity
        };
        let new_size = old_size
            .checked_add(signed_fill)
            .ok_or(PerpError::ArithmeticOverflow)?;
        // PerpUnits currently accepts u64 quantities; keep the aggregate
        // position within that range so mark-to-market always remains valid.
        u64::try_from(
            new_size
                .checked_abs()
                .ok_or(PerpError::ArithmeticOverflow)?,
        )
        .map_err(|_| PerpError::ArithmeticOverflow)?;

        let (new_basis, realized_on_fill) = if old_size == 0
            || (old_size > 0 && side == Side::Buy)
            || (old_size < 0 && side == Side::Sell)
        {
            let basis = self
                .open_cost_basis
                .checked_add(fill_value)
                .ok_or(PerpError::ArithmeticOverflow)?;
            (basis, 0)
        } else {
            let closed = old_quantity.min(quantity);
            // A full close consumes the entire remaining basis, including
            // remainders left by earlier partial closes.
            let whole = (self.open_cost_basis / old_quantity)
                .checked_mul(closed)
                .ok_or(PerpError::ArithmeticOverflow)?;
            let fractional = (self.open_cost_basis % old_quantity)
                .checked_mul(closed)
                .ok_or(PerpError::ArithmeticOverflow)?
                / old_quantity;
            let closed_basis = whole
                .checked_add(fractional)
                .ok_or(PerpError::ArithmeticOverflow)?;
            let close_value = self.units.notional(
                price,
                u64::try_from(closed).map_err(|_| PerpError::ArithmeticOverflow)?,
            )?;
            let realized = if old_size > 0 {
                close_value.checked_sub(closed_basis)
            } else {
                closed_basis.checked_sub(close_value)
            }
            .ok_or(PerpError::ArithmeticOverflow)?;

            let remaining_basis = if quantity > old_quantity {
                // A reversal opens only the excess at this fill price.
                self.units.notional(
                    price,
                    u64::try_from(quantity - old_quantity)
                        .map_err(|_| PerpError::ArithmeticOverflow)?,
                )?
            } else {
                self.open_cost_basis
                    .checked_sub(closed_basis)
                    .ok_or(PerpError::ArithmeticOverflow)?
            };
            (remaining_basis, realized)
        };

        let new_realized_pnl = self
            .realized_pnl
            .checked_add(realized_on_fill)
            .ok_or(PerpError::ArithmeticOverflow)?;
        let new_entry_price = if new_size == 0 {
            0
        } else {
            let magnitude = u128::try_from(new_basis).map_err(|_| PerpError::ArithmeticOverflow)?;
            let numerator = magnitude
                .checked_mul(u128::from(self.units.price.scale()))
                .and_then(|v| v.checked_mul(u128::from(self.units.quantity.scale())))
                .ok_or(PerpError::ArithmeticOverflow)?;
            let denominator = u128::try_from(
                new_size
                    .checked_abs()
                    .ok_or(PerpError::ArithmeticOverflow)?,
            )
            .map_err(|_| PerpError::ArithmeticOverflow)?
            .checked_mul(10u128.pow(u32::from(self.units.quote_decimals)))
            .ok_or(PerpError::ArithmeticOverflow)?;
            u64::try_from(numerator / denominator).map_err(|_| PerpError::ArithmeticOverflow)?
        };

        self.size = new_size;
        self.open_cost_basis = new_basis;
        self.entry_price = new_entry_price;
        self.realized_pnl = new_realized_pnl;
        Ok(realized_on_fill)
    }

    pub fn unrealized_pnl(&self, mark_price: u64) -> Result<i128, PerpError> {
        let marked_value = self.notional(mark_price)?;
        let pnl = if self.size < 0 {
            self.open_cost_basis.checked_sub(marked_value)
        } else {
            marked_value.checked_sub(self.open_cost_basis)
        };
        pnl.ok_or(PerpError::ArithmeticOverflow)
    }

    pub fn notional(&self, mark_price: u64) -> Result<i128, PerpError> {
        if self.size == 0 {
            return Ok(0);
        }
        let quantity = u64::try_from(
            self.size
                .checked_abs()
                .ok_or(PerpError::ArithmeticOverflow)?,
        )
        .map_err(|_| PerpError::ArithmeticOverflow)?;
        Ok(self.units.notional(mark_price, quantity)?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerpRiskParams {
    pub initial_margin_bps: u64,
    pub maintenance_margin_bps: u64,
}

impl PerpRiskParams {
    pub fn validate(&self) -> Result<(), PerpError> {
        if self.maintenance_margin_bps == 0
            || self.maintenance_margin_bps > self.initial_margin_bps
            || self.initial_margin_bps > 10_000
        {
            return Err(PerpError::InvalidRiskParams);
        }
        Ok(())
    }

    fn margin_required(&self, notional: i128, bps: u64) -> Result<i128, PerpError> {
        self.validate()?;
        if notional < 0 {
            return Err(PerpError::InvalidNotional);
        }
        // Split before multiplication to avoid overflow for large notionals.
        let whole = (notional / 10_000)
            .checked_mul(i128::from(bps))
            .ok_or(PerpError::ArithmeticOverflow)?;
        let remainder = (notional % 10_000) * i128::from(bps);
        whole
            .checked_add(remainder / 10_000 + i128::from(remainder % 10_000 != 0))
            .ok_or(PerpError::ArithmeticOverflow)
    }

    pub fn initial_margin_required(&self, notional: i128) -> Result<i128, PerpError> {
        self.margin_required(notional, self.initial_margin_bps)
    }

    pub fn maintenance_margin_required(&self, notional: i128) -> Result<i128, PerpError> {
        self.margin_required(notional, self.maintenance_margin_bps)
    }
}

pub fn equity(
    collateral: i128,
    position: &PerpPosition,
    mark_price: u64,
) -> Result<i128, PerpError> {
    collateral
        .checked_add(position.unrealized_pnl(mark_price)?)
        .ok_or(PerpError::ArithmeticOverflow)
}

/// Isolated collateral is held in quote atoms (micro-USD for BTC-USD).
/// Realized PnL is settled into collateral on each fill;
/// `position.realized_pnl` is a historical total.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IsolatedPerpAccount {
    collateral: i128,
    position: PerpPosition,
}

impl IsolatedPerpAccount {
    pub fn new(units: PerpUnits) -> Self {
        Self {
            collateral: 0,
            position: PerpPosition::new(units),
        }
    }

    pub fn collateral(&self) -> i128 {
        self.collateral
    }

    pub fn position(&self) -> &PerpPosition {
        &self.position
    }

    pub fn equity(&self, mark_price: u64) -> Result<i128, PerpError> {
        equity(self.collateral, &self.position, mark_price)
    }

    /// Cross-margin fills settle realized PnL into the shared pool instead of
    /// this market's isolated collateral. Portfolio risk is checked by the caller.
    pub(in crate::exchange) fn apply_cross_fill(
        &mut self,
        side: Side,
        quantity: u64,
        price: u64,
    ) -> Result<i128, PerpError> {
        self.position.apply_fill(side, quantity, price)
    }

    pub(in crate::exchange) fn debit_fee(&mut self, amount: i128) -> Result<(), PerpError> {
        if amount < 0 || self.collateral < amount {
            return Err(PerpError::InsufficientCollateral);
        }
        self.collateral -= amount;
        Ok(())
    }

    pub(in crate::exchange) fn adjust_collateral(&mut self, delta: i128) -> Result<(), PerpError> {
        self.collateral = self
            .collateral
            .checked_add(delta)
            .ok_or(PerpError::ArithmeticOverflow)?;
        Ok(())
    }

    pub fn deposit(&mut self, amount: u64) -> Result<(), PerpError> {
        if amount == 0 {
            return Err(PerpError::ZeroQuantity);
        }
        self.collateral = self
            .collateral
            .checked_add(i128::from(amount))
            .ok_or(PerpError::ArithmeticOverflow)?;
        Ok(())
    }

    pub fn withdraw(
        &mut self,
        amount: u64,
        mark_price: u64,
        risk: &PerpRiskParams,
    ) -> Result<(), PerpError> {
        if amount == 0 {
            return Err(PerpError::ZeroQuantity);
        }
        if mark_price == 0 {
            return Err(PerpError::ZeroPrice);
        }
        risk.validate()?;
        let next_collateral = self
            .collateral
            .checked_sub(i128::from(amount))
            .ok_or(PerpError::ArithmeticOverflow)?;
        if next_collateral < 0 {
            return Err(PerpError::InsufficientCollateral);
        }
        let required = risk.initial_margin_required(self.position.notional(mark_price)?)?;
        if equity(next_collateral, &self.position, mark_price)? < required {
            return Err(PerpError::InsufficientMargin);
        }
        self.collateral = next_collateral;
        Ok(())
    }

    /// Stage the fill so a failed risk check never changes position or collateral.
    /// Reducing exposure remains possible below initial margin; opening or
    /// reversing exposure requires initial margin at the supplied mark price.
    pub fn apply_fill(
        &mut self,
        side: Side,
        quantity: u64,
        price: u64,
        mark_price: u64,
        risk: &PerpRiskParams,
    ) -> Result<i128, PerpError> {
        risk.validate()?;
        if mark_price == 0 {
            return Err(PerpError::ZeroPrice);
        }
        let mut next_position = self.position.clone();
        let realized = next_position.apply_fill(side, quantity, price)?;
        let next_collateral = self
            .collateral
            .checked_add(realized)
            .ok_or(PerpError::ArithmeticOverflow)?;
        let old_size = self.position.size;
        let new_size = next_position.size;
        let increases_exposure = new_size
            .checked_abs()
            .ok_or(PerpError::ArithmeticOverflow)?
            > old_size
                .checked_abs()
                .ok_or(PerpError::ArithmeticOverflow)?
            || (old_size != 0 && new_size != 0 && old_size.signum() != new_size.signum());
        if increases_exposure {
            let required = risk.initial_margin_required(next_position.notional(mark_price)?)?;
            if equity(next_collateral, &next_position, mark_price)? < required {
                return Err(PerpError::InsufficientMargin);
            }
        }
        self.position = next_position;
        self.collateral = next_collateral;
        Ok(realized)
    }

    pub fn below_maintenance(
        &self,
        mark_price: u64,
        risk: &PerpRiskParams,
    ) -> Result<bool, PerpError> {
        if mark_price == 0 {
            return Err(PerpError::ZeroPrice);
        }
        if self.position.size == 0 {
            return Ok(false);
        }
        let required = risk.maintenance_margin_required(self.position.notional(mark_price)?)?;
        Ok(self.equity(mark_price)? < required)
    }
}

#[derive(Clone, Debug, Copy, PartialEq, Eq)]
pub struct FillSettlement {
    maker_realized_pnl: i128,
    taker_realized_pnl: i128,
}

pub fn settle_fill(
    maker: &mut IsolatedPerpAccount,
    taker: &mut IsolatedPerpAccount,
    maker_side: Side,
    quantity: u64,
    execution_price: u64,
    mark_price: u64,
    risk: &PerpRiskParams,
) -> Result<FillSettlement, PerpError> {
    let units = maker.position().units();
    if units != taker.position().units() || !units.price.accepts(mark_price) {
        return Err(PerpError::InvalidUnits);
    }
    let taker_side = match maker_side {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    };
    let mut next_maker = maker.clone();
    let mut next_taker = taker.clone();

    let maker_realized_pnl =
        next_maker.apply_fill(maker_side, quantity, execution_price, mark_price, risk)?;
    let taker_realized_pnl =
        next_taker.apply_fill(taker_side, quantity, execution_price, mark_price, risk)?;
    *maker = next_maker;
    *taker = next_taker;

    Ok(FillSettlement {
        maker_realized_pnl,
        taker_realized_pnl,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::market_config::DecimalStep;

    fn whole_units() -> PerpUnits {
        PerpUnits::new(
            DecimalStep::new(0, 1).unwrap(),
            DecimalStep::new(0, 1).unwrap(),
            0,
        )
        .unwrap()
    }

    fn btc_usd_units() -> PerpUnits {
        PerpUnits::new(
            DecimalStep::new(1, 1).unwrap(),
            DecimalStep::new(4, 1).unwrap(),
            6,
        )
        .unwrap()
    }

    fn risk() -> PerpRiskParams {
        PerpRiskParams {
            initial_margin_bps: 1_000,
            maintenance_margin_bps: 500,
        }
    }

    #[test]
    fn exact_basis_survives_rounding_and_partial_closes() {
        let mut position = PerpPosition::new(whole_units());
        position.apply_fill(Side::Buy, 1, 100).unwrap();
        position.apply_fill(Side::Buy, 1, 101).unwrap();
        assert_eq!(position.entry_price, 100); // Display average is rounded.
        assert_eq!(position.open_cost_basis(), 201);
        assert_eq!(position.unrealized_pnl(100).unwrap(), -1);
        assert_eq!(position.apply_fill(Side::Sell, 1, 100).unwrap(), 0);
        assert_eq!(position.open_cost_basis(), 101);
        assert_eq!(position.apply_fill(Side::Sell, 1, 100).unwrap(), -1);
        assert_eq!(position.realized_pnl, -1);
        assert_eq!(position.size, 0);
        assert_eq!(position.open_cost_basis(), 0);
    }

    #[test]
    fn short_reversal_preserves_realized_pnl_and_new_basis() {
        let mut position = PerpPosition::new(whole_units());
        position.apply_fill(Side::Sell, 2, 100).unwrap();
        assert_eq!(position.apply_fill(Side::Buy, 3, 90).unwrap(), 20);
        assert_eq!(position.size, 1);
        assert_eq!(position.open_cost_basis(), 90);
        assert_eq!(position.entry_price, 90);
        assert_eq!(position.realized_pnl, 20);
    }

    #[test]
    fn margin_rounds_up_and_uses_each_rate() {
        assert_eq!(risk().initial_margin_required(200_000).unwrap(), 20_000);
        assert_eq!(risk().maintenance_margin_required(200_000).unwrap(), 10_000);
        assert_eq!(risk().initial_margin_required(1).unwrap(), 1);
        assert_eq!(risk().maintenance_margin_required(1).unwrap(), 1);
        assert_eq!(
            risk().initial_margin_required(-1),
            Err(PerpError::InvalidNotional)
        );
    }

    #[test]
    fn fill_settles_pnl_and_failed_open_is_atomic() {
        let mut account = IsolatedPerpAccount::new(whole_units());
        account.deposit(10).unwrap();
        account.apply_fill(Side::Buy, 1, 100, 100, &risk()).unwrap();
        let before = account.clone();
        assert_eq!(
            account.apply_fill(Side::Buy, 1, 100, 100, &risk()),
            Err(PerpError::InsufficientMargin)
        );
        assert_eq!(account, before);
        assert_eq!(
            account
                .apply_fill(Side::Sell, 1, 110, 110, &risk())
                .unwrap(),
            10
        );
        assert_eq!(account.collateral(), 20);
        assert_eq!(account.position().realized_pnl, 10);
        assert_eq!(account.equity(110).unwrap(), 20); // Do not add historical PnL twice.
    }

    #[test]
    fn withdrawal_and_maintenance_use_mark_price() {
        let mut account = IsolatedPerpAccount::new(whole_units());
        account.deposit(10).unwrap();
        account.apply_fill(Side::Buy, 1, 100, 100, &risk()).unwrap();
        assert_eq!(
            account.withdraw(1, 100, &risk()),
            Err(PerpError::InsufficientMargin)
        );
        assert!(account.below_maintenance(94, &risk()).unwrap());
        account.apply_fill(Side::Sell, 1, 94, 94, &risk()).unwrap();
        assert_eq!(account.collateral(), 4);
    }

    #[test]
    fn fractional_btc_round_trip_realizes_exact_micro_usd() {
        let mut position = PerpPosition::new(btc_usd_units());
        assert_eq!(position.apply_fill(Side::Buy, 1, 1_000_001), Ok(0));
        assert_eq!(position.size(), 1); // 0.0001 BTC
        assert_eq!(position.entry_price(), 1_000_001); // $100000.1
        assert_eq!(position.open_cost_basis(), 10_000_010); // $10.000010
        assert_eq!(position.unrealized_pnl(1_000_002), Ok(10));
        assert_eq!(position.apply_fill(Side::Sell, 1, 1_000_002), Ok(10));
        assert_eq!(position.size(), 0);
        assert_eq!(position.open_cost_basis(), 0);
        assert_eq!(position.realized_pnl(), 10);
    }

    #[test]
    fn fractional_btc_loss_and_reversal_preserve_basis() {
        let mut position = PerpPosition::new(btc_usd_units());
        position.apply_fill(Side::Buy, 1, 1_000_002).unwrap();
        assert_eq!(position.apply_fill(Side::Sell, 2, 1_000_001), Ok(-10));
        assert_eq!(position.size(), -1);
        assert_eq!(position.open_cost_basis(), 10_000_010);
        assert_eq!(position.entry_price(), 1_000_001);
        assert_eq!(position.apply_fill(Side::Buy, 1, 1_000_000), Ok(10));
        assert_eq!(position.realized_pnl(), 0);
        assert_eq!(position.open_cost_basis(), 0);
    }

    #[test]
    fn isolated_margin_uses_micro_usd_not_raw_price_times_quantity() {
        let mut account = IsolatedPerpAccount::new(btc_usd_units());
        account.deposit(1_000_000).unwrap(); // $1 collateral
        account
            .apply_fill(Side::Buy, 1, 1_000_000, 1_000_000, &risk())
            .unwrap();
        assert_eq!(account.position().notional(1_000_000), Ok(10_000_000)); // $10
        let before = account.clone();
        assert_eq!(
            account.apply_fill(Side::Buy, 1, 1_000_000, 1_000_000, &risk()),
            Err(PerpError::InsufficientMargin)
        );
        assert_eq!(account, before);
        assert_eq!(
            account.apply_fill(Side::Sell, 1, 1_100_000, 1_100_000, &risk()),
            Ok(1_000_000)
        );
        assert_eq!(account.collateral(), 2_000_000);
    }

    #[test]
    fn settle_fill_opens_opposite_maker_and_taker_positions() {
        let mut maker = IsolatedPerpAccount::new(btc_usd_units());
        let mut taker = IsolatedPerpAccount::new(btc_usd_units());
        maker.deposit(2_000_000).unwrap();
        taker.deposit(2_000_000).unwrap();

        let settlement = settle_fill(
            &mut maker,
            &mut taker,
            Side::Sell,
            1,
            1_000_000,
            1_000_000,
            &risk(),
        )
        .unwrap();

        assert_eq!(maker.position().size(), -1);
        assert_eq!(taker.position().size(), 1);
        assert_eq!(maker.position().open_cost_basis(), 10_000_000);
        assert_eq!(taker.position().open_cost_basis(), 10_000_000);
        assert_eq!(settlement.maker_realized_pnl, 0);
        assert_eq!(settlement.taker_realized_pnl, 0);
    }

    #[test]
    fn failed_taker_margin_leaves_both_sides_unchanged() {
        let mut maker = IsolatedPerpAccount::new(btc_usd_units());
        let mut taker = IsolatedPerpAccount::new(btc_usd_units());
        maker.deposit(2_000_000).unwrap();
        let before_maker = maker.clone();
        let before_taker = taker.clone();

        assert_eq!(
            settle_fill(
                &mut maker,
                &mut taker,
                Side::Sell,
                1,
                1_000_000,
                1_000_000,
                &risk(),
            ),
            Err(PerpError::InsufficientMargin)
        );
        assert_eq!(maker, before_maker);
        assert_eq!(taker, before_taker);
    }
}
