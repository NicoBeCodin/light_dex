use serde::{Deserialize, Serialize};

use crate::{
    blockchain::Transaction,
    consensus::pbft::ConsensusMessage,
    node::ControlMessage,
};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum Message {
    Consensus(ConsensusMessage),
    Control(ControlMessage),
    Transaction(Transaction),
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
