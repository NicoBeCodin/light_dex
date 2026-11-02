use im::HashMap;

pub type AssetId = u64;
pub type AccountId = super::address::Address;

#[derive(Debug, Clone)]
pub struct Balance {
    pub available: u64,
    pub locked: u64,
}

#[derive(Debug, Clone)]
pub struct Account {
    pub id: AccountId,
    /// One more than the largest consumed nonce. This is a client hint, not
    /// an execution dependency: unused lower nonces remain valid in-window.
    pub nonce: u64,
    /// Bit 0 records nonce-1, bit 127 records nonce-128. A fixed-size window
    /// makes replay checks and account-state hashing bounded under load.
    pub used_nonces: [u64; 2],
    pub balances: HashMap<AssetId, Balance>,
}

impl Account {
    pub const NONCE_WINDOW: u64 = 128;

    pub fn accepts_nonce(&self, value: u64) -> bool {
        if value == u64::MAX {
            return false;
        }
        if value >= self.nonce {
            return value - self.nonce < Self::NONCE_WINDOW;
        }
        let distance = self.nonce - 1 - value;
        distance < Self::NONCE_WINDOW
            && self.used_nonces[(distance / 64) as usize] & (1u64 << (distance % 64)) == 0
    }

    pub fn consume_nonce(&mut self, value: u64) {
        debug_assert!(self.accepts_nonce(value));
        if value >= self.nonce {
            let shift = (value - self.nonce + 1) as u32;
            self.used_nonces = if shift >= 128 {
                [0, 0]
            } else if shift >= 64 {
                [0, self.used_nonces[0] << (shift - 64)]
            } else {
                [
                    self.used_nonces[0] << shift,
                    (self.used_nonces[1] << shift) | (self.used_nonces[0] >> (64 - shift)),
                ]
            };
            self.used_nonces[0] |= 1;
            self.nonce = value + 1;
        } else {
            let distance = self.nonce - 1 - value;
            self.used_nonces[(distance / 64) as usize] |= 1u64 << (distance % 64);
        }
    }

    pub fn nonce_used(&self, value: u64) -> bool {
        if value >= self.nonce {
            return false;
        }
        let distance = self.nonce - 1 - value;
        distance >= Self::NONCE_WINDOW
            || self.used_nonces[(distance / 64) as usize] & (1u64 << (distance % 64)) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_of_order_nonces_are_single_use_with_a_bounded_window() {
        let mut account = Account {
            id: 1.into(),
            nonce: 0,
            used_nonces: [0; 2],
            balances: HashMap::new(),
        };
        account.consume_nonce(2);
        assert_eq!(account.nonce, 3);
        assert!(account.accepts_nonce(0));
        assert!(account.accepts_nonce(1));
        assert!(!account.accepts_nonce(2));
        account.consume_nonce(0);
        account.consume_nonce(1);
        assert!(!account.accepts_nonce(0));
        assert!(!account.accepts_nonce(1));
        account.consume_nonce(66);
        assert!(!account.accepts_nonce(2));
        assert!(account.accepts_nonce(3));
        account.consume_nonce(130);
        assert!(!account.accepts_nonce(1));
        assert!(!account.accepts_nonce(259));
        assert!(!account.accepts_nonce(u64::MAX));
    }
}
