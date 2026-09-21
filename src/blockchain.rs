
use std::collections::HashSet;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as ShaDigest;
use sha2::Sha256;
pub type Digest = [u8; 32];

#[derive(Debug)]
pub struct BlockchainState {
    pub blocks: Vec<Block>,
    pub mempool: Vec<Transaction>,
    pub seen_transactions: HashSet<Digest>,
}

impl BlockchainState {
    pub fn new() -> Self {
        BlockchainState {
            blocks: Vec::new(),
            mempool: Vec::new(),
            seen_transactions: HashSet::new(),
        }
    }
    pub fn commit_block(&mut self, block: &Block) {
        self.blocks.push(block.clone())
    }
    pub fn remove_included_txs_from_mempool(&mut self, committed_block: &Block) {
        self.mempool = self
            .mempool
            .drain(..)
            .filter(|tx| !committed_block.transactions.contains(tx))
            .collect();
    }
    pub fn add_transaction(&mut self, tx: &Transaction)->bool{
        if self.seen_transactions.insert(tx.to_digest()) {
            self.mempool.push(tx.clone());
            return true
        } else {
            false
        }
        
    }
    pub fn build_block(&self)->Option<Block>{
        if self.mempool.is_empty() {
            return None
        }
        let previous_block = self.blocks.last()?;
        
        let block = Block {
            transactions: self.mempool.clone(),
            sequence: previous_block.sequence +1,
            previous_hash: previous_block.to_digest()
        };
        
        Some(block)
        
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct Block {
    pub sequence: u64,
    pub previous_hash: Digest,
    pub transactions: Vec<Transaction>,
}

impl Block {
    pub fn new() -> Block {
        Block {
            transactions: Vec::new(),
            sequence: 0,
            previous_hash: Digest::default(),
        }
    }
    pub fn new_initial_block_with(txs: Vec<Transaction>) -> Block {
        let mut block = Block::new();
        block.transactions = txs;
        block
    }

        pub fn to_digest(&self) -> Digest {
        let mut hasher = Sha256::new();
        hasher.update(&self.sequence.to_le_bytes());
        hasher.update(&self.previous_hash);
        for tx in &self.transactions {
            hasher.update(tx.payload.as_bytes());
        }
        hasher.finalize().into()
    }


}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct Proposal {
    pub block: Block,
}

impl Proposal {
    pub fn to_digest(&self) -> Digest {
        self.block.to_digest()
    }

    pub fn get_block(&self) -> Block {
        self.block.clone()
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Transaction {
    payload: String,
}

impl Transaction {
    pub fn new() -> Transaction {
        return Transaction {
            payload: String::new(),
        };
    }
    pub fn new_with(payload: String) -> Transaction {
        Transaction { payload }
    }
    pub fn to_digest(&self)->Digest{
        let mut hasher = Sha256::new();
        hasher.update(self.payload.as_bytes());
        hasher.finalize().into()
    }
}
