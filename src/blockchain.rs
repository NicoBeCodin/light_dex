use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as ShaDigest;
use sha2::Sha256;
pub type Digest = [u8; 32];

#[derive(Debug)]
pub struct BlockchainState {
    pub blocks: Vec<Block>,
    pub mempool: Vec<Transaction>
}

impl BlockchainState {
    pub fn new() -> Self {
        BlockchainState { blocks: Vec::new(), mempool: Vec::new() }
    }
    pub fn add_block(&mut self, block: Block) {
        self.blocks.push(block)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct Block {
    pub sequence: u64,
    pub previous_hash: Digest,
    pub transactions: Vec<String>,
}



impl Block {
    pub fn new() -> Block {
        Block {
            transactions: Vec::new(),
            sequence: 0,
            previous_hash: Digest::default(),
        }
    }
    pub fn new_initial_block_with(txs: Vec<String>) -> Block {
        let mut block = Block::new();
        block.transactions = txs;
        block
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct Proposal {
    pub block: Block,
}

impl Proposal {
    pub fn to_digest(&self) -> Digest {
        let mut hasher = Sha256::new();
        for tx in &self.block.transactions {
            hasher.update(tx.as_bytes());
        }
        hasher.finalize().into()
    }

    pub fn get_block(&self)->Block {
        self.block.clone()
    }
 }

#[derive(Debug)]
pub struct Transaction {
    payload: String,
}
