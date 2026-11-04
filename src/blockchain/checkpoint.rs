//! Certified exchange snapshots and bounded in-memory history for hosted runs.
use super::*;
use std::io::{BufRead, Read, Write};
const MAX_SNAPSHOT: u64 = 64 * 1024 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    version: u8,
    chain_id: String,
    base: usize,
    blocks: Vec<Block>,
    certificates: Vec<Option<CommitCertificate>>,
    exchange: ExchangeState,
}
impl Checkpoint {
    pub fn read(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        zstd::stream::read::Decoder::new(file)
            .map_err(|e| e.to_string())?
            .take(MAX_SNAPSHOT + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_SNAPSHOT {
            return Err("checkpoint exceeds size limit".into());
        }
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())
    }
    fn write(&self, path: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_SNAPSHOT {
            return Err("checkpoint exceeds size limit".into());
        }
        let tmp = path.with_extension("checkpoint.pending");
        let mut encoder =
            zstd::stream::write::Encoder::new(File::create(&tmp).map_err(|e| e.to_string())?, 3)
                .map_err(|e| e.to_string())?;
        encoder.write_all(&bytes).map_err(|e| e.to_string())?;
        encoder
            .finish()
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        let previous = path.with_extension("checkpoint.previous");
        if path.exists() {
            fs::rename(path, &previous).map_err(|e| e.to_string())?;
        }
        fs::rename(tmp, path).map_err(|e| e.to_string())?;
        File::open(path.parent().unwrap())
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())
    }
}
impl BlockchainState {
    pub fn height(&self) -> usize {
        self.base_height + self.blocks.len()
    }
    pub fn block_at(&self, sequence: usize) -> Option<&Block> {
        sequence
            .checked_sub(self.base_height)
            .and_then(|i| self.blocks.get(i))
    }
    pub fn certificate_at(&self, sequence: usize) -> Option<&Option<CommitCertificate>> {
        sequence
            .checked_sub(self.base_height)
            .and_then(|i| self.certificates.get(i))
    }
    pub fn checkpoint(&self) -> Option<Checkpoint> {
        let chain_id = self.signed_chain_id()?.to_owned();
        if self.blocks.is_empty() {
            return None;
        }
        let retained = std::env::var("LIGHT_DEX_RETAINED_BLOCKS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1024)
            .clamp(2, 1024);
        let keep = self.blocks.len().min(retained);
        let start = self.blocks.len() - keep;
        Some(Checkpoint {
            version: 1,
            chain_id,
            base: self.base_height + start,
            blocks: self.blocks[start..].to_vec(),
            certificates: self.certificates[start..].to_vec(),
            exchange: self.exchange_state.fork(),
        })
    }
    pub fn install_checkpoint(&mut self, mut cp: Checkpoint) -> Result<(), String> {
        let manifest = self
            .manifest
            .as_ref()
            .ok_or("checkpoint requires signed chain")?;
        if cp.version != 1
            || cp.chain_id != manifest.chain_id
            || cp.blocks.is_empty()
            || cp.blocks.len() != cp.certificates.len()
            || cp.blocks.len() > 1024
        {
            return Err("invalid checkpoint metadata".into());
        }
        for (index, (block, certificate)) in cp.blocks.iter().zip(&cp.certificates).enumerate() {
            if block.sequence != (cp.base + index) as u64 {
                return Err("checkpoint sequence mismatch".into());
            }
            if index > 0 && block.previous_hash != cp.blocks[index - 1].to_digest() {
                return Err("checkpoint chain mismatch".into());
            }
            verify_commit_certificate(
                block,
                certificate
                    .as_ref()
                    .ok_or("checkpoint certificate missing")?,
                manifest,
            )
            .map_err(|e| e.to_string())?;
            if let Some(local) = self.block_at(block.sequence as usize) {
                if local.to_digest() != block.to_digest() {
                    return Err("checkpoint conflicts with local history".into());
                }
            }
        }
        cp.exchange.restore_snapshot_indexes()?;
        if cp.exchange.state_digest() != cp.blocks.last().unwrap().state_root {
            return Err("checkpoint state root mismatch".into());
        }
        if cp.base + cp.blocks.len() < self.height() {
            return Err("checkpoint is older than local state".into());
        }
        self.exchange_state = cp.exchange;
        self.base_height = cp.base;
        self.blocks = cp.blocks;
        self.certificates = cp.certificates;
        self.committed_transactions = self
            .blocks
            .iter()
            .flat_map(|b| b.transactions.iter().map(Transaction::to_digest))
            .collect();
        self.seen_transactions = self.committed_transactions.clone();
        self.mempool = Mempool::default();
        self.signed_mempool.clear();
        self.prepared_block = None;
        self.recent_trades.clear();
        Ok(())
    }
    pub fn accept_checkpoint(&mut self, cp: Checkpoint) -> Result<(), String> {
        let manifest = self.manifest.clone().ok_or("unsigned checkpoint")?;
        let mut candidate = Self::new_signed(manifest);
        candidate.install_checkpoint(cp.clone())?;
        for block in &cp.blocks {
            if let Some(local) = self.block_at(block.sequence as usize) {
                if local.to_digest() != block.to_digest() {
                    return Err("checkpoint conflicts with local history".into());
                }
            }
        }
        if let Some(path) = &self.storage_path {
            cp.write(&path.with_extension("checkpoint"))?;
        }
        self.install_checkpoint(cp)
    }
    pub(super) fn maybe_checkpoint(&mut self) -> Result<(), String> {
        let interval = std::env::var("LIGHT_DEX_CHECKPOINT_INTERVAL")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        if interval == 0 || !self.height().is_multiple_of(interval) || self.manifest.is_none() {
            return Ok(());
        }
        let Some(path) = self.storage_path.clone() else {
            return Ok(());
        };
        let cp = self.checkpoint().ok_or("checkpoint unavailable")?;
        cp.write(&path.with_extension("checkpoint"))?;
        // Preserve the prior checkpoint's replay suffix until the next replacement.
        // No destructive pruning occurs until two durable snapshots exist.
        let previous = path.with_extension("checkpoint.previous");
        if previous.exists() {
            let previous_cp = Checkpoint::read(&previous)?;
            let previous_height = previous_cp.base + previous_cp.blocks.len();
            let journal_path = Self::journal_path(&path);
            if journal_path.exists() {
                let retained =
                    std::io::BufReader::new(File::open(&journal_path).map_err(|e| e.to_string())?)
                        .lines()
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|e| e.to_string())?;
                // Rewrite in place only while the node has no pending commit. Atomic snapshot
                // is already sufficient for recovery if interrupted while compacting.
                let mut file = OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&journal_path)
                    .map_err(|e| e.to_string())?;
                for line in retained {
                    let record: CommittedBlock =
                        serde_json::from_str(&line).map_err(|e| e.to_string())?;
                    if record.block.sequence >= previous_height as u64 {
                        writeln!(file, "{line}").map_err(|e| e.to_string())?;
                    }
                }
                file.sync_all().map_err(|e| e.to_string())?;
            }
        }
        let start = cp.base - self.base_height;
        self.blocks.drain(..start);
        self.certificates.drain(..start);
        self.base_height = cp.base;
        self.committed_transactions = self
            .blocks
            .iter()
            .flat_map(|b| b.transactions.iter().map(Transaction::to_digest))
            .collect();
        self.seen_transactions = self.committed_transactions.clone();
        Ok(())
    }
}
