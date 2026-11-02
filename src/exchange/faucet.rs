use serde::{Deserialize, Serialize};

/// Session-bound, finite mint allowance for one-time account claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaucetConfig {
    pub max_claims: u64,
    pub usd_per_claim: u64,
    pub btc_per_claim: u64,
}

impl FaucetConfig {
    pub fn validate(self) -> Result<(), String> {
        if self.max_claims == 0 || self.usd_per_claim == 0 || self.btc_per_claim == 0 {
            return Err("faucet claims and balances must be positive".into());
        }
        self.max_claims
            .checked_mul(self.usd_per_claim)
            .ok_or("maximum mintable USD overflow")?;
        self.max_claims
            .checked_mul(self.btc_per_claim)
            .ok_or("maximum mintable BTC overflow")?;
        Ok(())
    }
}
