use serde::{Deserialize, Serialize};

use crate::{blockchain::{Block, Transaction}, consensus::pbft::ConsensusMessage, node::ControlMessage};


#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum Message {
    Consensus(ConsensusMessage),
    Control(ControlMessage),
    Transaction(Transaction),
    

}

impl Message {
    
    pub fn to_vec(&self)->Result<Vec<u8>, std::io::Error>{
        Ok(serde_json::to_vec(&self).expect("Failed serializing"))   
    }
    
    pub fn from_vec(bytes: &[u8])->Result<Self, std::io::Error>{
        Ok(serde_json::from_slice(bytes)?)
    }

    pub fn to_str(&self)->Result<String, std::io::Error>{
        Ok(serde_json::to_string(&self).expect("Failed converting to string"))
    }

    pub fn from_str(string: String)->Result<Self, std::io::Error>{
        Ok(serde_json::from_str(string.as_str())?)
    }
}
