//! Integer fee arithmetic and committed insurance balances.
use super::*;

pub(super) fn fee_atoms(amount: u64, bps: u64) -> Result<u64, ExchangeError> {
    let value = u128::from(amount)
        .checked_mul(u128::from(bps))
        .and_then(|value| value.checked_add(9_999))
        .ok_or(ExchangeError::ArithmeticOverflow)?
        / 10_000;
    u64::try_from(value).map_err(|_| ExchangeError::ArithmeticOverflow)
}

impl ExchangeState {
    pub fn insurance_balance(&self, asset: AssetId) -> u64 {
        self.insurance_assets.get(&asset).copied().unwrap_or(0)
    }

    pub(super) fn accrue_insurance(
        &mut self,
        asset: AssetId,
        amount: u64,
    ) -> Result<(), ExchangeError> {
        if amount == 0 {
            return Ok(());
        }
        let next = self
            .insurance_balance(asset)
            .checked_add(amount)
            .ok_or(ExchangeError::ArithmeticOverflow)?;
        self.insurance_assets.insert(asset, next);
        self.state_tree
            .put(StateKey::InsuranceAsset(asset), &next.to_be_bytes());
        Ok(())
    }
}
