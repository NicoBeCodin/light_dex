use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use hmac::{Hmac, KeyInit, Mac};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
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
    /// Bounded validator-to-validator transaction handoff. The receiver
    /// verifies every account signature before admitting any transaction.
    SignedTransactionBatch {
        transactions: Vec<SignedTransaction>,
        prewarm: bool,
    },
    /// Client submission with a signed admission response (not a commit receipt).
    SubmitTransaction(SignedTransaction),
    PeerHandshake(PeerHandshake),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AdmissionReply {
    pub digest: String,
    pub accepted: bool,
    pub error: Option<String>,
    #[serde(default)]
    pub leader: Option<(u64, u64)>, // (view, validator); routing hint only
}

#[derive(Serialize, Deserialize)]
struct AuthEnvelope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    payload: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compressed_payload: Option<String>,
    mac: String,
}

const MAX_CONSENSUS_PAYLOAD: usize = 8 * 1024 * 1024;
const COMPRESS_FROM: usize = 8 * 1024;
const COMPRESSED_DOMAIN: &[u8] = b"light-dex/zstd-consensus/v1\0";

pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 15) as usize] as char);
    }
    result
}

pub fn hex_decode<const N: usize>(value: &str) -> io::Result<[u8; N]> {
    if value.len() != N * 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid hex length",
        ));
    }
    let mut bytes = [0u8; N];
    fn nibble(byte: u8) -> io::Result<u8> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            b'A'..=b'F' => Ok(byte - b'A' + 10),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "invalid hex")),
        }
    }
    for (byte, pair) in bytes.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
        *byte = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Ok(bytes)
}

pub fn sign_line<T: Serialize>(value: &T, key: &[u8; 32]) -> io::Result<String> {
    let payload = serde_json::to_string(value).map_err(io::Error::other)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(io::Error::other)?;
    mac.update(payload.as_bytes());
    let envelope = AuthEnvelope {
        payload: Some(payload),
        compressed_payload: None,
        mac: hex_encode(&mac.finalize().into_bytes()),
    };
    let mut line = serde_json::to_string(&envelope).map_err(io::Error::other)?;
    line.push('\n');
    Ok(line)
}

/// Compress large validator messages. The same authenticated envelope is used
/// for consensus, batched peer transactions, and block-sync responses.
pub fn sign_consensus_line(message: &Message, key: &[u8; 32]) -> io::Result<String> {
    if !matches!(message, Message::Consensus(_) | Message::SignedConsensus(_)) {
        return sign_line(message, key);
    }
    if std::env::var_os("LIGHT_DEX_DISABLE_CONSENSUS_COMPRESSION").is_some() {
        return sign_line(message, key);
    }
    sign_compressed_line(message, key)
}

pub fn sign_compressed_line<T: Serialize>(value: &T, key: &[u8; 32]) -> io::Result<String> {
    sign_compressed_line_with_limit(value, key, MAX_CONSENSUS_PAYLOAD)
}

pub fn sign_compressed_line_with_limit<T: Serialize>(
    value: &T,
    key: &[u8; 32],
    limit: usize,
) -> io::Result<String> {
    let payload = serde_json::to_vec(value).map_err(io::Error::other)?;
    if payload.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "peer payload too large",
        ));
    }
    if payload.len() < COMPRESS_FROM {
        return sign_line(value, key);
    }
    let compressed = zstd::bulk::compress(&payload, 1)?;
    let encoded = BASE64.encode(compressed);
    if encoded.len() >= payload.len() {
        return sign_line(value, key);
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(io::Error::other)?;
    mac.update(COMPRESSED_DOMAIN);
    mac.update(encoded.as_bytes());
    let envelope = AuthEnvelope {
        payload: None,
        compressed_payload: Some(encoded),
        mac: hex_encode(&mac.finalize().into_bytes()),
    };
    let mut line = serde_json::to_string(&envelope).map_err(io::Error::other)?;
    line.push('\n');
    Ok(line)
}

pub fn verify_line<T: DeserializeOwned>(line: &str, key: &[u8; 32]) -> io::Result<T> {
    verify_line_with_limit(line, key, MAX_CONSENSUS_PAYLOAD)
}

pub fn verify_line_with_limit<T: DeserializeOwned>(
    line: &str,
    key: &[u8; 32],
    limit: usize,
) -> io::Result<T> {
    if line.len() > limit.saturating_mul(2) {
        return Err(io::Error::other("envelope too large"));
    }
    let envelope: AuthEnvelope = serde_json::from_str(line).map_err(io::Error::other)?;
    let (payload, compressed) = match (&envelope.payload, &envelope.compressed_payload) {
        (Some(payload), None) => (payload, false),
        (None, Some(payload)) => (payload, true),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid authenticated envelope",
            ));
        }
    };
    let tag = hex_decode::<32>(&envelope.mac)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(io::Error::other)?;
    if compressed {
        mac.update(COMPRESSED_DOMAIN);
    }
    mac.update(payload.as_bytes());
    mac.verify_slice(&tag)
        .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "authentication failed"))?;
    if compressed {
        let bytes = BASE64.decode(payload).map_err(io::Error::other)?;
        if bytes.len() > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "compressed consensus payload too large",
            ));
        }
        let decoded = zstd::bulk::decompress(&bytes, limit)?;
        serde_json::from_slice(&decoded).map_err(io::Error::other)
    } else {
        if payload.len() > limit {
            return Err(io::Error::other("payload too large"));
        }
        serde_json::from_str(payload).map_err(io::Error::other)
    }
}

impl Message {
    pub fn to_vec(&self) -> Result<Vec<u8>, std::io::Error> {
        serde_json::to_vec(self)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub fn from_vec(bytes: &[u8]) -> Result<Self, std::io::Error> {
        serde_json::from_slice(bytes)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub fn to_str(&self) -> Result<String, std::io::Error> {
        serde_json::to_string(self)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub fn from_str(string: String) -> Result<Self, std::io::Error> {
        serde_json::from_str(&string)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }
}

#[cfg(test)]
mod tests {
    use super::{Message, sign_compressed_line, sign_consensus_line, sign_line, verify_line};
    use crate::consensus::{
        ConsensusMessage,
        chained::{ChainedMessage, ChainedQC},
    };
    use crate::node::ControlMessage;

    #[test]
    fn authenticated_line_rejects_tampering_and_wrong_key() {
        let key = [7u8; 32];
        let message = Message::Control(ControlMessage::Shutdown);
        let line = sign_line(&message, &key).unwrap();
        assert!(matches!(
            verify_line::<Message>(&line, &key).unwrap(),
            Message::Control(ControlMessage::Shutdown)
        ));
        assert!(verify_line::<Message>(&line, &[8u8; 32]).is_err());
        let tampered = line.replace("Shutdown", "Status");
        assert!(verify_line::<Message>(&tampered, &key).is_err());
    }

    #[test]
    fn compressed_consensus_is_bounded_and_authenticated() {
        let key = [7u8; 32];
        let mut qc = ChainedQC::genesis();
        qc.signatures = vec!["abcd".repeat(80); 100];
        let message = Message::Consensus(ConsensusMessage::Chained(ChainedMessage::ViewNotice {
            view: 1,
            high_qc: qc,
        }));
        let line = sign_consensus_line(&message, &key).unwrap();
        assert!(line.contains("compressed_payload"));
        assert!(line.len() < sign_line(&message, &key).unwrap().len() / 2);
        let decoded: Message = verify_line(&line, &key).unwrap();
        assert!(matches!(
            decoded,
            Message::Consensus(ConsensusMessage::Chained(ChainedMessage::ViewNotice {
                view: 1,
                ..
            }))
        ));
        assert!(verify_line::<Message>(&line, &[8u8; 32]).is_err());
        let tampered = line.replacen("compressed_payload\":\"", "compressed_payload\":\"A", 1);
        assert!(verify_line::<Message>(&tampered, &key).is_err());
    }

    #[test]
    fn large_block_sync_payload_uses_authenticated_compression() {
        let key = [9u8; 32];
        let body = vec!["repeated block data".repeat(1_000)];
        let line = sign_compressed_line(&body, &key).unwrap();
        assert!(line.contains("compressed_payload"));
        assert_eq!(verify_line::<Vec<String>>(&line, &key).unwrap(), body);
    }
}
