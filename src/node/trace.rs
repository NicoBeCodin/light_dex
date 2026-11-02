//! Opt-in benchmark timestamps, outside protocol state. Validator 0 writes
//! commit traces; sampled stage traces come from all validators. Comparing
//! stage timestamps across nodes requires a shared host clock.
use std::{
    io::{BufWriter, Write},
    path::PathBuf,
    sync::mpsc,
};
#[derive(serde::Serialize)]
pub(super) struct CommitTrace {
    pub sequence: u64,
    pub committed_unix_us: u128,
    pub successful: Vec<String>,
    pub outcomes: Vec<TraceOutcome>,
}
#[derive(serde::Serialize)]
pub(super) struct TraceOutcome {
    pub digest: String,
    pub market_id: Option<u64>,
    pub success: bool,
    pub fills: usize,
    pub error: Option<String>,
}
pub(super) struct TraceWriter {
    tx: Option<mpsc::SyncSender<CommitTrace>>,
    worker: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    dropped: u64,
}
impl TraceWriter {
    pub fn new(path: PathBuf) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let (tx, rx) = mpsc::sync_channel::<CommitTrace>(128);
        let worker = std::thread::spawn(move || {
            let mut file = BufWriter::new(file);
            while let Ok(record) = rx.recv() {
                serde_json::to_writer(&mut file, &record)?;
                file.write_all(b"\n")?;
            }
            file.flush()
        });
        Ok(Self {
            tx: Some(tx),
            worker: Some(worker),
            dropped: 0,
        })
    }
    pub fn send(
        &mut self,
        sequence: u64,
        successful: Vec<String>,
        outcomes: Vec<TraceOutcome>,
        committed_unix_us: u128,
    ) {
        if self
            .tx
            .as_ref()
            .unwrap()
            .try_send(CommitTrace {
                sequence,
                committed_unix_us,
                successful,
                outcomes,
            })
            .is_err()
        {
            self.dropped += 1;
        }
    }
}
impl Drop for TraceWriter {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(worker) = self.worker.take() {
            println!(
                "PROFILE trace_dropped={} trace_result={:?}",
                self.dropped,
                worker.join()
            );
        }
    }
}

#[derive(serde::Serialize)]
pub(super) struct StageTrace {
    pub node_id: u64,
    pub digest: String,
    pub block_digest: Option<String>,
    pub stage: &'static str,
    pub sequence: Option<u64>,
    pub view: u64,
    pub unix_us: u128,
}

/// Optional sampled trace. A bounded writer keeps file I/O outside the node
/// event loop; dropped records are reported at shutdown rather than blocking
/// consensus when tracing cannot keep up.
pub(super) struct StageWriter {
    tx: Option<mpsc::SyncSender<StageTrace>>,
    worker: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    sample_mod: u64,
    dropped: u64,
}

impl StageWriter {
    pub fn new(path: PathBuf, sample_mod: u64) -> std::io::Result<Self> {
        assert!(sample_mod > 0);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let (tx, rx) = mpsc::sync_channel::<StageTrace>(4_096);
        let worker = std::thread::spawn(move || {
            let mut file = BufWriter::new(file);
            while let Ok(record) = rx.recv() {
                serde_json::to_writer(&mut file, &record)?;
                file.write_all(b"\n")?;
            }
            file.flush()
        });
        Ok(Self {
            tx: Some(tx),
            worker: Some(worker),
            sample_mod,
            dropped: 0,
        })
    }

    pub fn record(
        &mut self,
        node_id: u64,
        digest: [u8; 32],
        stage: &'static str,
        sequence: Option<u64>,
        block_digest: Option<[u8; 32]>,
        view: u64,
    ) {
        // The first eight digest bytes select a stable, repeatable sample.
        if u64::from_be_bytes(digest[..8].try_into().unwrap()) % self.sample_mod != 0 {
            return;
        }
        let unix_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        let record = StageTrace {
            node_id,
            digest: crate::message::hex_encode(&digest),
            block_digest: block_digest.map(|digest| crate::message::hex_encode(&digest)),
            stage,
            sequence,
            view,
            unix_us,
        };
        if self
            .tx
            .as_ref()
            .is_some_and(|tx| tx.try_send(record).is_err())
        {
            self.dropped += 1;
        }
    }
}

impl Drop for StageWriter {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(worker) = self.worker.take() {
            println!(
                "PROFILE stage_trace_dropped={} stage_trace_result={:?}",
                self.dropped,
                worker.join()
            );
        }
    }
}

impl<C: super::Consensus> super::Node<C> {
    pub(super) fn trace_tx_stage(
        &mut self,
        digest: super::Digest,
        stage: &'static str,
        sequence: Option<u64>,
    ) {
        if let Some(trace) = &mut self.stage_trace {
            trace.record(
                self.node_id,
                digest,
                stage,
                sequence,
                None,
                self.consensus.current_view(),
            );
        }
    }

    pub(super) fn trace_block_stage(&mut self, block: &super::Block, stage: &'static str) {
        if self.stage_trace.is_none() {
            return;
        }
        let block_digest = block.to_digest();
        for tx in &block.transactions {
            if let Some(trace) = &mut self.stage_trace {
                trace.record(
                    self.node_id,
                    tx.to_digest(),
                    stage,
                    Some(block.sequence),
                    Some(block_digest),
                    self.consensus.current_view(),
                );
            }
        }
    }
}
