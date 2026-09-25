use std::collections::HashSet;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as ShaDigest;
use sha2::Sha256;

use crate::exchange::ExchangeState;
use crate::exchange::account::AccountId;
use crate::exchange::account::AssetId;
use crate::exchange::market::MarketId;
use crate::exchange::order::OrderId;
use crate::exchange::order::OrderType;
use crate::exchange::order::Side;

pub type Digest = [u8; 32];


#[derive(Debug)]
pub struct BlockchainState {
    pub blocks: Vec<Block>,
    pub mempool: Vec<Transaction>,
    pub seen_transactions: HashSet<Digest>,

    pub exchange_state: ExchangeState,
}

impl BlockchainState {
    pub fn new() -> Self {
        BlockchainState {
            blocks: Vec::new(),
            mempool: Vec::new(),
            seen_transactions: HashSet::new(),
            exchange_state: ExchangeState::default()
            
        }
    }
    pub fn commit_block(&mut self, block: &Block) {
        for (i, tx) in block.transactions.iter().enumerate() {
            let result = self.exchange_state.execute(&tx);
            if result.is_err() {
                eprintln!("Error executing tx {i} : {tx:?}");
            }
        }
        
        self.blocks.push(block.clone())
    }
    pub fn remove_included_txs_from_mempool(&mut self, committed_block: &Block) {
        self.mempool = self
            .mempool
            .drain(..)
            .filter(|tx| !committed_block.transactions.contains(tx))
            .collect();
    }
    pub fn add_transaction(&mut self, tx: &Transaction) -> bool {
        if self.seen_transactions.insert(tx.to_digest()) {
            self.mempool.push(tx.clone());
            return true;
        } else {
            false
        }
    }
    pub fn build_block(&self) -> Option<Block> {
        if self.mempool.is_empty() {
            return None;
        }
        let previous_block = self.blocks.last()?;

        let block = Block {
            transactions: self.mempool.clone(),
            sequence: previous_block.sequence + 1,
            previous_hash: previous_block.to_digest(),
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
    pub fn new_initial_block() -> Block {
        Block::new()
    }

    pub fn to_digest(&self) -> Digest {
        let mut hasher = Sha256::new();
        hasher.update(&self.sequence.to_le_bytes());
        hasher.update(&self.previous_hash);
        for tx in &self.transactions {
            hasher.update(tx.to_digest());
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
pub enum Transaction {
    Transfer {
        from: AccountId,
        to: AccountId,
        asset: AssetId,
        amount: u64,
        nonce: u64,
    },

    PlaceOrder {
        account_id: AccountId,
        market_id: MarketId,
        order_type: OrderType,
        side: Side,
        price: u64,
        quantity: u64,
        nonce: u64,
    },

    CancelOrder {
        account_id: AccountId,
        market_id: MarketId,
        order_id: OrderId,
        nonce: u64,
    },
}



impl Transaction {
    // pub fn new() -> Transaction {
    //     return Transaction {
    //         payload: String::new(),
    //     };
    // }
    // pub fn new_with(payload: String) -> Transaction {
    //     Transaction { payload }
    // }
    pub fn to_digest(&self) -> Digest {
        let mut hasher = Sha256::new();
        match self {
            Transaction::Transfer {
                from,
                to,
                asset,
                amount,
                nonce,
            } => {
                hasher.update(b"TRANSFER");
                hasher.update(from.to_le_bytes());
                hasher.update(to.to_le_bytes());
                hasher.update(asset.to_le_bytes());
                hasher.update(amount.to_le_bytes());
                hasher.update(nonce.to_le_bytes());
            }
            Transaction::PlaceOrder {
                account_id: account,
                market_id: market,
                side,
                order_type,
                price,
                quantity,
                nonce,
            } => {
                hasher.update(b"PLACE_ORDER");

                hasher.update(account.to_le_bytes());
                hasher.update(market.to_le_bytes());

                match side {
                    Side::Buy => hasher.update([0u8]),
                    Side::Sell => hasher.update([1u8]),
                }
                match order_type {
                    OrderType::IOC => hasher.update([0u8]),
                    OrderType::FOK => hasher.update([1u8]),
                    OrderType::LO => hasher.update([2u8]),
                }

                hasher.update(price.to_le_bytes());
                hasher.update(quantity.to_le_bytes());
                hasher.update(nonce.to_le_bytes());
            }

            Transaction::CancelOrder {
                account_id: account,
                market_id: market,
                order_id,
                nonce,
            } => {
                hasher.update(b"CANCEL_ORDER");

                hasher.update(account.to_le_bytes());
                hasher.update(market.to_le_bytes());
                hasher.update(order_id.to_le_bytes());
                hasher.update(nonce.to_le_bytes());
            }
        }
        hasher.finalize().into()
    }
}
