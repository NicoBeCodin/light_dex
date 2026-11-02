use super::*;

pub(super) fn commit_trace_latencies(
    dir: Option<&str>,
    submissions: &HashMap<String, u128>,
) -> Result<Vec<u64>, String> {
    let Some(dir) = dir else {
        return Ok(Vec::new());
    };
    let path = Path::new(dir).join("node-0.latency.jsonl");
    if !path.exists() {
        return Ok(Vec::new());
    }
    #[derive(serde::Deserialize)]
    struct Trace {
        committed_unix_us: u128,
        successful: Vec<String>,
    }
    let input = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let mut seen = std::collections::HashSet::new();
    let mut latencies = Vec::new();
    for line in input.lines() {
        let trace: Trace = serde_json::from_str(line).map_err(|error| error.to_string())?;
        for digest in trace.successful {
            if let Some(start) = submissions.get(&digest) {
                if seen.insert(digest) {
                    let us = trace
                        .committed_unix_us
                        .checked_sub(*start)
                        .ok_or("benchmark clock moved backwards")?;
                    latencies.push((us / 1000).min(u64::MAX as u128) as u64);
                }
            }
        }
    }
    Ok(latencies)
}

pub(super) fn market_trace_outcomes(
    dir: Option<&str>,
    start_sequence: u64,
    end_sequence: u64,
) -> Result<
    (
        BTreeMap<u64, MarketOutcomeSummary>,
        BTreeMap<String, u64>,
        u64,
    ),
    String,
> {
    let Some(dir) = dir else {
        return Ok((BTreeMap::new(), BTreeMap::new(), 0));
    };
    let path = Path::new(dir).join("node-0.latency.jsonl");
    if !path.exists() {
        return Ok((BTreeMap::new(), BTreeMap::new(), 0));
    }
    #[derive(serde::Deserialize)]
    struct Outcome {
        market_id: Option<u64>,
        success: bool,
        fills: u64,
        error: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Trace {
        sequence: u64,
        #[serde(default)]
        outcomes: Vec<Outcome>,
    }
    let input = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let mut markets = BTreeMap::<u64, MarketOutcomeSummary>::new();
    let mut failures = BTreeMap::<String, u64>::new();
    let mut rows = 0u64;
    for line in input.lines() {
        let trace: Trace = serde_json::from_str(line).map_err(|error| error.to_string())?;
        if trace.sequence < start_sequence || trace.sequence >= end_sequence {
            continue;
        }
        for outcome in trace.outcomes {
            rows += 1;
            if let Some(market_id) = outcome.market_id {
                let entry = markets.entry(market_id).or_default();
                entry.included += 1;
                entry.successful += u64::from(outcome.success);
                entry.failed += u64::from(!outcome.success);
                entry.fills += outcome.fills;
            }
            if !outcome.success {
                *failures
                    .entry(outcome.error.unwrap_or_else(|| "unknown".into()))
                    .or_default() += 1;
            }
        }
    }
    Ok((markets, failures, rows))
}

pub(super) fn arg(name: &str) -> Option<String> {
    let args = env::args().collect::<Vec<_>>();
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}
pub(super) fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}
pub(super) fn project_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}
pub(super) fn command_output(project: &Path, cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .current_dir(project)
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default()
}
pub(super) fn cpu_model() -> String {
    fs::read_to_string("/proc/cpuinfo")
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix("model name\t: "))
        .unwrap_or("unknown")
        .to_string()
}
pub(super) fn diff_hash(project: &Path) -> String {
    let output = Command::new("git")
        .args(["diff", "--binary"])
        .current_dir(project)
        .output()
        .ok();
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    if let Some(output) = output {
        hash.update(output.stdout);
    }
    hex_encode(&hash.finalize())
}
pub(super) fn file_hash(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = fs::read(path)?;
    let mut hash = Sha256::new();
    hash.update(bytes);
    Ok(hex_encode(&hash.finalize()))
}
pub(super) fn proc_number(pid: u32, file: &str, key: &str) -> Option<u64> {
    fs::read_to_string(format!("/proc/{pid}/{file}"))
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix(key)?
                .trim()
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
}
pub(super) fn cpu_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    Some(tail.get(11)?.parse::<u64>().ok()? + tail.get(12)?.parse::<u64>().ok()?)
}
pub(super) fn sample(
    snapshot: &ControlSnapshot,
    elapsed: Duration,
    attempted_extra: u64,
) -> Sample {
    Sample {
        elapsed_ms: elapsed.as_millis().min(u64::MAX as u128) as u64,
        submitted: snapshot.metrics.submitted,
        attempted_extra,
        fatal_logs: snapshot
            .logs
            .iter()
            .filter(|line| {
                line.contains("panicked")
                    || line.contains("node failed:")
                    || line.contains("Dropping peer")
                    || line.contains("Could not reconnect")
            })
            .rev()
            .take(20)
            .cloned()
            .collect(),
        controller_cpu_ticks: cpu_ticks(std::process::id()),
        controller_rss_kib: proc_number(std::process::id(), "status", "VmRSS:"),
        feed_age_ms: snapshot
            .agents
            .iter()
            .filter_map(|agent| agent.feed_age_ms)
            .min(),
        nodes: snapshot
            .nodes
            .iter()
            .map(|node| NodeSample {
                id: node.node_id,
                host_label: node.host_label.clone(),
                status_rtt_ms: node.status_rtt_ms,
                state: node.state.clone(),
                height: node.status.as_ref().map(|s| s.chain_height),
                view: node.status.as_ref().map(|s| s.current_view),
                mempool: node.status.as_ref().map(|s| s.mempool_size),
                seen: node.status.as_ref().map(|s| s.seen_transactions),
                successful: node.status.as_ref().map(|s| s.transactions_committed),
                failed: node.status.as_ref().map(|s| s.transactions_failed),
                trades: node.status.as_ref().map(|s| s.trades_committed),
                catch_up_events: node.status.as_ref().map(|s| s.catch_up_events),
                branch_sync_attempts: node.status.as_ref().map(|s| s.branch_sync_attempts),
                branch_sync_failures: node.status.as_ref().map(|s| s.branch_sync_failures),
                proposal_parent_misses: node.status.as_ref().map(|s| s.proposal_parent_misses),
                peer_queued_bytes: node.status.as_ref().map(|s| s.peer_queued_bytes),
                peer_write_timeouts: node.status.as_ref().map(|s| s.peer_write_timeouts),
                peer_write_errors: node.status.as_ref().map(|s| s.peer_write_errors),
                peer_queue_full: node.status.as_ref().map(|s| s.peer_queue_full),
                peer_reconnects: node.status.as_ref().map(|s| s.peer_reconnects),
                rss_kib: (node.host_label == "Local")
                    .then(|| proc_number(node.pid, "status", "VmRSS:"))
                    .flatten(),
                cpu_ticks: (node.host_label == "Local")
                    .then(|| cpu_ticks(node.pid))
                    .flatten(),
                write_bytes: (node.host_label == "Local")
                    .then(|| proc_number(node.pid, "io", "write_bytes:"))
                    .flatten(),
            })
            .collect(),
    }
}
pub(super) fn best_status(snapshot: &ControlSnapshot) -> Option<&NodeStatus> {
    snapshot
        .nodes
        .iter()
        .filter(|node| node.state == "ok")
        .filter_map(|node| node.status.as_ref())
        .max_by_key(|status| status.chain_height)
}
pub(super) fn percentile(values: &[u64], fraction: f64) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut values = values.to_vec();
    values.sort_unstable();
    let index = ((values.len() - 1) as f64 * fraction).ceil() as usize;
    values.get(index).copied()
}
pub(super) fn correctness(snapshot: &ControlSnapshot, expected_nodes: u64) -> Result<(), String> {
    if snapshot.nodes.len() != expected_nodes as usize
        || snapshot.nodes.iter().any(|node| node.state == "dead")
    {
        return Err("one or more validators exited".into());
    }
    Ok(())
}

pub(super) struct PersistedState {
    pub height_min: usize,
    pub height_max: usize,
    pub consistent_tip: bool,
}

pub(super) fn persisted_correctness(
    snapshot: &ControlSnapshot,
    expected_nodes: u64,
) -> Result<PersistedState, String> {
    let data_dir = snapshot
        .active_data_dir
        .as_ref()
        .ok_or("missing node data directory")?;
    let manifest = Arc::new(
        SessionManifest::load(&Path::new(data_dir).join("manifest.json"))
            .map_err(|error| error.to_string())?,
    );
    let mut replicas = Vec::new();
    for id in 0..expected_nodes {
        let audit_path = Path::new(data_dir).join(format!("node-{id}.audit.json"));
        if audit_path.exists() {
            #[derive(serde::Deserialize)]
            struct Audit {
                node_id: u64,
                block_digests: Vec<String>,
                state_root: String,
            }
            let audit: Audit =
                serde_json::from_slice(&fs::read(&audit_path).map_err(|e| e.to_string())?)
                    .map_err(|error| format!("node {id} audit decode failed: {error}"))?;
            if audit.node_id != id || audit.block_digests.is_empty() {
                return Err(format!("node {id} audit identity or chain is invalid"));
            }
            let blocks = audit
                .block_digests
                .iter()
                .map(|digest| {
                    light_dex::message::hex_decode::<32>(digest).map_err(|e| e.to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            let root = light_dex::message::hex_decode::<32>(&audit.state_root)
                .map_err(|error| format!("node {id} audit root invalid: {error}"))?;
            replicas.push((id, blocks, root));
            continue;
        }
        let state = BlockchainState::load_signed(
            Path::new(data_dir).join(format!("node-{id}.json")),
            manifest.clone(),
        )
        .map_err(|error| format!("node {id} chain replay failed: {error}"))?;
        replicas.push((
            id,
            state
                .blocks
                .iter()
                .map(|block| block.to_digest())
                .collect::<Vec<_>>(),
            state.exchange_state.state_digest(),
        ));
    }
    validate_replayed_replicas(&replicas)
}

fn validate_replayed_replicas(
    replicas: &[(u64, Vec<Digest>, Digest)],
) -> Result<PersistedState, String> {
    let min_height = replicas
        .iter()
        .map(|(_, blocks, _)| blocks.len())
        .min()
        .unwrap_or(0);
    let max_height = replicas
        .iter()
        .map(|(_, blocks, _)| blocks.len())
        .max()
        .unwrap_or(0);
    if max_height.saturating_sub(min_height) > 2 {
        return Err(format!(
            "persisted replica height skew is {} blocks",
            max_height - min_height
        ));
    }
    for height in 0..min_height {
        let expected = replicas[0].1[height];
        if let Some((id, _, _)) = replicas
            .iter()
            .find(|(_, blocks, _)| blocks[height] != expected)
        {
            return Err(format!(
                "node {id} persisted a conflicting block at sequence {height}"
            ));
        }
    }
    if min_height == max_height {
        let expected = replicas[0].2;
        if let Some((id, _, _)) = replicas.iter().find(|(_, _, root)| *root != expected) {
            return Err(format!(
                "node {id} replayed a different exchange state root"
            ));
        }
    }
    Ok(PersistedState {
        height_min: min_height,
        height_max: max_height,
        consistent_tip: min_height == max_height,
    })
}

#[cfg(test)]
mod replay_tests {
    use super::validate_replayed_replicas;

    #[test]
    fn shutdown_one_block_ahead_is_not_a_fork() {
        let common = [1; 32];
        let persisted = validate_replayed_replicas(&[
            (0, vec![common], [2; 32]),
            (1, vec![common, [3; 32]], [4; 32]),
        ])
        .unwrap();
        assert_eq!((persisted.height_min, persisted.height_max), (1, 2));
        assert!(!persisted.consistent_tip);
    }

    #[test]
    fn conflicting_prefix_and_large_lag_are_rejected() {
        assert!(
            validate_replayed_replicas(
                &[(0, vec![[1; 32]], [2; 32]), (1, vec![[3; 32]], [2; 32]),]
            )
            .is_err()
        );
        assert!(
            validate_replayed_replicas(&[
                (0, vec![[1; 32]], [2; 32]),
                (1, vec![[1; 32], [2; 32], [3; 32], [4; 32]], [5; 32]),
            ])
            .is_err()
        );
    }
}
pub(super) fn wait_for(
    controller: &ControllerState,
    deadline: Instant,
    predicate: impl Fn(&ControlSnapshot) -> bool,
) -> Result<ControlSnapshot, String> {
    loop {
        let snapshot = controller.snapshot();
        if predicate(&snapshot) {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "cluster wait timed out: {:?}",
                snapshot.logs.iter().rev().take(30).collect::<Vec<_>>()
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
}
