use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::io;

use crate::{
    blockchain::Transaction,
    consensus::ConsensusMessage,
    crypto::{PeerHandshake, SignedConsensusMessage, SignedTransaction},
    node::ControlMessage,
};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum Message {
    Consensus(ConsensusMessage),
    SignedConsensus(SignedConsensusMessage),
    Control(ControlMessage),
    Transaction(Transaction),
    SignedTransaction(SignedTransaction),
    PeerHandshake(PeerHandshake),
}

#[derive(Serialize, Deserialize)]
struct AuthEnvelope {
    payload: String,
    mac: String,
}

pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn hex_decode<const N: usize>(value: &str) -> io::Result<[u8; N]> {
    if value.len() != N * 2 { return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid hex length")); }
    let mut bytes = [0u8; N];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid hex"))?;
    }
    Ok(bytes)
}

pub fn sign_line<T: Serialize>(value: &T, key: &[u8; 32]) -> io::Result<String> {
    let payload = serde_json::to_string(value).map_err(io::Error::other)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(io::Error::other)?;
    mac.update(payload.as_bytes());
    let envelope = AuthEnvelope { payload, mac: hex_encode(&mac.finalize().into_bytes()) };
    let mut line = serde_json::to_string(&envelope).map_err(io::Error::other)?;
    line.push('\n');
    Ok(line)
}

pub fn verify_line<T: DeserializeOwned>(line: &str, key: &[u8; 32]) -> io::Result<T> {
    let envelope: AuthEnvelope = serde_json::from_str(line).map_err(io::Error::other)?;
    let tag = hex_decode::<32>(&envelope.mac)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(io::Error::other)?;
    mac.update(envelope.payload.as_bytes());
    mac.verify_slice(&tag).map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "authentication failed"))?;
    serde_json::from_str(&envelope.payload).map_err(io::Error::other)
}

impl Message {
    pub fn to_vec(&self) -> Result<Vec<u8>, std::io::Error> {
        serde_json::to_vec(self).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error,
            )
        })
    }

    pub fn from_vec(bytes: &[u8]) -> Result<Self, std::io::Error> {
        serde_json::from_slice(bytes).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error,
            )
        })
    }

    pub fn to_str(&self) -> Result<String, std::io::Error> {
        serde_json::to_string(self).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error,
            )
        })
    }

    pub fn from_str(string: String) -> Result<Self, std::io::Error> {
        serde_json::from_str(&string).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Message, sign_line, verify_line};
    use crate::node::ControlMessage;

    #[test]
    fn authenticated_line_rejects_tampering_and_wrong_key() {
        let key = [7u8; 32];
        let message = Message::Control(ControlMessage::Shutdown);
        let line = sign_line(&message, &key).unwrap();
        assert!(matches!(verify_line::<Message>(&line, &key).unwrap(), Message::Control(ControlMessage::Shutdown)));
        assert!(verify_line::<Message>(&line, &[8u8; 32]).is_err());
        let tampered = line.replace("Shutdown", "Status");
        assert!(verify_line::<Message>(&tampered, &key).is_err());
    }
}
