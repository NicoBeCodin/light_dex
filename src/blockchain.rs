use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as ShaDigest;
use sha2::Sha256;

use crate::exchange::{ExchangeError, ExchangeState};
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
    committed_transactions: HashSet<Digest>,

    pub exchange_state: ExchangeState,
    storage_path: Option<PathBuf>,
}

impl BlockchainState {
    pub fn new() -> Self {
        BlockchainState {
            blocks: Vec::new(),
            mempool: Vec::new(),
            seen_transactions: HashSet::new(),
            committed_transactions: HashSet::new(),
            exchange_state: ExchangeState::default(),
            storage_path: None,

        }
    }

    pub fn load(path: PathBuf) -> Result<Self, String> {
        let mut state = Self::new();
        if path.exists() {
            let mut file = File::open(&path).map_err(|error| error.to_string())?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).map_err(|error| error.to_string())?;
            let blocks: Vec<Block> = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            for block in blocks {
                state.commit_block(&block)?;
            }
        }
        state.storage_path = Some(path);
        Ok(state)
    }

    fn persist(&self, blocks: &[Block]) -> Result<(), String> {
        let Some(path) = &self.storage_path else { return Ok(()); };
        let parent = path.parent().ok_or("invalid storage path")?;
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let temporary = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec(blocks).map_err(|error| error.to_string())?;
        let mut file = File::create(&temporary).map_err(|error| error.to_string())?;
        file.write_all(&bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::rename(&temporary, path).map_err(|error| error.to_string())?;
        File::open(parent).and_then(|directory| directory.sync_all()).map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn validate_block(&self, block: &Block) -> Result<(), String> {
        let expected_sequence = self.blocks.len() as u64;
        if block.sequence != expected_sequence { return Err(format!("expected sequence {expected_sequence}, got {}", block.sequence)); }
        let expected_previous = self.blocks.last().map(Block::to_digest).unwrap_or_default();
        if block.previous_hash != expected_previous { return Err("previous hash mismatch".into()); }
        if block.sequence == 0 && !block.transactions.is_empty() { return Err("genesis must be empty".into()); }
        let mut digests = HashSet::new();
        if block.transactions.iter().any(|tx| !digests.insert(tx.to_digest())) { return Err("duplicate transaction in block".into()); }
        if digests.iter().any(|digest| self.committed_transactions.contains(digest)) { return Err("already committed transaction".into()); }
        let mut exchange = self.exchange_state.clone();
        for tx in &block.transactions { let _ = exchange.execute(tx); }
        if block.state_root != exchange.state_digest() { return Err("state root mismatch".into()); }
        Ok(())
    }

    pub fn commit_block(&mut self, block: &Block) -> Result<Vec<Result<(), ExchangeError>>, String> {
        self.validate_block(block)?;
        let mut next_blocks = self.blocks.clone();
        next_blocks.push(block.clone());
        self.persist(&next_blocks)?;
        let mut outcomes = Vec::with_capacity(block.transactions.len());
        for (i, tx) in block.transactions.iter().enumerate() {
            let result = self.exchange_state.execute(&tx);
            if result.is_err() {
                eprintln!("Error executing tx {i} : {tx:?}");
                // A failed transaction did not consume its nonce. It may be retried
                // after the account state changes, including after a restart.
                self.seen_transactions.remove(&tx.to_digest());
            } else {
                self.seen_transactions.insert(tx.to_digest());
                self.committed_transactions.insert(tx.to_digest());
            }
            outcomes.push(result);
        }

        self.blocks = next_blocks;
        Ok(outcomes)
    }
    pub fn remove_included_txs_from_mempool(&mut self, committed_block: &Block) {
        self.mempool = self
            .mempool
            .drain(..)
            .filter(|tx| !committed_block.transactions.contains(tx))
            .collect();
        // A different transaction may have consumed the same account nonce.
        // Such leftovers cannot ever become executable, including after a
        // view change, and must not swamp the new primary's mempool.
        self.mempool.retain(|tx| self.exchange_state
            .get_nonce_for_account(tx.account_id())
            .is_ok_and(|nonce| tx.nonce() >= nonce));
        let pending_digests: HashSet<_> = self.mempool.iter().map(Transaction::to_digest).collect();
        self.seen_transactions.retain(|digest| self.committed_transactions.contains(digest)
            || pending_digests.contains(digest));
    }
    pub fn add_transaction(&mut self, tx: &Transaction) -> bool {
        if !self.exchange_state.get_nonce_for_account(tx.account_id())
            .is_ok_and(|nonce| tx.nonce() >= nonce) {
            return false;
        }
        if self.seen_transactions.insert(tx.to_digest()) {
            self.mempool.push(tx.clone());
            return true;
        } else {
            false
        }
    }
    pub fn build_block(&self) -> Option<Block> {
        if self.mempool.is_empty() {
            println!("Empty mempool");
            return None;
        }
        let previous_block = self.blocks.last()?;
        let transactions = self.build_executable_transactions();
        if transactions.is_empty() {
            println!("Empty executable transactions");
            return None;
        }


        let mut exchange = self.exchange_state.clone();
        for tx in &transactions { let _ = exchange.execute(tx); }
        let block = Block {
            transactions: transactions,
            sequence: previous_block.sequence + 1,
            previous_hash: previous_block.to_digest(),
            state_root: exchange.state_digest(),
        };

        Some(block)
    }

    pub fn build_executable_transactions(&self)->Vec<Transaction>{


        let mut per_account: HashMap<AccountId, Vec<PendingTransaction>> = HashMap::new();
        for (i,tx ) in self.mempool.iter().enumerate() {
            per_account.entry(tx.account_id()).or_default().push(PendingTransaction {
                arrival_index: i,
                transaction: tx.clone()
            });

        }
        let mut queues: HashMap<AccountId, VecDeque<PendingTransaction>> = HashMap::new();
        for (account_id, mut txs) in per_account {
            txs.sort_by_key(|tx| tx.transaction.nonce());
            let Ok(mut expected_nonce) = self.exchange_state.get_nonce_for_account(account_id) else {
                continue;
            };
            let mut queue = VecDeque::new();
            for pending in txs {
                if pending.transaction.nonce() < expected_nonce {
                    //old tx so ignore
                    continue;
                }
                if pending.transaction.nonce() > expected_nonce {
                    //Missing nonce so we must wait until
                    break;
                }
                queue.push_back(pending);
                expected_nonce +=1;

            }
            if !queue.is_empty() {
                queues.insert(account_id, queue);
            }

        }

        let mut heap: BinaryHeap<(Reverse<usize>, AccountId)> =BinaryHeap::new();
        for (account_id, queue) in &queues {
            if let Some(front) = queue.front() {
                heap.push((Reverse(front.arrival_index), *account_id));
            }

        }

        let mut selected: Vec<Transaction> = Vec::new();

        while let Some((Reverse(_arrival_index), account_id)) =heap.pop() {
            let queue = queues.get_mut(&account_id).expect("Heap referenced missing account queue");
            let pending = queue.pop_front().expect("heap referenced empty account queue");
            selected.push(pending.transaction);
            if let Some(next) = queue.front() {
                heap.push((Reverse(next.arrival_index), account_id));
            }
        }
        selected






    }
}

#[derive(Debug, Clone)]
struct PendingTransaction{
    arrival_index: usize,
    transaction: Transaction,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct Block {
    pub sequence: u64,
    pub previous_hash: Digest,
    pub transactions: Vec<Transaction>,
    pub state_root: Digest,
}

impl Block {
    pub fn new() -> Block {
        Block {
            transactions: Vec::new(),
            state_root: ExchangeState::default().state_digest(),
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
        hasher.update(&self.state_root);
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

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq,  ts_rs::TS)]
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
    pub fn account_id(&self)->AccountId{
        let account_id = match self {
            Transaction::Transfer { from, to: _,  amount: _, nonce: _, asset:_ } => from,
            Transaction::PlaceOrder { account_id, market_id: _, order_type:_ , side:_, price:_, quantity:_, nonce:_ } => account_id,
            Transaction::CancelOrder { account_id, market_id:_, order_id:_, nonce:_ } => account_id,
        };

        *account_id

    }
    pub fn nonce(&self)->AccountId{
        let nonce = match self {
            Transaction::Transfer { from: _, to: _,  amount: _, nonce, asset:_ } => nonce,
            Transaction::PlaceOrder { account_id: _ , market_id: _, order_type:_ , side:_, price:_, quantity:_, nonce } => nonce,
            Transaction::CancelOrder { account_id: _, market_id:_, order_id:_, nonce } => nonce,
        };

        *nonce

    }
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

#[cfg(test)]
mod tests {
    use super::{Block, BlockchainState, Transaction};

    #[test]
    fn executable_queue_keeps_later_valid_nonce_for_same_account() {
        let mut state = BlockchainState::new();
        state.exchange_state.accounts.get_mut(&0).unwrap().nonce = 1;
        for (nonce, amount) in [(0, 1), (1, 2), (2, 3)] {
            state.add_transaction(&Transaction::Transfer {
                from: 0,
                to: 1,
                asset: 0,
                amount,
                nonce,
            });
        }
        let selected = state.build_executable_transactions();
        assert_eq!(selected.iter().map(Transaction::nonce).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn rejects_wrong_state_root_and_recovers_committed_blocks() {
        let directory = std::env::temp_dir().join(format!("light-dex-chain-{}", rand::random::<u64>()));
        let path = directory.join("node-0.json");
        let mut state = BlockchainState::load(path.clone()).unwrap();
        state.commit_block(&Block::new_initial_block()).unwrap();
        let tx = Transaction::Transfer { from: 0, to: 1, asset: 0, amount: 5, nonce: 0 };
        state.add_transaction(&tx);
        let block = state.build_block().unwrap();
        let mut corrupt = block.clone();
        corrupt.state_root[0] ^= 1;
        assert!(state.commit_block(&corrupt).is_err());
        state.commit_block(&block).unwrap();
        let recovered = BlockchainState::load(path.clone()).unwrap();
        assert_eq!(recovered.blocks.len(), 2);
        assert_eq!(recovered.exchange_state.state_digest(), state.exchange_state.state_digest());
        assert_eq!(recovered.exchange_state.accounts[&0].nonce, 1);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn failed_transaction_keeps_nonce_and_can_be_retried_after_recovery() {
        let directory = std::env::temp_dir().join(format!("light-dex-retry-{}", rand::random::<u64>()));
        let path = directory.join("node-0.json");
        let mut state = BlockchainState::load(path.clone()).unwrap();
        state.commit_block(&Block::new_initial_block()).unwrap();
        let tx = Transaction::Transfer { from: 0, to: 1, asset: 0, amount: 2_000_000, nonce: 0 };
        assert!(state.add_transaction(&tx));
        let block = state.build_block().unwrap();
        assert!(state.commit_block(&block).unwrap()[0].is_err());
        assert_eq!(state.exchange_state.accounts[&0].nonce, 0);
        let mut recovered = BlockchainState::load(path.clone()).unwrap();
        assert_eq!(recovered.exchange_state.accounts[&0].nonce, 0);
        assert!(recovered.add_transaction(&tx));
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn committed_nonce_prunes_conflicting_mempool_entries() {
        let mut state = BlockchainState::new();
        state.commit_block(&Block::new_initial_block()).unwrap();
        let chosen = Transaction::Transfer { from: 0, to: 1, asset: 0, amount: 1, nonce: 0 };
        let conflict = Transaction::Transfer { from: 0, to: 2, asset: 0, amount: 1, nonce: 0 };
        let next = Transaction::Transfer { from: 0, to: 1, asset: 0, amount: 1, nonce: 1 };
        assert!(state.add_transaction(&chosen));
        assert!(state.add_transaction(&conflict));
        assert!(state.add_transaction(&next));
        let block = state.build_block().unwrap();
        assert_eq!(block.transactions, vec![chosen.clone(), next.clone()]);
        state.commit_block(&block).unwrap();
        state.remove_included_txs_from_mempool(&block);
        assert!(state.mempool.is_empty());
        assert!(!state.add_transaction(&conflict));
        assert!(!state.add_transaction(&chosen));
    }
}
