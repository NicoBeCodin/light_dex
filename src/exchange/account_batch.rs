//! Per-transaction wallet changes. Contains only the touched accounts, not an
//! ExchangeState with empty market/index maps. Publish after all checks pass.
use super::{Account, AccountId, AssetId, Balance, ExchangeError, HashMap};

pub(super) struct AccountBatch {
    pub accounts: HashMap<AccountId, Account>,
}

impl AccountBatch {
    fn balance_mut(
        &mut self,
        account: AccountId,
        asset: AssetId,
    ) -> Result<&mut Balance, ExchangeError> {
        Ok(self
            .accounts
            .get_mut(&account)
            .ok_or(ExchangeError::AccountNotFound)?
            .balances
            .entry(asset)
            .or_insert(Balance {
                available: 0,
                locked: 0,
            }))
    }

    pub fn debit_available(
        &mut self,
        account: AccountId,
        asset: AssetId,
        amount: u64,
    ) -> Result<(), ExchangeError> {
        let balance = self.balance_mut(account, asset)?;
        balance.available = balance
            .available
            .checked_sub(amount)
            .ok_or(ExchangeError::InsufficientBalance)?;
        Ok(())
    }

    pub fn credit_available(
        &mut self,
        account: AccountId,
        asset: AssetId,
        amount: u64,
    ) -> Result<(), ExchangeError> {
        let balance = self.balance_mut(account, asset)?;
        balance.available = balance
            .available
            .checked_add(amount)
            .ok_or(ExchangeError::ArithmeticOverflow)?;
        Ok(())
    }

    pub fn lock(
        &mut self,
        account: AccountId,
        asset: AssetId,
        amount: u64,
    ) -> Result<(), ExchangeError> {
        self.debit_available(account, asset, amount)?;
        let balance = self.balance_mut(account, asset)?;
        balance.locked = balance
            .locked
            .checked_add(amount)
            .ok_or(ExchangeError::ArithmeticOverflow)?;
        Ok(())
    }

    pub fn spend_locked(
        &mut self,
        account: AccountId,
        asset: AssetId,
        amount: u64,
    ) -> Result<(), ExchangeError> {
        let balance = self.balance_mut(account, asset)?;
        balance.locked = balance
            .locked
            .checked_sub(amount)
            .ok_or(ExchangeError::InsufficientBalance)?;
        Ok(())
    }

    pub fn unlock(
        &mut self,
        account: AccountId,
        asset: AssetId,
        amount: u64,
    ) -> Result<(), ExchangeError> {
        self.spend_locked(account, asset, amount)?;
        self.credit_available(account, asset, amount)
    }

    pub fn advance_nonce(&mut self, account: AccountId, nonce: u64) {
        self.accounts
            .get_mut(&account)
            .expect("validated account")
            .consume_nonce(nonce);
    }

    pub fn transfer(
        &mut self,
        from: AccountId,
        to: AccountId,
        asset: AssetId,
        amount: u64,
        nonce: u64,
    ) -> Result<(), ExchangeError> {
        let sender = self
            .accounts
            .get(&from)
            .ok_or(ExchangeError::AccountNotFound)?;
        if !sender.accepts_nonce(nonce) {
            return Err(ExchangeError::InvalidNonce);
        }
        to.public_key()
            .map_err(|_| ExchangeError::AccountNotFound)?;
        if amount == 0 {
            return Err(ExchangeError::InvalidOrder);
        }
        self.debit_available(from, asset, amount)?;
        self.accounts.entry(to).or_insert_with(|| Account {
            id: to,
            nonce: 0,
            used_nonces: [0; 2],
            balances: Default::default(),
        });
        self.credit_available(to, asset, amount)?;
        self.advance_nonce(from, nonce);
        Ok(())
    }
}
