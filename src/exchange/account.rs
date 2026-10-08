use std::collections::HashMap;

pub type AssetId = u64;
pub type AccountId = super::address::Address;

#[derive(Debug, Clone)]
pub struct Balance {
    pub available: u64,
    pub locked: u64,
}

#[derive(Debug, Clone)]
pub struct Account{
    pub id: AccountId,
    pub nonce: u64,
    pub balances: HashMap<AssetId, Balance>,
}
