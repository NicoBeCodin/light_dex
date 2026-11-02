use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as ShaDigest;
use sha2::Sha256;

use crate::consensus::CommitCertificate;
use crate::crypto::verify_commit_certificate;
use crate::crypto::{PROTOCOL_VERSION, SessionManifest, SignedTransaction, verify_transaction};
use crate::exchange::account::AccountId;
use crate::exchange::account::AssetId;
use crate::exchange::market::MarketId;
use crate::exchange::order::OrderId;
use crate::exchange::order::OrderType;
use crate::exchange::order::Side;
use crate::exchange::{ExchangeError, ExchangeState, ExecutedTrade};
use crate::oracle::OracleUpdate;

mod block;
mod digest_cache;
mod journal;
mod mempool;
mod storage;
pub(crate) use journal::DurableBlock;
mod limits;
mod trade;
mod transaction;

pub use block::{Block, BlockAuthentication, CommittedBlock, Proposal};
pub use mempool::Mempool;
pub use trade::Trade;
pub use transaction::{QuoteOrder, QuoteReduction, Transaction};

pub type Digest = [u8; 32];
/// Version 0 means the block predates explicit versioning. Historical files
/// may contain either the old full-state digest or the first Merkle root.
pub const LEGACY_STATE_ROOT_VERSION: u8 = 0;
pub const MERKLE_STATE_ROOT_VERSION: u8 = 1;
pub const ADDRESS_STATE_ROOT_VERSION: u8 = 4;

#[derive(Debug)]
struct PreparedBlock {
    exchange: ExchangeState,
    outcomes: Vec<Result<Vec<ExecutedTrade>, ExchangeError>>,
}

#[derive(Debug)]
pub struct BlockchainState {
    pub blocks: Vec<Block>,
    pub certificates: Vec<Option<CommitCertificate>>,
    pub mempool: Mempool,
    signed_mempool: HashMap<Digest, String>,
    manifest: Option<Arc<SessionManifest>>,
    pub seen_transactions: HashSet<Digest>,
    committed_transactions: HashSet<Digest>,

    pub exchange_state: ExchangeState,
    pub recent_trades: VecDeque<Trade>,
    storage_path: Option<PathBuf>,
    journal: std::sync::OnceLock<journal::Journal>,
    // A proposal validated by the node can be committed without executing
    // its transactions a second time, provided the parent state is unchanged.
    prepared_block: Option<(Digest, Digest, PreparedBlock)>,
}

impl BlockchainState {
    pub fn validate_block(&mut self, block: &Block) -> Result<(), String> {
        // The signed hash zips transactions/signatures; reject a malformed
        // length even when a valid proposal with that digest was cached.
        if block
            .authentication
            .as_ref()
            .is_some_and(|auth| auth.signatures.len() != block.transactions.len())
        {
            return Err("invalid block authentication metadata".into());
        }
        if self
            .prepared_block
            .as_ref()
            .is_some_and(|(digest, root, _)| {
                *digest == block.to_digest()
                    && *root == self.exchange_state.state_digest()
                    && block.sequence == self.blocks.len() as u64
                    && block.previous_hash
                        == self.blocks.last().map(Block::to_digest).unwrap_or_default()
            })
        {
            return Ok(());
        }
        self.prepared_block = None;
        let prepared = self.prepare_block(block)?;
        self.prepared_block = Some((
            block.to_digest(),
            self.exchange_state.state_digest(),
            prepared,
        ));
        Ok(())
    }

    fn prepare_block(&self, block: &Block) -> Result<PreparedBlock, String> {
        self.prepare_block_on(block, self.blocks.last(), &self.exchange_state)
    }

    /// Validate a certified parent's child against the parent's speculative
    /// exchange state. This does not publish or persist the speculative state.
    pub fn validate_speculative_block(
        &self,
        block: &Block,
        parent: &Block,
        parent_exchange: &ExchangeState,
    ) -> Result<ExchangeState, String> {
        self.prepare_block_on(block, Some(parent), parent_exchange)
            .map(|prepared| prepared.exchange)
    }

    pub(crate) fn take_built_speculative(&mut self, digest: Digest) -> Option<ExchangeState> {
        match self.prepared_block.take() {
            Some((candidate, _, prepared)) if candidate == digest => Some(prepared.exchange),
            other => {
                self.prepared_block = other;
                None
            }
        }
    }

    fn prepare_block_on(
        &self,
        block: &Block,
        parent: Option<&Block>,
        parent_exchange: &ExchangeState,
    ) -> Result<PreparedBlock, String> {
        let total_started = std::time::Instant::now();
        let verify_started = std::time::Instant::now();
        limits::validate(block)?;
        if let Some(manifest) = &self.manifest {
            let auth = block
                .authentication
                .as_ref()
                .ok_or("unsigned block in signed session")?;
            if block.state_root_version != ADDRESS_STATE_ROOT_VERSION
                || auth.version != PROTOCOL_VERSION
                || auth.chain_id != manifest.chain_id
                || auth.signatures.len() != block.transactions.len()
            {
                return Err("invalid block authentication metadata".into());
            }
            crate::crypto::verification::verify_block_transactions(block, manifest)
                .map_err(|error| error.to_string())?;
        } else if block.authentication.is_some() {
            return Err("signed block requires session manifest".into());
        }
        crate::profile::record(
            "follower_auth",
            verify_started.elapsed(),
            block.transactions.len() as u64,
        );
        let expected_sequence = parent.map_or(0, |block| block.sequence + 1);
        if block.sequence != expected_sequence {
            return Err(format!(
                "expected sequence {expected_sequence}, got {}",
                block.sequence
            ));
        }
        let expected_previous = parent.map(Block::to_digest).unwrap_or_default();
        if block.previous_hash != expected_previous {
            return Err("previous hash mismatch".into());
        }
        if block.sequence == 0
            && (!block.transactions.is_empty() || !block.oracle_updates.is_empty())
        {
            return Err("genesis must be empty".into());
        }
        if !matches!(
            block.state_root_version,
            LEGACY_STATE_ROOT_VERSION | MERKLE_STATE_ROOT_VERSION | ADDRESS_STATE_ROOT_VERSION
        ) {
            return Err("unsupported state root version".into());
        }
        if parent.is_some_and(|previous| {
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
        let mut exchange = parent_exchange.fork();
        exchange.begin_state_batch();
        let oracle_started = std::time::Instant::now();
        if block.oracle_updates.len() > 32 {
            return Err("too many oracle updates in block".into());
        }
        let mut previous_pair = None;
        for update in &block.oracle_updates {
            if previous_pair.is_some_and(|pair| pair >= update.pair) {
                return Err("oracle updates must be ordered by distinct pair".into());
            }
            previous_pair = Some(update.pair);
            exchange
                .apply_oracle_update(update)
                .map_err(|error| format!("invalid oracle update: {error:?}"))?;
        }
        crate::profile::record(
            "follower_oracle",
            oracle_started.elapsed(),
            block.oracle_updates.len() as u64,
        );
        let mut outcomes = Vec::with_capacity(block.transactions.len());
        let mut work = 0usize;
        let prefetch_distance = crate::exchange::prefetch::distance();
        let execute_started = std::time::Instant::now();
        for (index, tx) in block.transactions.iter().enumerate() {
            if prefetch_distance > 0 {
                if let Some(next) = block.transactions.get(index + prefetch_distance) {
                    exchange.prefetch_transaction(next);
                }
            }
            outcomes.push(exchange.execute_with_receipt(tx));
            work = work.saturating_add(exchange.last_execution_work());
            if work > crate::exchange::limits::MAX_BLOCK_WORK {
                return Err("block execution work limit".into());
            }
        }
        crate::profile::record(
            "follower_execute",
            execute_started.elapsed(),
            block.transactions.len() as u64,
        );
        let materialize_started = std::time::Instant::now();
        exchange.finish_state_batch();
        let root_matches = match block.state_root_version {
            LEGACY_STATE_ROOT_VERSION => {
                block.state_root == exchange.full_state_digest_legacy()
                    || block.state_root == exchange.state_digest()
            }
            MERKLE_STATE_ROOT_VERSION | ADDRESS_STATE_ROOT_VERSION => {
                block.state_root == exchange.state_digest()
            }
            _ => unreachable!("checked root version"),
        };
        if !root_matches {
            return Err("state root mismatch".into());
        }
        crate::profile::record(
            "follower_state_root",
            materialize_started.elapsed(),
            block.transactions.len() as u64,
        );
        crate::profile::record(
            "follower_total",
            total_started.elapsed(),
            block.transactions.len() as u64,
        );
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
        self.commit_block_inner(block, None, false)
    }

    pub fn commit_block_with_certificate(
        &mut self,
        block: &Block,
        certificate: CommitCertificate,
    ) -> Result<Vec<Result<Vec<Trade>, ExchangeError>>, String> {
        self.commit_block_inner(block, Some(certificate), false)
    }

    fn commit_block_inner(
        &mut self,
        block: &Block,
        certificate: Option<CommitCertificate>,
        durable: bool,
    ) -> Result<Vec<Result<Vec<Trade>, ExchangeError>>, String> {
        if block
            .authentication
            .as_ref()
            .is_some_and(|auth| auth.signatures.len() != block.transactions.len())
        {
            return Err("invalid block authentication metadata".into());
        }
        match (&self.manifest, &certificate) {
            (Some(manifest), Some(proof)) => verify_commit_certificate(block, proof, manifest)
                .map_err(|error| error.to_string())?,
            (Some(_), None) => return Err("signed session requires a commit certificate".into()),
            (None, Some(_)) => {
                return Err("legacy state cannot accept a signed commit certificate".into());
            }
            (None, None) => {}
        }
        let parent_root = self.exchange_state.state_digest();
        let block_digest = block.to_digest();
        let prepared = match self.prepared_block.take() {
            Some((digest, root, prepared))
                if digest == block_digest
                    && root == parent_root
                    && block.sequence == self.blocks.len() as u64
                    && block.previous_hash
                        == self.blocks.last().map(Block::to_digest).unwrap_or_default() =>
            {
                prepared
            }
            _ => self.prepare_block(block)?,
        };
        if !durable {
            self.persist(block, certificate.as_ref())?;
        }
        self.exchange_state = prepared.exchange;
        let mut outcomes = Vec::with_capacity(block.transactions.len());
        for (i, (tx, result)) in block.transactions.iter().zip(prepared.outcomes).enumerate() {
            if result.is_err() {
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
                    .map(|(fill_index, fill)| {
                        Trade::from_fill(
                            block.sequence,
                            i,
                            fill_index,
                            fill,
                            matches!(tx, Transaction::LiquidatePerp { .. }),
                        )
                    })
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
}

#[cfg(test)]
mod tests;
