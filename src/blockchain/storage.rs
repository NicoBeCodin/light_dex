use super::*;

impl BlockchainState {
    pub(crate) fn trace_path(&self) -> Option<PathBuf> {
        self.storage_path
            .as_ref()
            .map(|path| path.with_extension("latency.jsonl"))
    }
    pub(crate) fn stage_trace_path(&self) -> Option<PathBuf> {
        self.storage_path
            .as_ref()
            .map(|path| path.with_extension("stages.jsonl"))
    }
    pub fn journal_path(path: &Path) -> PathBuf {
        path.with_extension("json.journal")
    }

    pub fn new() -> Self {
        BlockchainState {
            blocks: Vec::new(),
            base_height: 0,
            certificates: Vec::new(),
            mempool: Mempool::default(),
            signed_mempool: HashMap::new(),
            manifest: None,
            seen_transactions: HashSet::new(),
            committed_transactions: HashSet::new(),
            exchange_state: ExchangeState::default(),
            recent_trades: VecDeque::new(),
            storage_path: None,
            journal: std::sync::OnceLock::new(),
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
        state.exchange_state =
            ExchangeState::with_claim_faucet(manifest.faucet).expect("validated session faucet");
        state.manifest = Some(manifest);
        state
    }

    pub fn signed_chain_id(&self) -> Option<&str> {
        self.manifest
            .as_ref()
            .map(|manifest| manifest.chain_id.as_str())
    }

    pub fn load_signed(path: PathBuf, manifest: Arc<SessionManifest>) -> Result<Self, String> {
        if manifest.initial_state_root
            != crate::message::hex_encode(
                &ExchangeState::with_claim_faucet(manifest.faucet)
                    .map_err(|error| error.to_string())?
                    .state_digest(),
            )
        {
            return Err("session manifest initial state root differs from local genesis".into());
        }
        Self::load_with_manifest(path, Some(manifest))
    }

    fn load_with_manifest(
        path: PathBuf,
        manifest: Option<Arc<SessionManifest>>,
    ) -> Result<Self, String> {
        let mut state = match manifest {
            Some(manifest) => Self::new_signed(manifest),
            None => Self::new(),
        };
        // Old snapshots remain readable. New commits are appended to the
        // adjacent journal, so recovery can also resume a legacy chain.
        if path.exists() {
            let mut file = File::open(&path).map_err(|error| error.to_string())?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|error| error.to_string())?;
            let blocks: Vec<Block> =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            if state.manifest.is_some() && !blocks.is_empty() {
                return Err("signed sessions require certified journal records".into());
            }
            for block in blocks {
                state.commit_block(&block)?;
            }
        }
        for checkpoint_path in [
            path.with_extension("checkpoint"),
            path.with_extension("checkpoint.previous"),
        ] {
            if checkpoint_path.exists() {
                match super::checkpoint::Checkpoint::read(&checkpoint_path)
                    .and_then(|checkpoint| state.install_checkpoint(checkpoint))
                {
                    Ok(()) => break,
                    Err(error) => eprintln!("Checkpoint recovery rejected: {error}"),
                }
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
                    let committed: CommittedBlock =
                        serde_json::from_slice(line).map_err(|error| error.to_string())?;
                    if committed.block.sequence < state.height() as u64 {
                        continue;
                    }
                    state.commit_block_with_certificate(&committed.block, committed.certificate)?;
                } else {
                    let block: Block =
                        serde_json::from_slice(line).map_err(|error| error.to_string())?;
                    state.commit_block(&block)?;
                }
            }
        }
        state.storage_path = Some(path);
        Ok(state)
    }

    pub(super) fn persist(
        &self,
        block: &Block,
        certificate: Option<&CommitCertificate>,
    ) -> Result<(), String> {
        let Some(path) = &self.storage_path else {
            return Ok(());
        };
        self.journal
            .get_or_init(|| journal::Journal::start(Self::journal_path(path)))
            .write(block.clone(), certificate.cloned())?
            .recv()
            .map_err(|_| "journal writer closed".to_string())??;
        Ok(())
    }

    pub(crate) fn persist_commit_async(
        &mut self,
        block: Block,
        certificate: Option<CommitCertificate>,
    ) -> Result<std::sync::mpsc::Receiver<Result<DurableBlock, String>>, String> {
        self.validate_block(&block)?;
        match (&self.manifest, &certificate) {
            (Some(manifest), Some(proof)) => {
                verify_commit_certificate(&block, proof, manifest).map_err(|e| e.to_string())?
            }
            (None, None) => {}
            _ => return Err("commit certificate/session mismatch".into()),
        }
        if let Some(path) = &self.storage_path {
            self.journal
                .get_or_init(|| journal::Journal::start(Self::journal_path(path)))
                .write(block, certificate)
        } else {
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = tx.send(Ok(DurableBlock {
                block,
                certificate,
                write_us: 0,
            }));
            Ok(rx)
        }
    }

    pub(crate) fn commit_durable(
        &mut self,
        record: DurableBlock,
    ) -> Result<(Block, Vec<Result<Vec<Trade>, ExchangeError>>), String> {
        let outcomes = self.commit_block_inner(&record.block, record.certificate, true)?;
        Ok((record.block, outcomes))
    }
}
