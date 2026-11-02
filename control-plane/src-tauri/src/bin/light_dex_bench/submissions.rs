//! FIFO worker shards: an account always uses the same worker, so concurrent
//! sockets do not reorder that account's submission stream.
use super::*;
use std::sync::mpsc;

pub(super) struct SubmissionResult {
    pub account: u64,
    pub nonce: u64,
    pub accepted: bool,
    pub first: bool,
    pub elapsed_ms: u64,
    pub error_category: Option<&'static str>,
}
pub(super) struct Submissions {
    queues: Vec<mpsc::SyncSender<(u64, Transaction, bool, Instant)>>,
    results: mpsc::Receiver<SubmissionResult>,
    handles: Vec<std::thread::JoinHandle<()>>,
}
impl Submissions {
    pub fn new(controller: &ControllerState) -> Self {
        let workers = arg("--submission-workers")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(16)
            .clamp(1, 64);
        let target = match arg("--submission-target").as_deref() {
            Some("node-0") => TargetNode::Node { node_id: 0 },
            Some("random") => TargetNode::Random,
            _ => TargetNode::Primary,
        };
        let (completed, results) = mpsc::channel();
        let mut queues = Vec::new();
        let mut handles = Vec::new();
        for _ in 0..workers {
            let (tx, rx) = mpsc::sync_channel::<(u64, Transaction, bool, Instant)>(32);
            queues.push(tx);
            let controller = controller.clone();
            let target = target.clone();
            let completed = completed.clone();
            handles.push(thread::spawn(move || {
                while let Ok((account, transaction, first, at)) = rx.recv() {
                    let nonce = transaction.nonce();
                    let submission =
                        controller.submit_benchmark_transaction(transaction, target.clone());
                    let accepted = submission.is_ok();
                    let _ = completed.send(SubmissionResult {
                        account,
                        nonce,
                        accepted,
                        first,
                        elapsed_ms: at.elapsed().as_millis().min(u64::MAX as u128) as u64,
                        error_category: submission
                            .err()
                            .map(|error| categorize_submission_error(&error)),
                    });
                }
            }));
        }
        Self {
            queues,
            results,
            handles,
        }
    }
    pub fn send(&self, account: u64, tx: Transaction, first: bool) -> bool {
        self.queues[account as usize % self.queues.len()]
            .try_send((account, tx, first, Instant::now()))
            .is_ok()
    }
    pub fn drain(&self) -> Vec<SubmissionResult> {
        self.results.try_iter().collect()
    }
}

pub(super) fn categorize_submission_error(error: &str) -> &'static str {
    if error.contains("no confirmed nonce") {
        "no_confirmed_nonce"
    } else if error.contains("permits nonce") {
        "nonce_window"
    } else if error.contains("backlog high") {
        "controller_overload"
    } else if error.contains("no available target") {
        "no_target"
    } else if error.contains("timed out") {
        "transport_timeout"
    } else if error.contains("rejected") {
        "validator_rejected"
    } else {
        "other"
    }
}
impl Drop for Submissions {
    fn drop(&mut self) {
        self.queues.clear();
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}
