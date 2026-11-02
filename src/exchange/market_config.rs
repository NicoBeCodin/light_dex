use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::account::AssetId;
use super::market::{BTC, USD};

/// Default BTC-USD spot market. Wire price 1 = $0.1; wire quantity 1 =
/// 0.0001 BTC. The corresponding asset balance scales live in market.rs.
pub fn btc_usd_spot_config() -> MarketConfig {
    MarketConfig {
        base_asset: BTC,
        quote_asset: USD,
        price: DecimalStep::new(1, 1).expect("valid BTC-USD tick"),
        quantity: DecimalStep::new(4, 1).expect("valid BTC-USD lot"),
        market_type: MarketType::Spot,
        fees: FeeSchedule::default(),
        maintenance_margin_bps: 0,
        funding_interval_updates: 0,
        max_funding_bps: 0,
        max_liquidation_slippage_bps: 0,
    }
}

/// Integer wire units: `decimals` determines the decimal point, while `step`
/// is the smallest permitted increment in those units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DecimalStep {
    pub decimals: u8,
    pub step: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecimalError {
    InvalidScale,
    InvalidStep,
    InvalidNumber,
    TooPrecise,
    NotOnStep,
    Overflow,
}

impl DecimalStep {
    pub fn new(decimals: u8, step: u64) -> Result<Self, DecimalError> {
        if decimals > 18 {
            return Err(DecimalError::InvalidScale);
        }
        if step == 0 {
            return Err(DecimalError::InvalidStep);
        }
        Ok(Self { decimals, step })
    }

    pub fn scale(self) -> u64 {
        10u64.pow(u32::from(self.decimals))
    }

    pub fn accepts(self, atoms: u64) -> bool {
        atoms > 0 && atoms % self.step == 0
    }

    /// Convert an oracle USD price at 10^-8 precision to this market's
    /// executable price tick. Nearest tick, ties upward, with checked math.
    pub fn round_e8_to_tick(self, price_e8: u64) -> Result<u64, DecimalError> {
        let step = u128::from(self.step);
        let denominator = 100_000_000u128
            .checked_mul(step)
            .ok_or(DecimalError::Overflow)?;
        let numerator = u128::from(price_e8)
            .checked_mul(u128::from(self.scale()))
            .and_then(|value| value.checked_add(denominator / 2))
            .ok_or(DecimalError::Overflow)?;
        let ticks = numerator / denominator;
        let atoms = ticks.checked_mul(step).ok_or(DecimalError::Overflow)?;
        let atoms = u64::try_from(atoms).map_err(|_| DecimalError::Overflow)?;
        self.accepts(atoms)
            .then_some(atoms)
            .ok_or(DecimalError::InvalidNumber)
    }

    pub fn format_u64(self, atoms: u64) -> String {
        format_magnitude(u128::from(atoms), self.decimals)
    }

    pub fn format_i128(self, atoms: i128) -> String {
        let magnitude = format_magnitude(atoms.unsigned_abs(), self.decimals);
        if atoms < 0 {
            format!("-{magnitude}")
        } else {
            magnitude
        }
    }

    /// Parse a decimal exactly. Reject extra precision and off-step values;
    /// transaction construction must never silently round a user's price.
    pub fn parse(self, text: &str) -> Result<u64, DecimalError> {
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        if whole.is_empty()
            || !whole.bytes().all(|ch| ch.is_ascii_digit())
            || !fraction.bytes().all(|ch| ch.is_ascii_digit())
        {
            return Err(DecimalError::InvalidNumber);
        }
        if fraction.len() > usize::from(self.decimals) {
            return Err(DecimalError::TooPrecise);
        }
        let whole: u64 = whole.parse().map_err(|_| DecimalError::Overflow)?;
        let fraction_len = fraction.len();
        let fraction: u64 = if fraction.is_empty() {
            0
        } else {
            fraction.parse().map_err(|_| DecimalError::Overflow)?
        };
        let units = whole
            .checked_mul(self.scale())
            .ok_or(DecimalError::Overflow)?
            .checked_add(
                fraction
                    .checked_mul(10u64.pow(u32::from(self.decimals) - fraction_len as u32))
                    .ok_or(DecimalError::Overflow)?,
            )
            .ok_or(DecimalError::Overflow)?;
        if !self.accepts(units) {
            return Err(DecimalError::NotOnStep);
        }
        Ok(units)
    }

    /// Round an externally observed price to a valid chain price. Bids round
    /// down, asks round up, so quoting never crosses the observed price.
    pub fn parse_bid(self, text: &str) -> Result<u64, DecimalError> {
        self.parse_rounded(text, false)
    }
    pub fn parse_ask(self, text: &str) -> Result<u64, DecimalError> {
        self.parse_rounded(text, true)
    }

    fn parse_rounded(self, text: &str, up: bool) -> Result<u64, DecimalError> {
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        if whole.is_empty()
            || !whole.bytes().all(|ch| ch.is_ascii_digit())
            || !fraction.bytes().all(|ch| ch.is_ascii_digit())
        {
            return Err(DecimalError::InvalidNumber);
        }
        let scale = u128::from(self.scale());
        let whole: u128 = whole.parse().map_err(|_| DecimalError::Overflow)?;
        let fractional: u128 = if fraction.is_empty() {
            0
        } else {
            fraction.parse().map_err(|_| DecimalError::Overflow)?
        };
        let fraction_scale = 10u128
            .checked_pow(fraction.len() as u32)
            .ok_or(DecimalError::Overflow)?;
        let numerator = whole
            .checked_mul(fraction_scale)
            .ok_or(DecimalError::Overflow)?
            .checked_add(fractional)
            .ok_or(DecimalError::Overflow)?
            .checked_mul(scale)
            .ok_or(DecimalError::Overflow)?;
        let denominator = fraction_scale
            .checked_mul(u128::from(self.step))
            .ok_or(DecimalError::Overflow)?;
        let steps = numerator / denominator + u128::from(up && numerator % denominator != 0);
        let atoms = steps
            .checked_mul(u128::from(self.step))
            .ok_or(DecimalError::Overflow)?;
        let atoms = u64::try_from(atoms).map_err(|_| DecimalError::Overflow)?;
        if atoms == 0 {
            return Err(DecimalError::NotOnStep);
        }
        Ok(atoms)
    }
}

fn format_magnitude(atoms: u128, decimals: u8) -> String {
    if decimals == 0 {
        return atoms.to_string();
    }
    let scale = 10u128.pow(u32::from(decimals));
    format!(
        "{}.{:0width$}",
        atoms / scale,
        atoms % scale,
        width = usize::from(decimals)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MarketType {
    Spot,
    Perp { max_leverage: u32 },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct FeeSchedule {
    pub maker_bps: u64,
    pub taker_bps: u64,
    pub liquidation_bps: u64,
    pub keeper_reward_bps: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct MarketConfig {
    pub base_asset: AssetId,
    pub quote_asset: AssetId,
    pub price: DecimalStep,
    pub quantity: DecimalStep,
    pub market_type: MarketType,
    pub fees: FeeSchedule,
    pub maintenance_margin_bps: u64,
    pub funding_interval_updates: u64,
    pub max_funding_bps: u64,
    pub max_liquidation_slippage_bps: u64,
}

impl MarketConfig {
    pub fn validate(self) -> Result<(), &'static str> {
        if self.base_asset == self.quote_asset {
            return Err("base and quote must differ");
        }
        DecimalStep::new(self.price.decimals, self.price.step).map_err(|_| "invalid price tick")?;
        DecimalStep::new(self.quantity.decimals, self.quantity.step)
            .map_err(|_| "invalid quantity lot")?;
        if matches!(self.market_type, MarketType::Perp { max_leverage: 0 }) {
            return Err("max leverage must be positive");
        }
        if self.fees.maker_bps > 1_000
            || self.fees.taker_bps > 1_000
            || self.fees.liquidation_bps > 1_000
            || self.fees.keeper_reward_bps > 1_000
            || self.max_funding_bps > 1_000
            || self.max_liquidation_slippage_bps > 1_000
        {
            return Err("fee or funding rate exceeds 10%");
        }
        match self.market_type {
            MarketType::Spot
                if self.maintenance_margin_bps != 0
                    || self.funding_interval_updates != 0
                    || self.max_funding_bps != 0
                    || self.max_liquidation_slippage_bps != 0
                    || self.fees.keeper_reward_bps != 0 =>
            {
                return Err("spot market cannot have perp risk parameters");
            }
            MarketType::Perp { max_leverage } => {
                let initial = 10_000_u64.div_ceil(u64::from(max_leverage));
                if self.maintenance_margin_bps == 0
                    || self.maintenance_margin_bps >= initial
                    || self.funding_interval_updates == 0
                {
                    return Err("invalid perp margin or funding parameters");
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_parses_integer_and_fractional_units() {
        let eth = DecimalStep::new(1, 1).unwrap();
        let sol = DecimalStep::new(2, 1).unwrap();
        assert_eq!(eth.format_u64(1234), "123.4");
        assert_eq!(eth.parse("123.4"), Ok(1234));
        assert_eq!(sol.format_i128(-1234), "-12.34");
        assert_eq!(
            sol.format_i128(i128::MIN),
            "-1701411834604692317316873037158841057.28"
        );
        assert_eq!(sol.parse("12.34"), Ok(1234));
        assert_eq!(sol.parse("12.345"), Err(DecimalError::TooPrecise));
    }

    #[test]
    fn external_prices_round_outward_to_market_tick() {
        let step = DecimalStep::new(2, 5).unwrap();
        assert_eq!(step.parse_bid("12.349"), Ok(1230));
        assert_eq!(step.parse_ask("12.349"), Ok(1235));
        assert_eq!(step.parse("12.34"), Err(DecimalError::NotOnStep));
        assert_eq!(step.parse("12.35"), Ok(1235));
    }
}
