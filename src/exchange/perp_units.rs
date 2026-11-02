use super::market_config::DecimalStep;

/// Converts wire price/quantity atoms into the collateral asset's atoms.
/// For BTC-USD: price 100001 means $10,000.1, quantity 1 means 0.0001 BTC,
/// and quote_decimals 6 returns the notional in micro-USD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerpUnits {
    pub price: DecimalStep,
    pub quantity: DecimalStep,
    pub quote_decimals: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitError {
    InvalidScale,
    InvalidStep,
    InvalidValue,
    NonIntegralNotional,
    Overflow,
}

pub const USD_DECIMALS: u8 = 6;

impl PerpUnits {
    pub fn new(
        price: DecimalStep,
        quantity: DecimalStep,
        quote_decimals: u8,
    ) -> Result<Self, UnitError> {
        DecimalStep::new(price.decimals, price.step).map_err(|_| UnitError::InvalidStep)?;
        DecimalStep::new(quantity.decimals, quantity.step).map_err(|_| UnitError::InvalidStep)?;
        if quote_decimals > 18 {
            return Err(UnitError::InvalidScale);
        }
        let units = Self {
            price,
            quantity,
            quote_decimals,
        };
        // Every combination of on-step price and lot quantities must settle
        // exactly; otherwise individual fills could silently lose collateral.
        units.notional(price.step, quantity.step)?;
        Ok(units)
    }

    pub fn notional(self, price_atoms: u64, quantity_atoms: u64) -> Result<i128, UnitError> {
        if !self.price.accepts(price_atoms) || !self.quantity.accepts(quantity_atoms) {
            return Err(UnitError::InvalidValue);
        }
        let numerator = u128::from(price_atoms)
            .checked_mul(u128::from(quantity_atoms))
            .and_then(|v| v.checked_mul(10u128.pow(u32::from(self.quote_decimals))))
            .ok_or(UnitError::Overflow)?;
        let denominator = u128::from(self.price.scale())
            .checked_mul(u128::from(self.quantity.scale()))
            .ok_or(UnitError::Overflow)?;
        if numerator % denominator != 0 {
            return Err(UnitError::NonIntegralNotional);
        }
        i128::try_from(numerator / denominator).map_err(|_| UnitError::Overflow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn btc_fractional_lot_and_tick_settle_exactly() {
        let units = PerpUnits::new(
            DecimalStep::new(1, 1).unwrap(),
            DecimalStep::new(4, 1).unwrap(),
            6,
        )
        .unwrap();
        assert_eq!(units.notional(1, 1), Ok(10)); // $0.1 * 0.0001 BTC
        assert_eq!(units.notional(1_000_001, 1), Ok(10_000_010)); // $100000.1 * 0.0001 BTC
    }

    #[test]
    fn rejects_rounding_away_small_fills() {
        assert_eq!(
            PerpUnits::new(
                DecimalStep::new(6, 1).unwrap(),
                DecimalStep::new(6, 1).unwrap(),
                6
            ),
            Err(UnitError::NonIntegralNotional)
        );
    }
}
