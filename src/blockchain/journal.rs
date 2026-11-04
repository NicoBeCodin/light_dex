//! One ordered writer per chain. Completion is a durability token, not merely
//! an enqueue acknowledgement. A failed append poisons the writer until restart.
use super::*;
use std::sync::mpsc;
use std::time::Instant;

#[derive(Debug)]
pub struct DurableBlock {
    pub(super) block: Block,
    pub(super) certificate: Option<CommitCertificate>,
    pub(crate) write_us: u64,
}
struct Request {
    block: Block,
    certificate: Option<CommitCertificate>,
    reply: mpsc::Sender<Result<DurableBlock, String>>,
}
#[derive(Clone, Copy)]
enum SyncMode {
    All,
    Data,
}

impl SyncMode {
    fn from_env() -> Self {
        if std::env::var("LIGHT_DEX_JOURNAL_SYNC_MODE").as_deref() == Ok("data") {
            Self::Data
        } else {
            Self::All
        }
    }
    fn sync(self, file: &File) -> std::io::Result<()> {
        match self {
            Self::All => file.sync_all(),
            Self::Data => file.sync_data(),
        }
    }
}
#[derive(Debug)]
pub(super) struct Journal(mpsc::SyncSender<Request>);
impl Journal {
    pub(super) fn start(path: PathBuf) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Request>(2);
        let sync_mode = SyncMode::from_env();
        let max_bytes = std::env::var("LIGHT_DEX_JOURNAL_MAX_BYTES")
            .ok()
            .map(|text| {
                let value = text.parse::<u64>().expect("invalid journal byte limit");
                assert!(value > 0, "journal byte limit must be positive");
                value
            });
        std::thread::Builder::new()
            .name("durable-journal".into())
            .spawn(move || {
                let mut file: Option<File> = None;
                let mut failed: Option<String> = None;
                while let Ok(request) = rx.recv() {
                    let started = Instant::now();
                    let result = if let Some(error) = &failed {
                        Err(error.clone())
                    } else {
                        append(
                            &path,
                            &mut file,
                            &request.block,
                            request.certificate.as_ref(),
                            sync_mode,
                            max_bytes,
                        )
                    };
                    if let Err(error) = &result {
                        failed = Some(error.clone());
                    }
                    crate::profile::record(
                        "journal_total",
                        started.elapsed(),
                        request.block.transactions.len() as u64,
                    );
                    let _ = request.reply.send(result.map(|()| DurableBlock {
                        block: request.block,
                        certificate: request.certificate,
                        write_us: started.elapsed().as_micros().min(u64::MAX as u128) as u64,
                    }));
                }
            })
            .expect("journal writer");
        Self(tx)
    }
    pub(super) fn write(
        &self,
        block: Block,
        certificate: Option<CommitCertificate>,
    ) -> Result<mpsc::Receiver<Result<DurableBlock, String>>, String> {
        let (reply, rx) = mpsc::channel();
        self.0
            .try_send(Request {
                block,
                certificate,
                reply,
            })
            .map_err(|_| "journal writer unavailable or queue full".to_string())?;
        Ok(rx)
    }
}

fn append(
    path: &Path,
    file: &mut Option<File>,
    block: &Block,
    certificate: Option<&CommitCertificate>,
    sync_mode: SyncMode,
    max_bytes: Option<u64>,
) -> Result<(), String> {
    let parent = path.parent().ok_or("invalid journal directory")?;
    if file.is_none() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let created = !path.exists();
        let opened = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        if created {
            opened.sync_all().map_err(|e| e.to_string())?;
            File::open(parent)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
        }
        *file = Some(opened);
    }
    // Borrow the record: serializing does not clone a full block again.
    #[derive(Serialize)]
    struct Record<'a> {
        block: &'a Block,
        certificate: &'a CommitCertificate,
    }
    let stage_started = Instant::now();
    let mut bytes = match certificate {
        Some(certificate) => serde_json::to_vec(&Record { block, certificate }),
        None => serde_json::to_vec(block),
    }
    .map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    crate::profile::record(
        "journal_encode",
        stage_started.elapsed(),
        block.transactions.len() as u64,
    );
    let file = file.as_mut().unwrap();
    let stage_started = Instant::now();
    let previous_len = file.metadata().map_err(|e| e.to_string())?.len();
    if max_bytes.is_some_and(|limit| previous_len.saturating_add(bytes.len() as u64) > limit) {
        return Err("journal storage budget reached; commit not published".into());
    }
    crate::profile::record("journal_metadata", stage_started.elapsed(), 1);
    let stage_started = Instant::now();
    let write_result = file.write_all(&bytes);
    crate::profile::record("journal_write", stage_started.elapsed(), bytes.len() as u64);
    let stage_started = Instant::now();
    let result = write_result.and_then(|()| sync_mode.sync(file));
    if result.is_ok() {
        crate::profile::record("journal_sync", stage_started.elapsed(), bytes.len() as u64);
    }
    if let Err(error) = result {
        let rollback = file.set_len(previous_len).and_then(|()| file.sync_all());
        return Err(format!(
            "journal append failed: {error}; rollback: {rollback:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sync_modes_replay_and_discard_an_incomplete_tail() {
        for mode in [SyncMode::All, SyncMode::Data] {
            let directory = std::env::temp_dir().join(format!(
                "light-dex-journal-mode-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join("node.json");
            let journal = BlockchainState::journal_path(&path);
            let mut file = None;
            append(
                &journal,
                &mut file,
                &Block::new_initial_block(),
                None,
                mode,
                None,
            )
            .unwrap();
            drop(file);
            let mut tail = OpenOptions::new().append(true).open(&journal).unwrap();
            tail.write_all(b"{\"interrupted\":").unwrap();
            tail.sync_all().unwrap();
            drop(tail);
            let loaded = BlockchainState::load(path).unwrap();
            assert_eq!(loaded.blocks.len(), 1);
            assert!(std::fs::read(&journal).unwrap().ends_with(b"\n"));
            std::fs::remove_dir_all(directory).unwrap();
        }
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    #[test]
    fn exceeding_storage_budget_keeps_existing_journal_unchanged() {
        let dir = std::env::temp_dir().join(format!("ldex-budget-{}", rand::random::<u64>()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("chain.journal");
        let mut file = None;
        let block = Block::new_initial_block();
        append(&path, &mut file, &block, None, SyncMode::All, None).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(
            append(
                &path,
                &mut file,
                &block,
                None,
                SyncMode::All,
                Some(before.len() as u64)
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        drop(file);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
