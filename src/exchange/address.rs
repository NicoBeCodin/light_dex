//! Account identity in protocol v2. The display encoding is lowercase hex;
//! consensus and state commitments use the underlying 32 public-key bytes.
use std::{fmt, str::FromStr};

use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};

use crate::message::{hex_decode, hex_encode};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, ts_rs::TS)]
#[ts(type = "string")]
pub struct Address([u8; 32]);

impl Address {
    pub fn from_public_key(key: &VerifyingKey) -> Self {
        Self(key.to_bytes())
    }
    pub fn bytes(self) -> [u8; 32] {
        self.0
    }
    pub fn public_key(self) -> Result<VerifyingKey, String> {
        let key = VerifyingKey::from_bytes(&self.0).map_err(|error| error.to_string())?;
        if key.is_weak() {
            return Err("weak account address".into());
        }
        Ok(key)
    }

    /// Deterministic test/benchmark identity, never used to sign live sessions.
    pub fn fixture(index: u64) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"light-dex/fixture-account/v2");
        hash.update(index.to_be_bytes());
        let seed: [u8; 32] = hash.finalize().into();
        Self::from_public_key(&SigningKey::from_bytes(&seed).verifying_key())
    }
}

impl From<u64> for Address {
    fn from(index: u64) -> Self {
        Self::fixture(index)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex_encode(&self.0))
    }
}
impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Address({self})")
    }
}
impl FromStr for Address {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 || value != value.to_ascii_lowercase() {
            return Err("address must be 64 lowercase hexadecimal characters".into());
        }
        let bytes = hex_decode::<32>(value).map_err(|error| error.to_string())?;
        let address = Self(bytes);
        address.public_key()?;
        Ok(address)
    }
}
impl Serialize for Address {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for Address {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn address_round_trip_and_rejects_noncanonical_text() {
        let address = Address::fixture(7);
        assert_eq!(address.to_string().parse::<Address>().unwrap(), address);
        assert!(
            address
                .to_string()
                .to_ascii_uppercase()
                .parse::<Address>()
                .is_err()
        );
        assert_eq!(
            serde_json::from_str::<Address>(&serde_json::to_string(&address).unwrap()).unwrap(),
            address
        );
    }
}
