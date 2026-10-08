use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as ShaDigest;
use sha2::Sha256;

use crate::exchange::account::AccountId;
use crate::exchange::account::AssetId;
use crate::exchange::market::MarketId;
use crate::exchange::order::OrderId;
use crate::exchange::order::OrderType;
use crate::exchange::order::Side;
use crate::exchange::{ExchangeError, ExchangeState, ExecutedTrade};
use crate::crypto::{SessionManifest, SignedTransaction, verify_transaction};
use crate::crypto::verify_commit_certificate;
use crate::consensus::CommitCertificate;

pub type Digest = [u8; 32];
/// Version 0 means the block predates explicit versioning. Historical files
/// may contain either the old full-state digest or the first Merkle root.
pub const LEGACY_STATE_ROOT_VERSION: u8 = 0;
pub const MERKLE_STATE_ROOT_VERSION: u8 = 1;
pub const ADDRESS_STATE_ROOT_VERSION: u8 = 2;

#[derive(Debug)]
struct PreparedBlock {
    exchange: ExchangeState,
    outcomes: Vec<Result<Vec<ExecutedTrade>, ExchangeError>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ts_rs::TS)]
pub struct Trade {
    pub market_id: MarketId,
    pub block_sequence: u64,
    pub transaction_index: usize,
    pub fill_index: usize,
    pub price: u64,
    pub quantity: u64,
    pub aggressor_side: Side,
    pub maker_order_id: OrderId,
    pub maker_account: AccountId,
    pub taker_order_id: OrderId,
    pub taker_account: AccountId,
}

impl Trade {
    fn from_fill(
        block_sequence: u64,
        transaction_index: usize,
        fill_index: usize,
        fill: ExecutedTrade,
    ) -> Self {
        Self {
            market_id: fill.market_id,
            block_sequence,
            transaction_index,
            fill_index,
            price: fill.price,
            quantity: fill.quantity,
            aggressor_side: fill.aggressor_side,
            maker_order_id: fill.maker_order_id,
            maker_account: fill.maker_account,
            taker_order_id: fill.taker_order_id,
            taker_account: fill.taker_account,
        }
    }
}

#[derive(Debug)]
pub struct BlockchainState {
    pub blocks: Vec<Block>,
    pub certificates: Vec<Option<CommitCertificate>>,
    pub mempool: Vec<Transaction>,
    signed_mempool: HashMap<Digest, String>,
    manifest: Option<Arc<SessionManifest>>,
    pub seen_transactions: HashSet<Digest>,
    committed_transactions: HashSet<Digest>,

    pub exchange_state: ExchangeState,
    pub recent_trades: VecDeque<Trade>,
    storage_path: Option<PathBuf>,
    // A proposal validated by the node can be committed without executing
    // its transactions a second time, provided the parent state is unchanged.
    prepared_block: Option<(Digest, Digest, PreparedBlock)>,
}

impl BlockchainState {
    pub fn journal_path(path: &Path) -> PathBuf {
        path.with_extension("json.journal")
    }

    pub fn new() -> Self {
        BlockchainState {
            blocks: Vec::new(),
            certificates: Vec::new(),
            mempool: Vec::new(),
            signed_mempool: HashMap::new(),
            manifest: None,
            seen_transactions: HashSet::new(),
            committed_transactions: HashSet::new(),
            exchange_state: ExchangeState::default(),
            recent_trades: VecDeque::new(),
            storage_path: None,
            prepared_block: None,
        }
    }

    pub fn load(path: PathBuf) -> Result<Self, String> {
        Self::load_with_manifest(path, None)
    }

    /// Inspect a historical unsigned chain without opening it for new commits.
    pub fn replay_legacy(path: PathBuf) -> Result<Self, String> {
        let mut state = Self::load_with_manifest(path, None)?;
        state.storage_path = None;
        Ok(state)
    }

    pub fn new_signed(manifest: Arc<SessionManifest>) -> Self {
        let mut state = Self::new();
        state.exchange_state = ExchangeState::with_funded_addresses(manifest.accounts.iter().copied());
        state.manifest = Some(manifest);
        state
    }

    pub fn signed_chain_id(&self) -> Option<&str> {
        self.manifest.as_ref().map(|manifest| manifest.chain_id.as_str())
    }

    pub fn load_signed(path: PathBuf, manifest: Arc<SessionManifest>) -> Result<Self, String> {
        if manifest.initial_state_root != crate::message::hex_encode(&ExchangeState::with_funded_addresses(manifest.accounts.iter().copied()).state_digest()) {
            return Err("session manifest initial state root differs from local genesis".into());
        }
        Self::load_with_manifest(path, Some(manifest))
    }

    fn load_with_manifest(path: PathBuf, manifest: Option<Arc<SessionManifest>>) -> Result<Self, String> {
        let mut state = match manifest { Some(manifest) => Self::new_signed(manifest), None => Self::new() };
        // Old snapshots remain readable. New commits are appended to the
        // adjacent journal, so recovery can also resume a legacy chain.
        if path.exists() {
            let mut file = File::open(&path).map_err(|error| error.to_string())?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|error| error.to_string())?;
            let blocks: Vec<Block> =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            if state.manifest.is_some() && !blocks.is_empty() { return Err("signed sessions require certified journal records".into()); }
            for block in blocks {
                state.commit_block(&block)?;
            }
        }
        let journal = Self::journal_path(&path);
        if journal.exists() {
            let mut bytes = fs::read(&journal).map_err(|error| error.to_string())?;
            let complete = bytes
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |index| index + 1);
            if complete < bytes.len() {
                // An interrupted append cannot be a committed block.
                let file = OpenOptions::new()
                    .write(true)
                    .open(&journal)
                    .map_err(|error| error.to_string())?;
                file.set_len(complete as u64)
                    .map_err(|error| error.to_string())?;
                file.sync_all().map_err(|error| error.to_string())?;
                bytes.truncate(complete);
            }
            for line in bytes
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
            {
                if state.manifest.is_some() {
                    let committed: CommittedBlock = serde_json::from_slice(line).map_err(|error| error.to_string())?;
                    state.commit_block_with_certificate(&committed.block, committed.certificate)?;
                } else {
                    let block: Block = serde_json::from_slice(line).map_err(|error| error.to_string())?;
                    state.commit_block(&block)?;
                }
            }
        }
        state.storage_path = Some(path);
        Ok(state)
    }

    fn persist(&self, block: &Block, certificate: Option<&CommitCertificate>) -> Result<(), String> {
        let Some(path) = &self.storage_path else {
            return Ok(());
        };
        let parent = path.parent().ok_or("invalid storage path")?;
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let journal = Self::journal_path(path);
        let newly_created = !journal.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&journal)
            .map_err(|error| error.to_string())?;
        let previous_len = file.metadata().map_err(|error| error.to_string())?.len();
        let mut bytes = if let Some(certificate) = certificate {
            serde_json::to_vec(&CommittedBlock { block: block.clone(), certificate: certificate.clone() })
        } else { serde_json::to_vec(block) }.map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        let result = file
            .write_all(&bytes)
            .and_then(|_| file.sync_all())
            .and_then(|_| {
                if newly_created {
                    File::open(parent)?.sync_all()
                } else {
                    Ok(())
                }
            });
        if let Err(error) = result {
            let _ = file.set_len(previous_len);
            let _ = file.sync_all();
            return Err(error.to_string());
        }
        Ok(())
    }

    pub fn validate_block(&mut self, block: &Block) -> Result<(), String> {
        self.prepared_block = None;
        let prepared = self.prepare_block(block)?;
        self.prepared_block = Some((block.to_digest(), self.exchange_state.state_digest(), prepared));
        Ok(())
    }

    fn prepare_block(&self, block: &Block) -> Result<PreparedBlock, String> {
        if let Some(manifest) = &self.manifest {
            let auth = block.authentication.as_ref().ok_or("unsigned block in signed session")?;
            if block.state_root_version != ADDRESS_STATE_ROOT_VERSION
                || auth.version != 2 || auth.chain_id != manifest.chain_id
                || auth.signatures.len() != block.transactions.len() {
                return Err("invalid block authentication metadata".into());
            }
            for (transaction, signature) in block.transactions.iter().zip(&auth.signatures) {
                verify_transaction(&SignedTransaction { transaction: transaction.clone(), signature: signature.clone() }, manifest)
                    .map_err(|error| error.to_string())?;
            }
        } else if block.authentication.is_some() {
            return Err("signed block requires session manifest".into());
        }
        let expected_sequence = self.blocks.len() as u64;
        if block.sequence != expected_sequence {
            return Err(format!(
                "expected sequence {expected_sequence}, got {}",
                block.sequence
            ));
        }
        let expected_previous = self.blocks.last().map(Block::to_digest).unwrap_or_default();
        if block.previous_hash != expected_previous {
            return Err("previous hash mismatch".into());
        }
        if block.sequence == 0 && !block.transactions.is_empty() {
            return Err("genesis must be empty".into());
        }
        if !matches!(block.state_root_version, LEGACY_STATE_ROOT_VERSION | MERKLE_STATE_ROOT_VERSION | ADDRESS_STATE_ROOT_VERSION) {
            return Err("unsupported state root version".into());
        }
        if self.blocks.last().is_some_and(|previous| {
            previous.state_root_version == MERKLE_STATE_ROOT_VERSION
                && block.state_root_version == LEGACY_STATE_ROOT_VERSION
        }) {
            return Err("state root version downgrade".into());
        }
        let mut digests = HashSet::new();
        if block
            .transactions
            .iter()
            .any(|tx| !digests.insert(tx.to_digest()))
        {
            return Err("duplicate transaction in block".into());
        }
        if digests
            .iter()
            .any(|digest| self.committed_transactions.contains(digest))
        {
            return Err("already committed transaction".into());
        }
        let mut exchange = self.exchange_state.clone();
        let outcomes = block.transactions.iter()
            .map(|tx| exchange.execute_with_receipt(tx))
            .collect();
        let root_matches = match block.state_root_version {
            LEGACY_STATE_ROOT_VERSION => {
                block.state_root == exchange.full_state_digest_legacy()
                    || block.state_root == exchange.state_digest()
            }
            MERKLE_STATE_ROOT_VERSION | ADDRESS_STATE_ROOT_VERSION => block.state_root == exchange.state_digest(),
            _ => unreachable!("checked root version"),
        };
        if !root_matches {
            return Err("state root mismatch".into());
        }
        Ok(PreparedBlock { exchange, outcomes })
    }

    pub fn commit_block(
        &mut self,
        block: &Block,
    ) -> Result<Vec<Result<(), ExchangeError>>, String> {
        self.commit_block_with_receipts(block).map(|outcomes| {
            outcomes
                .into_iter()
                .map(|result| result.map(|_| ()))
                .collect()
        })
    }

    pub fn commit_block_with_receipts(
        &mut self,
        block: &Block,
    ) -> Result<Vec<Result<Vec<Trade>, ExchangeError>>, String> {
        self.commit_block_inner(block, None)
    }

    pub fn commit_block_with_certificate(&mut self, block: &Block, certificate: CommitCertificate)
        -> Result<Vec<Result<Vec<Trade>, ExchangeError>>, String> {
        self.commit_block_inner(block, Some(certificate))
    }

    fn commit_block_inner(&mut self, block: &Block, certificate: Option<CommitCertificate>)
        -> Result<Vec<Result<Vec<Trade>, ExchangeError>>, String> {
        match (&self.manifest, &certificate) {
            (Some(manifest), Some(proof)) => verify_commit_certificate(block, proof, manifest).map_err(|error| error.to_string())?,
            (Some(_), None) => return Err("signed session requires a commit certificate".into()),
            (None, Some(_)) => return Err("legacy state cannot accept a signed commit certificate".into()),
            (None, None) => {}
        }
        let parent_root = self.exchange_state.state_digest();
        let block_digest = block.to_digest();
        let prepared = match self.prepared_block.take() {
            Some((digest, root, prepared))
                if digest == block_digest
                    && root == parent_root
                    && block.sequence == self.blocks.len() as u64
                    && block.previous_hash == self.blocks.last().map(Block::to_digest).unwrap_or_default() => prepared,
            _ => self.prepare_block(block)?,
        };
        self.persist(block, certificate.as_ref())?;
        self.exchange_state = prepared.exchange;
        let mut outcomes = Vec::with_capacity(block.transactions.len());
        for (i, (tx, result)) in block.transactions.iter().zip(prepared.outcomes).enumerate() {
            if result.is_err() {
                eprintln!("Error executing tx {i} : {tx:?}");
                // A failed transaction did not consume its nonce. It may be retried
                // after the account state changes, including after a restart.
                self.seen_transactions.remove(&tx.to_digest());
            } else {
                self.seen_transactions.insert(tx.to_digest());
                self.committed_transactions.insert(tx.to_digest());
            }
            let result = result.map(|fills| {
                fills
                    .into_iter()
                    .enumerate()
                    .map(|(fill_index, fill)| Trade::from_fill(block.sequence, i, fill_index, fill))
                    .collect::<Vec<_>>()
            });
            if let Ok(trades) = &result {
                self.recent_trades.extend(trades.iter().cloned());
                while self.recent_trades.len() > 200 {
                    self.recent_trades.pop_front();
                }
            }
            outcomes.push(result);
        }

        self.blocks.push(block.clone());
        self.certificates.push(certificate);
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
        self.mempool.retain(|tx| {
            self.exchange_state
                .get_nonce_for_account(tx.account_id())
                .is_ok_and(|nonce| tx.nonce() >= nonce)
        });
        let live: HashSet<_> = self.mempool.iter().map(Transaction::to_digest).collect();
        self.signed_mempool.retain(|digest, _| live.contains(digest));
        let pending_digests: HashSet<_> = self.mempool.iter().map(Transaction::to_digest).collect();
        self.seen_transactions.retain(|digest| {
            self.committed_transactions.contains(digest) || pending_digests.contains(digest)
        });
    }
    pub fn add_transaction(&mut self, tx: &Transaction) -> bool {
        if self.manifest.is_some() { return false; }
        self.add_transaction_verified(tx)
    }

    pub fn add_signed_transaction(&mut self, signed: &SignedTransaction) -> bool {
        let Some(manifest) = &self.manifest else { return false; };
        if verify_transaction(signed, manifest).is_err() { return false; }
        let digest = signed.transaction.to_digest();
        if !self.add_transaction_verified(&signed.transaction) { return false; }
        self.signed_mempool.insert(digest, signed.signature.clone());
        true
    }

    pub fn signed_transaction(&self, tx: &Transaction) -> Option<SignedTransaction> {
        self.signed_mempool.get(&tx.to_digest()).map(|signature| SignedTransaction {
            transaction: tx.clone(), signature: signature.clone(),
        })
    }

    fn add_transaction_verified(&mut self, tx: &Transaction) -> bool {
        if !self
            .exchange_state
            .get_nonce_for_account(tx.account_id())
            .is_ok_and(|nonce| tx.nonce() >= nonce)
        {
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
        for tx in &transactions {
            let _ = exchange.execute(tx);
        }
        let authentication = self.manifest.as_ref().map(|manifest| BlockAuthentication {
            version: 2,
            chain_id: manifest.chain_id.clone(),
            signatures: transactions.iter().map(|tx| self.signed_mempool.get(&tx.to_digest()).cloned().expect("verified mempool signature missing")).collect(),
        });
        let block = Block {
            transactions: transactions,
            sequence: previous_block.sequence + 1,
            previous_hash: previous_block.to_digest(),
            state_root: exchange.state_digest(),
            state_root_version: if self.manifest.is_some() { ADDRESS_STATE_ROOT_VERSION } else { MERKLE_STATE_ROOT_VERSION },
            authentication,
        };

        Some(block)
    }

    pub fn build_executable_transactions(&self) -> Vec<Transaction> {
        let mut per_account: HashMap<AccountId, Vec<PendingTransaction>> = HashMap::new();
        for (i, tx) in self.mempool.iter().enumerate() {
            per_account
                .entry(tx.account_id())
                .or_default()
                .push(PendingTransaction {
                    arrival_index: i,
                    transaction: tx.clone(),
                });
        }
        let mut queues: HashMap<AccountId, VecDeque<PendingTransaction>> = HashMap::new();
        for (account_id, mut txs) in per_account {
            txs.sort_by_key(|tx| tx.transaction.nonce());
            let Ok(mut expected_nonce) = self.exchange_state.get_nonce_for_account(account_id)
            else {
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
                expected_nonce += 1;
            }
            if !queue.is_empty() {
                queues.insert(account_id, queue);
            }
        }

        let mut heap: BinaryHeap<(Reverse<usize>, AccountId)> = BinaryHeap::new();
        for (account_id, queue) in &queues {
            if let Some(front) = queue.front() {
                heap.push((Reverse(front.arrival_index), *account_id));
            }
        }

        let mut selected: Vec<Transaction> = Vec::new();

        while let Some((Reverse(_arrival_index), account_id)) = heap.pop() {
            let queue = queues
                .get_mut(&account_id)
                .expect("Heap referenced missing account queue");
            let pending = queue
                .pop_front()
                .expect("heap referenced empty account queue");
            selected.push(pending.transaction);
            if let Some(next) = queue.front() {
                heap.push((Reverse(next.arrival_index), account_id));
            }
        }
        selected
    }
}

#[derive(Debug, Clone)]
struct PendingTransaction {
    arrival_index: usize,
    transaction: Transaction,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Block {
    pub sequence: u64,
    pub previous_hash: Digest,
    pub transactions: Vec<Transaction>,
    pub state_root: Digest,
    /// Missing in pre-version journals. Version 0 retains their historical
    /// block hash and accepts either historical state-root format.
    #[serde(default)]
    pub state_root_version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication: Option<BlockAuthentication>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CommittedBlock {
    pub block: Block,
    pub certificate: CommitCertificate,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BlockAuthentication {
    pub version: u8,
    pub chain_id: String,
    pub signatures: Vec<String>,
}

impl Default for Block {
    fn default() -> Self { Self::new() }
}

impl Block {
    pub fn new() -> Block {
        Block {
            transactions: Vec::new(),
            state_root: ExchangeState::default().state_digest(),
            state_root_version: MERKLE_STATE_ROOT_VERSION,
            authentication: None,
            sequence: 0,
            previous_hash: Digest::default(),
        }
    }
    pub fn new_initial_block() -> Block {
        Block::new()
    }

    pub fn new_initial_block_signed(chain_id: &str, initial_root: Digest) -> Block {
        let mut block = Self::new();
        block.state_root = initial_root;
        block.state_root_version = ADDRESS_STATE_ROOT_VERSION;
        block.authentication = Some(BlockAuthentication { version: 2, chain_id: chain_id.into(), signatures: Vec::new() });
        block
    }

    pub fn to_digest(&self) -> Digest {
        let mut hasher = Sha256::new();
        if let Some(auth) = &self.authentication {
            hasher.update(b"light-dex/block/v3");
            hasher.update([auth.version, self.state_root_version]);
            hasher.update(auth.chain_id.as_bytes());
            hasher.update(self.sequence.to_be_bytes());
            hasher.update(self.previous_hash);
            hasher.update(self.state_root);
            hasher.update((self.transactions.len() as u64).to_be_bytes());
            for (tx, signature) in self.transactions.iter().zip(&auth.signatures) {
                hasher.update(tx.to_digest());
                hasher.update(signature.as_bytes());
            }
            return hasher.finalize().into();
        }
        if self.state_root_version != LEGACY_STATE_ROOT_VERSION {
            hasher.update(b"light-dex/block/v1");
            hasher.update([self.state_root_version]);
        }
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

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, ts_rs::TS)]
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
    pub fn account_id(&self) -> AccountId {
        let account_id = match self {
            Transaction::Transfer {
                from,
                to: _,
                amount: _,
                nonce: _,
                asset: _,
            } => from,
            Transaction::PlaceOrder {
                account_id,
                market_id: _,
                order_type: _,
                side: _,
                price: _,
                quantity: _,
                nonce: _,
            } => account_id,
            Transaction::CancelOrder {
                account_id,
                market_id: _,
                order_id: _,
                nonce: _,
            } => account_id,
        };

        *account_id
    }
    pub fn nonce(&self) -> u64 {
        let nonce = match self {
            Transaction::Transfer {
                from: _,
                to: _,
                amount: _,
                nonce,
                asset: _,
            } => nonce,
            Transaction::PlaceOrder {
                account_id: _,
                market_id: _,
                order_type: _,
                side: _,
                price: _,
                quantity: _,
                nonce,
            } => nonce,
            Transaction::CancelOrder {
                account_id: _,
                market_id: _,
                order_id: _,
                nonce,
            } => nonce,
        };

        *nonce
    }
    pub fn to_digest(&self) -> Digest {
        let mut hasher = Sha256::new();
        hasher.update(b"light-dex/transaction/v2");
        match self {
            Transaction::Transfer {
                from,
                to,
                asset,
                amount,
                nonce,
            } => {
                hasher.update(b"TRANSFER");
                hasher.update(from.bytes());
                hasher.update(to.bytes());
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

                hasher.update(account.bytes());
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

                hasher.update(account.bytes());
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
    use super::{Block, BlockchainState, Transaction, LEGACY_STATE_ROOT_VERSION, MERKLE_STATE_ROOT_VERSION};
    use crate::exchange::order::{OrderType, Side};

    #[test]
    fn committed_fills_have_stable_coordinates_and_replay() {
        let directory =
            std::env::temp_dir().join(format!("light-dex-trades-{}", rand::random::<u64>()));
        let path = directory.join("node-0.json");
        let mut state = BlockchainState::load(path.clone()).unwrap();
        state.commit_block(&Block::new_initial_block()).unwrap();
        for (account_id, price) in [(1, 100), (2, 101)] {
            assert!(state.add_transaction(&Transaction::PlaceOrder {
                account_id: account_id.into(),
                market_id: 0,
                order_type: OrderType::LO,
                side: Side::Sell,
                price,
                quantity: 1,
                nonce: 0,
            }));
        }
        let block = state.build_block().unwrap();
        state.commit_block(&block).unwrap();
        state.remove_included_txs_from_mempool(&block);
        assert!(state.add_transaction(&Transaction::PlaceOrder {
            account_id: 3.into(),
            market_id: 0,
            order_type: OrderType::IOC,
            side: Side::Buy,
            price: 101,
            quantity: 2,
            nonce: 0,
        }));
        let block = state.build_block().unwrap();
        let receipts = state.commit_block_with_receipts(&block).unwrap();
        let trades = receipts[0].as_ref().unwrap();
        assert_eq!(trades.len(), 2);
        assert_eq!(
            trades.iter().map(|trade| trade.price).collect::<Vec<_>>(),
            vec![100, 101]
        );
        assert_eq!(
            trades
                .iter()
                .map(|trade| trade.fill_index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert!(trades
            .iter()
            .all(|trade| trade.block_sequence == block.sequence && trade.taker_account == 3.into()));
        let replay = BlockchainState::load(path.clone()).unwrap();
        assert_eq!(
            replay.recent_trades.iter().cloned().collect::<Vec<_>>(),
            trades.clone()
        );
        std::fs::remove_file(BlockchainState::journal_path(&path)).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn executable_queue_keeps_later_valid_nonce_for_same_account() {
        let mut state = BlockchainState::new();
        let mut account = state.exchange_state.accounts()[&0.into()].clone();
        account.nonce = 1;
        state.exchange_state.upsert_genesis_account(account);
        for (nonce, amount) in [(0, 1), (1, 2), (2, 3)] {
            state.add_transaction(&Transaction::Transfer {
                from: 0.into(),
                to: 1.into(),
                asset: 0,
                amount,
                nonce,
            });
        }
        let selected = state.build_executable_transactions();
        assert_eq!(
            selected.iter().map(Transaction::nonce).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn rejects_wrong_state_root_and_recovers_committed_blocks() {
        let directory =
            std::env::temp_dir().join(format!("light-dex-chain-{}", rand::random::<u64>()));
        let path = directory.join("node-0.json");
        let mut state = BlockchainState::load(path.clone()).unwrap();
        state.commit_block(&Block::new_initial_block()).unwrap();
        let tx = Transaction::Transfer {
            from: 0.into(),
            to: 1.into(),
            asset: 0,
            amount: 5,
            nonce: 0,
        };
        state.add_transaction(&tx);
        let block = state.build_block().unwrap();
        let mut corrupt = block.clone();
        corrupt.state_root[0] ^= 1;
        assert!(state.commit_block(&corrupt).is_err());
        state.commit_block(&block).unwrap();
        let recovered = BlockchainState::load(path.clone()).unwrap();
        assert_eq!(recovered.blocks.len(), 2);
        assert_eq!(
            recovered.exchange_state.state_digest(),
            state.exchange_state.state_digest()
        );
        assert_eq!(recovered.exchange_state.accounts()[&0.into()].nonce, 1);
        std::fs::remove_file(BlockchainState::journal_path(&path)).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn failed_transaction_keeps_nonce_and_can_be_retried_after_recovery() {
        let directory =
            std::env::temp_dir().join(format!("light-dex-retry-{}", rand::random::<u64>()));
        let path = directory.join("node-0.json");
        let mut state = BlockchainState::load(path.clone()).unwrap();
        state.commit_block(&Block::new_initial_block()).unwrap();
        let tx = Transaction::Transfer {
            from: 0.into(),
            to: 1.into(),
            asset: 0,
            amount: 2_000_000,
            nonce: 0,
        };
        assert!(state.add_transaction(&tx));
        let block = state.build_block().unwrap();
        assert!(state.commit_block(&block).unwrap()[0].is_err());
        assert_eq!(state.exchange_state.accounts()[&0.into()].nonce, 0);
        let mut recovered = BlockchainState::load(path.clone()).unwrap();
        assert_eq!(recovered.exchange_state.accounts()[&0.into()].nonce, 0);
        assert!(recovered.add_transaction(&tx));
        std::fs::remove_file(BlockchainState::journal_path(&path)).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn legacy_snapshot_and_interrupted_journal_append_recover() {
        let directory =
            std::env::temp_dir().join(format!("light-dex-legacy-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("node-0.json");
        let genesis = Block::new_initial_block();
        std::fs::write(&path, serde_json::to_vec(&vec![genesis]).unwrap()).unwrap();
        let mut state = BlockchainState::load(path.clone()).unwrap();
        let tx = Transaction::Transfer {
            from: 0.into(),
            to: 1.into(),
            asset: 0,
            amount: 1,
            nonce: 0,
        };
        assert!(state.add_transaction(&tx));
        let block = state.build_block().unwrap();
        state.commit_block(&block).unwrap();
        let journal = BlockchainState::journal_path(&path);
        assert_eq!(
            std::fs::read_to_string(&journal).unwrap().lines().count(),
            1
        );
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&journal)
            .unwrap()
            .write_all(b"{\"sequence\":")
            .unwrap();
        let recovered = BlockchainState::load(path.clone()).unwrap();
        assert_eq!(recovered.blocks.len(), 2);
        assert_eq!(
            recovered.exchange_state.state_digest(),
            state.exchange_state.state_digest()
        );
        assert!(std::fs::read_to_string(&journal).unwrap().ends_with('\n'));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn committed_nonce_prunes_conflicting_mempool_entries() {
        let mut state = BlockchainState::new();
        state.commit_block(&Block::new_initial_block()).unwrap();
        let chosen = Transaction::Transfer {
            from: 0.into(),
            to: 1.into(),
            asset: 0,
            amount: 1,
            nonce: 0,
        };
        let conflict = Transaction::Transfer {
            from: 0.into(),
            to: 2.into(),
            asset: 0,
            amount: 1,
            nonce: 0,
        };
        let next = Transaction::Transfer {
            from: 0.into(),
            to: 1.into(),
            asset: 0,
            amount: 1,
            nonce: 1,
        };
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

    #[test]
    fn old_blocks_replay_and_transition_to_versioned_merkle_roots() {
        let directory = std::env::temp_dir()
            .join(format!("light-dex-root-upgrade-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("node-0.json");

        let mut old_genesis = Block::new_initial_block();
        old_genesis.state_root_version = LEGACY_STATE_ROOT_VERSION;
        old_genesis.state_root = BlockchainState::new().exchange_state.full_state_digest_legacy();
        let old_hash = old_genesis.to_digest();
        let mut old_json = serde_json::to_value(&old_genesis).unwrap();
        old_json.as_object_mut().unwrap().remove("state_root_version");
        std::fs::write(&path, serde_json::to_vec(&vec![old_json]).unwrap()).unwrap();

        let mut state = BlockchainState::load(path.clone()).unwrap();
        assert_eq!(state.blocks[0].to_digest(), old_hash);
        let tx = Transaction::Transfer { from: 0.into(), to: 1.into(), asset: 0, amount: 5, nonce: 0 };
        let mut legacy_exchange = state.exchange_state.clone();
        legacy_exchange.execute(&tx).unwrap();
        let old_block = Block {
            sequence: 1,
            previous_hash: old_hash,
            transactions: vec![tx],
            state_root: legacy_exchange.full_state_digest_legacy(),
            state_root_version: LEGACY_STATE_ROOT_VERSION,
            authentication: None,
        };
        state.commit_block(&old_block).unwrap();
        let mut recovered = BlockchainState::load(path.clone()).unwrap();
        assert_eq!(recovered.blocks.len(), 2);
        assert_eq!(recovered.blocks[1].to_digest(), old_block.to_digest());

        let next = Transaction::Transfer { from: 0.into(), to: 1.into(), asset: 0, amount: 1, nonce: 1 };
        assert!(recovered.add_transaction(&next));
        let upgraded = recovered.build_block().unwrap();
        assert_eq!(upgraded.state_root_version, MERKLE_STATE_ROOT_VERSION);
        assert_eq!(upgraded.previous_hash, old_block.to_digest());
        recovered.commit_block(&upgraded).unwrap();
        let mut replayed = BlockchainState::load(path.clone()).unwrap();
        assert_eq!(replayed.blocks.len(), 3);
        assert_eq!(replayed.exchange_state.state_digest(), recovered.exchange_state.state_digest());

        let mut downgrade = upgraded.clone();
        downgrade.sequence += 1;
        downgrade.previous_hash = upgraded.to_digest();
        downgrade.state_root_version = LEGACY_STATE_ROOT_VERSION;
        assert!(replayed.validate_block(&downgrade).unwrap_err().contains("downgrade"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unversioned_merkle_blocks_from_transition_also_replay() {
        let directory = std::env::temp_dir()
            .join(format!("light-dex-unversioned-merkle-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("node-0.json");
        let mut state = BlockchainState::new();
        let mut genesis = Block::new_initial_block();
        genesis.state_root_version = LEGACY_STATE_ROOT_VERSION;
        state.commit_block(&genesis).unwrap();
        let tx = Transaction::Transfer { from: 0.into(), to: 1.into(), asset: 0, amount: 5, nonce: 0 };
        let mut exchange = state.exchange_state.clone();
        exchange.execute(&tx).unwrap();
        let next = Block {
            sequence: 1,
            previous_hash: genesis.to_digest(),
            transactions: vec![tx],
            state_root: exchange.state_digest(),
            state_root_version: LEGACY_STATE_ROOT_VERSION,
            authentication: None,
        };
        state.commit_block(&next).unwrap();
        let old_json = state.blocks.iter().map(|block| {
            let mut value = serde_json::to_value(block).unwrap();
            value.as_object_mut().unwrap().remove("state_root_version");
            value
        }).collect::<Vec<_>>();
        std::fs::write(&path, serde_json::to_vec(&old_json).unwrap()).unwrap();
        let mut recovered = BlockchainState::load(path).unwrap();
        assert_eq!(recovered.blocks[1].to_digest(), next.to_digest());
        assert_eq!(recovered.exchange_state.state_digest(), state.exchange_state.state_digest());
        assert!(recovered.add_transaction(&Transaction::Transfer {
            from: 0.into(), to: 1.into(), asset: 0, amount: 1, nonce: 1,
        }));
        assert_eq!(recovered.build_block().unwrap().state_root_version, MERKLE_STATE_ROOT_VERSION);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_persistence_does_not_publish_prepared_state() {
        let directory = std::env::temp_dir()
            .join(format!("light-dex-atomic-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&directory).unwrap();
        let non_directory = directory.join("file");
        std::fs::write(&non_directory, b"not a directory").unwrap();
        let mut state = BlockchainState::load(non_directory.join("node.json")).unwrap();
        let before = state.exchange_state.state_digest();
        state.validate_block(&Block::new_initial_block()).unwrap();
        assert!(state.prepared_block.is_some());
        assert!(state.commit_block(&Block::new_initial_block()).is_err());
        assert!(state.blocks.is_empty());
        assert_eq!(state.exchange_state.state_digest(), before);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn commit_consumes_matching_prepared_result() {
        let mut state = BlockchainState::new();
        let genesis = Block::new_initial_block();
        state.validate_block(&genesis).unwrap();
        assert!(state.prepared_block.is_some());
        state.commit_block(&genesis).unwrap();
        assert!(state.prepared_block.is_none());

        let tx = Transaction::Transfer { from: 0.into(), to: 1.into(), asset: 0, amount: 1, nonce: 0 };
        assert!(state.add_transaction(&tx));
        let block = state.build_block().unwrap();
        state.validate_block(&block).unwrap();
        let prepared_root = state.prepared_block.as_ref().unwrap().2.exchange.state_digest();
        let outcomes = state.commit_block(&block).unwrap();
        assert!(outcomes[0].is_ok());
        assert!(state.prepared_block.is_none());
        assert_eq!(state.exchange_state.state_digest(), prepared_root);
        assert_eq!(state.exchange_state.accounts()[&0.into()].nonce, 1);
    }
}
