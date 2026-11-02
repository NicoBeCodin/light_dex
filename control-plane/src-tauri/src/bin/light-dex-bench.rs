//! Headless, opt-in pilot benchmarks. Nothing is launched by `cargo test`.
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
use light_dex::node::TransactionOutcome;
use light_dex::{
    blockchain::{BlockchainState, Digest, Transaction},
    crypto::SessionManifest,
    exchange::order::{OrderType, Side},
    message::hex_encode,
    node::{AccountNonce, NodeStatus},
    participants::strategies::AgentStrategy,
};
use light_dex_control_plane_lib::{
    controller::ControllerState,
    hundred_participant_fleet_template, participant_fleet_template,
    types::{
        AgentConfig, ConsensusAlgorithm, ControlSnapshot, LaunchConfig, PollMode, RemotePlacement,
        TargetNode,
    },
};
use serde::Serialize;
use std::sync::Arc;

#[path = "light_dex_bench/driver.rs"]
mod driver;
#[path = "light_dex_bench/measurement.rs"]
mod measurement;
#[path = "light_dex_bench/submissions.rs"]
mod submissions;
#[path = "light_dex_bench/trials.rs"]
mod trials;

use driver::*;
use measurement::*;
use trials::*;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Algorithm {
    Pbft,
    Hotstuff,
    Hotstuff2,
    Votor,
}
impl Algorithm {
    fn label(self) -> &'static str {
        match self {
            Self::Pbft => "pbft",
            Self::Hotstuff => "hotstuff",
            Self::Hotstuff2 => "hotstuff2",
            Self::Votor => "votor",
        }
    }
    fn config(self) -> ConsensusAlgorithm {
        match self {
            Self::Pbft => ConsensusAlgorithm::Pbft,
            Self::Hotstuff => ConsensusAlgorithm::Hotstuff,
            Self::Hotstuff2 => ConsensusAlgorithm::Hotstuff2,
            Self::Votor => ConsensusAlgorithm::Votor,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Workload {
    Mixed,
    Transfer,
    Resting,
    Orderflow,
    Multimarket,
    BalancedSix,
}
impl Workload {
    fn label(self) -> &'static str {
        match self {
            Self::Mixed => "mixed",
            Self::Transfer => "transfer",
            Self::Resting => "resting",
            Self::Orderflow => "orderflow",
            Self::Multimarket => "multimarket",
            Self::BalancedSix => "balanced_six",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct Trial {
    name: String,
    algorithm: Algorithm,
    nodes: u64,
    timeout_ms: u64,
    offered_tps: u64,
}

#[derive(Serialize)]
struct NodeSample {
    id: u64,
    host_label: String,
    status_rtt_ms: Option<u64>,
    state: String,
    height: Option<usize>,
    view: Option<u64>,
    mempool: Option<usize>,
    seen: Option<usize>,
    successful: Option<u64>,
    failed: Option<u64>,
    trades: Option<u64>,
    catch_up_events: Option<u64>,
    branch_sync_attempts: Option<u64>,
    branch_sync_failures: Option<u64>,
    proposal_parent_misses: Option<u64>,
    peer_queued_bytes: Option<usize>,
    peer_write_timeouts: Option<u64>,
    peer_write_errors: Option<u64>,
    peer_queue_full: Option<u64>,
    peer_reconnects: Option<u64>,
    rss_kib: Option<u64>,
    cpu_ticks: Option<u64>,
    write_bytes: Option<u64>,
}

#[derive(Serialize)]
struct Sample {
    elapsed_ms: u64,
    submitted: u64,
    attempted_extra: u64,
    fatal_logs: Vec<String>,
    controller_cpu_ticks: Option<u64>,
    controller_rss_kib: Option<u64>,
    feed_age_ms: Option<u64>,
    nodes: Vec<NodeSample>,
}

#[derive(Serialize)]
struct Summary {
    latency_target_ms: u64,
    latency_slo_met: Option<bool>,
    end_to_end_p50_ms: Option<u64>,
    end_to_end_p95_ms: Option<u64>,
    end_to_end_samples: usize,
    end_to_end_unmatched: usize,
    scheduled_slots: u64,
    blocked_slots: u64,
    unaccounted_slots: u64,
    scheduled_to_commit_p50_ms: Option<u64>,
    scheduled_to_commit_p95_ms: Option<u64>,
    offered_submissions: u64,
    rejected_initial_submissions: u64,
    rejected_submission_reasons: BTreeMap<String, u64>,
    admission_p95_ms: Option<u64>,
    observed_confirmation_p95_ms: Option<u64>,
    observed_confirmation_samples: usize,
    unresolved_after_drain: usize,
    name: String,
    algorithm: Algorithm,
    nodes: u64,
    timeout_ms: u64,
    offered_tps: u64,
    participants: usize,
    driver_accounts: usize,
    workload: Workload,
    in_flight_per_account: usize,
    result: String,
    reason: Option<String>,
    measured_seconds: f64,
    attempted_extra: u64,
    delivered_extra_tps: f64,
    min_responsive_during_measurement: usize,
    repair_submissions: u64,
    retry_submissions: u64,
    pending_at_end: usize,
    submitted_writes: u64,
    successful_tps: f64,
    accepted_order_actions_per_second: f64,
    included_quote_batches: u64,
    failed_tps: f64,
    trades_per_second: f64,
    included_transfers: u64,
    included_places: u64,
    included_cancels: u64,
    included_order_transactions_by_market: BTreeMap<u64, u64>,
    per_market_outcomes: BTreeMap<u64, MarketOutcomeSummary>,
    exact_failure_reasons: BTreeMap<String, u64>,
    trace_outcome_rows: u64,
    trace_rows_expected: u64,
    included_oracle_updates_by_base_asset: BTreeMap<u64, u64>,
    observed_failure_reasons: BTreeMap<String, u64>,
    active_accounts: usize,
    open_orders_at_measure_end: usize,
    latency_p50_ms: Option<u64>,
    latency_p95_ms: Option<u64>,
    latency_sample_count: usize,
    height_min: Option<usize>,
    height_max: Option<usize>,
    view_changes: u64,
    catch_up_events: u64,
    max_backlog: usize,
    peak_rss_kib: u64,
    final_consistency: String,
    data_dir: Option<String>,
}

#[derive(Default, Debug, Serialize)]
struct MarketOutcomeSummary {
    included: u64,
    successful: u64,
    failed: u64,
    fills: u64,
}

#[derive(Serialize)]
struct TrialRecord {
    trial: Trial,
    summary: Summary,
    samples: Vec<Sample>,
    started_unix_ms: u128,
    git_head: String,
    git_diff_sha256: String,
    git_status: String,
    cpu_model: String,
    node_binary: String,
    node_binary_sha256: String,
    remote_placements: Vec<RemotePlacement>,
    submission_target: String,
    chained_hotstuff: bool,
    tx_batch_window_ms: u64,
    tx_stage_sample_mod: u64,
    verify_workers: Option<usize>,
    driver_retry_after_ms: u64,
    pinned_node_cores: Option<Vec<usize>>,
}

#[derive(Serialize)]
struct SubmissionRecord {
    digest: String,
    scheduled_unix_us: u128,
    submitted_unix_us: u128,
}

struct ClusterGuard(ControllerState);
impl Drop for ClusterGuard {
    fn drop(&mut self) {
        self.0.stop_all();
        let _ = wait_for(&self.0, Instant::now() + Duration::from_secs(8), |s| {
            !s.running && !s.launching && !s.stopping && s.metrics.alive_nodes == 0
        });
    }
}

struct PendingTx {
    transaction: Transaction,
    digest: String,
    last_sent: Instant,
    failed: bool,
    repair_attempts: u8,
    first_sent: Instant,
    sending: bool,
}

#[derive(Debug, PartialEq)]
enum MaintenanceAction {
    Repair(Transaction),
    Retry(Transaction),
}

struct ExtraDriver {
    first_submissions: HashMap<String, u128>,
    scheduled_submissions: HashMap<String, u128>,
    submissions: Option<submissions::Submissions>,
    offered: u64,
    scheduled_slots: u64,
    blocked_slots: u64,
    rejected: u64,
    rejected_reasons: BTreeMap<String, u64>,
    admission_latency_ms: Vec<u64>,
    observed_confirmation_ms: Vec<u64>,
    pending: HashMap<u64, BTreeMap<u64, PendingTx>>,
    confirmed: HashMap<u64, u64>,
    workload: Workload,
    accounts: Vec<u64>,
    driver_base: u64,
    wallet: std::collections::BTreeMap<u64, light_dex::exchange::address::Address>,
    window: usize,
    cursor: u64,
    attempted: u64,
    repair_submissions: u64,
    retry_submissions: u64,
    retry_after: Duration,
    fatal_error: Option<String>,
    schedule_origin: Instant,
    next_at: Instant,
}

fn remote_placements_arg() -> Result<Vec<RemotePlacement>, String> {
    if let Some(raw) = arg("--remote-nodes") {
        if arg("--remote-profile").is_some() {
            return Err("use --remote-nodes or --remote-profile, not both".into());
        }
        let mut seen = HashSet::new();
        return raw
            .split(',')
            .map(|entry| {
                let (profile_id, count) = entry
                    .split_once('=')
                    .ok_or("--remote-nodes requires profile=count entries")?;
                let count = count
                    .parse::<u64>()
                    .map_err(|_| "invalid remote node count")?;
                if profile_id.is_empty() || count == 0 || !seen.insert(profile_id.to_string()) {
                    return Err("remote profiles must be unique and counts positive".into());
                }
                Ok(RemotePlacement {
                    profile_id: profile_id.into(),
                    count,
                })
            })
            .collect();
    }
    Ok(arg("--remote-profile")
        .map(|profile_id| {
            vec![RemotePlacement {
                profile_id,
                count: 1,
            }]
        })
        .unwrap_or_default())
}

fn retry_after_ms() -> Result<u64, String> {
    // A remote quorum can take longer than the local retry timer to commit.
    // Retrying before that point adds duplicate network and admission work to
    // the workload we are trying to measure.
    let default = if remote_placements_arg()?.is_empty() {
        2_000
    } else {
        5_000
    };
    let requested = arg("--retry-after-ms")
        .map(|value| value.parse::<u64>().map_err(|error| error.to_string()))
        .transpose()?
        .unwrap_or(default);
    Ok(requested.clamp(200, 60_000))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = env::args().nth(1).unwrap_or_default();
    if arg("--latency-target-ms").is_some()
        && env::var("LIGHT_DEX_COMMIT_TRACE").as_deref() != Ok("1")
    {
        return Err("--latency-target-ms requires LIGHT_DEX_COMMIT_TRACE=1 so the validator records durable commit timestamps".into());
    }
    if ![
        "pilot",
        "smoke",
        "stress",
        "stress-high",
        "timeout-probe",
        "profile",
        "capacity",
    ]
    .contains(&mode.as_str())
    {
        return Err("usage: light-dex-bench pilot|smoke|stress|stress-high|timeout-probe|profile|capacity [--algorithm pbft|hotstuff] [--classic-hotstuff] [--nodes 4..25] [--participants 0..500] [--driver-accounts 0..500] [--offered-tps N] [--submission-target primary|node-0|random] [--submission-workers 1..64] [--workload mixed|transfer|resting|orderflow|multimarket|balanced-six] [--in-flight-per-account 1..32] [--retry-after-ms N] [--remote-nodes tokyo=2,ideapad=1] [--remote-profile ID] [--timeout-ms N] [--pin-node-cores 0,1,2,3] [--auto-view-change-blocks N] [--tx-batch-window-ms 0..1000] [--poll-mode all|round-robin|pinned] [--out DIR] [--measure-sec N] [--warmup-sec N]".into());
    }
    let workload = match arg("--workload").as_deref().unwrap_or("mixed") {
        "mixed" => Workload::Mixed,
        "transfer" => Workload::Transfer,
        "resting" => Workload::Resting,
        "orderflow" => Workload::Orderflow,
        "multimarket" => Workload::Multimarket,
        "balanced-six" => Workload::BalancedSix,
        _ => return Err(
            "--workload must be mixed, transfer, resting, orderflow, multimarket or balanced-six"
                .into(),
        ),
    };
    if !matches!(
        arg("--submission-target").as_deref(),
        None | Some("primary" | "node-0" | "random")
    ) {
        return Err("--submission-target must be primary, node-0, or random".into());
    }
    let window = arg("--in-flight-per-account")
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(1);
    if !(1..=32).contains(&window) {
        return Err("--in-flight-per-account must be 1..32".into());
    }
    if workload == Workload::Mixed && window != 1 {
        return Err("mixed orders require a window of 1; pipeline the transfer workload for a clean consensus stress test".into());
    }
    let participants = arg("--participants")
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(13);
    if participants > 500 {
        return Err("--participants must be 0..500".into());
    }
    let project = project_dir();
    let node_binary =
        project.join(arg("--node-binary").unwrap_or_else(|| "target/release/light_dex".into()));
    if !node_binary.is_file() {
        return Err("build the release node first: cargo build --release --bin light_dex".into());
    }
    let run_root = arg("--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| project.join("benchmark_results/runs"));
    let output = run_root.join(format!("pilot-{}", unix_ms()));
    fs::create_dir_all(&output)?;
    let smoke = mode == "smoke";
    let warmup = Duration::from_secs(
        arg("--warmup-sec")
            .and_then(|s| s.parse().ok())
            .unwrap_or(if smoke { 12 } else { 12 }),
    );
    let measure = Duration::from_secs(
        arg("--measure-sec")
            .and_then(|s| s.parse().ok())
            .unwrap_or(if smoke { 10 } else { 30 }),
    );
    let git_head = command_output(&project, "git", &["rev-parse", "HEAD"]);
    let git_diff_sha256 = diff_hash(&project);
    let git_status = command_output(&project, "git", &["status", "--porcelain"]);
    let node_binary_sha256 = file_hash(&node_binary)?;
    let cpu_model = cpu_model();
    let began = Instant::now();
    let mut summaries = Vec::<Summary>::new();
    let selected = match mode.as_str() {
        "capacity" => {
            let algorithm = match arg("--algorithm").as_deref().unwrap_or("hotstuff") {
                "pbft" => Algorithm::Pbft,
                "hotstuff" => Algorithm::Hotstuff,
                "hotstuff2" => Algorithm::Hotstuff2,
                "votor" => Algorithm::Votor,
                _ => return Err("--algorithm must be pbft, hotstuff, hotstuff2, or votor".into()),
            };
            let offered_tps = arg("--offered-tps")
                .ok_or("capacity mode requires --offered-tps")?
                .parse::<u64>()?;
            let nodes = arg("--nodes")
                .map(|value| value.parse::<u64>())
                .transpose()?
                .unwrap_or(7);
            if !(4..=25).contains(&nodes) {
                return Err("--nodes must be 4..25".into());
            }
            vec![Trial {
                name: format!(
                    "capacity_{}_{}n_{}p_{}_{}",
                    algorithm.label(),
                    nodes,
                    participants,
                    workload.label(),
                    offered_tps
                ),
                algorithm,
                nodes,
                timeout_ms: arg("--timeout-ms")
                    .map(|v| v.parse::<u64>())
                    .transpose()?
                    .unwrap_or(1500),
                offered_tps,
            }]
        }
        "stress" => trials(false)
            .into_iter()
            .filter(|trial| trial.name.starts_with("stress_"))
            .collect(),
        "stress-high" => trials(false)
            .into_iter()
            .filter(|trial| trial.name.starts_with("stress_") && trial.offered_tps >= 80)
            .collect(),
        "timeout-probe" => [50, 100, 150]
            .into_iter()
            .flat_map(|timeout_ms| {
                [Algorithm::Pbft, Algorithm::Hotstuff]
                    .into_iter()
                    .map(move |algorithm| Trial {
                        name: format!("probe_{}_{}", algorithm.label(), timeout_ms),
                        algorithm,
                        nodes: 7,
                        timeout_ms,
                        offered_tps: 0,
                    })
            })
            .collect(),
        "profile" => vec![Trial {
            name: "profile_hotstuff_7_80".into(),
            algorithm: Algorithm::Hotstuff,
            nodes: 7,
            timeout_ms: 1500,
            offered_tps: 80,
        }],
        _ => trials(smoke),
    };
    let classic_hotstuff = env::args().any(|arg| arg == "--classic-hotstuff");
    for mut trial in selected {
        let chained_hotstuff = matches!(trial.algorithm, Algorithm::Hotstuff) && !classic_hotstuff;
        if matches!(trial.algorithm, Algorithm::Hotstuff) {
            trial.name.push_str(if chained_hotstuff {
                "_chained"
            } else {
                "_classic"
            });
        }
        if !smoke && began.elapsed() > Duration::from_secs(28 * 60) {
            break;
        }
        println!("Starting {}", trial.name);
        let trial_dir = output.join(&trial.name);
        fs::create_dir_all(&trial_dir)?;
        let started_unix_ms = unix_ms();
        match run_trial(
            &trial,
            &project,
            &trial_dir,
            warmup,
            measure,
            workload,
            window,
            participants,
        ) {
            Ok((summary, samples)) => {
                println!(
                    "{}: {} {:.2} successful TPS",
                    trial.name, summary.result, summary.successful_tps
                );
                if let Some(met) = summary.latency_slo_met {
                    println!(
                        "end-to-end p95={:?} ms; {} ms latency target met={met}; matched={} unmatched={}",
                        summary.end_to_end_p95_ms,
                        summary.latency_target_ms,
                        summary.end_to_end_samples,
                        summary.end_to_end_unmatched
                    );
                } else {
                    println!(
                        "end-to-end p95={:?} ms; matched={} unmatched={}",
                        summary.end_to_end_p95_ms,
                        summary.end_to_end_samples,
                        summary.end_to_end_unmatched
                    );
                }
                let record = TrialRecord {
                    trial,
                    summary,
                    samples,
                    started_unix_ms,
                    git_head: git_head.clone(),
                    git_diff_sha256: git_diff_sha256.clone(),
                    git_status: git_status.clone(),
                    cpu_model: cpu_model.clone(),
                    node_binary: node_binary.display().to_string(),
                    node_binary_sha256: node_binary_sha256.clone(),
                    remote_placements: remote_placements_arg()?,
                    submission_target: arg("--submission-target")
                        .unwrap_or_else(|| "primary".into()),
                    chained_hotstuff,
                    tx_batch_window_ms: arg("--tx-batch-window-ms")
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(50),
                    tx_stage_sample_mod: env::var("LIGHT_DEX_TX_STAGE_SAMPLE_MOD")
                        .ok()
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(0),
                    verify_workers: env::var("LIGHT_DEX_VERIFY_WORKERS")
                        .ok()
                        .and_then(|value| value.parse().ok()),
                    driver_retry_after_ms: retry_after_ms()?,
                    pinned_node_cores: arg("--pin-node-cores")
                        .map(|value| {
                            value
                                .split(',')
                                .map(str::parse)
                                .collect::<Result<Vec<usize>, _>>()
                        })
                        .transpose()?,
                };
                fs::write(
                    trial_dir.join("run.json"),
                    serde_json::to_vec_pretty(&record)?,
                )?;
                summaries.push(record.summary);
            }
            Err(error) => {
                eprintln!("{} failed: {error}", trial.name);
                fs::write(trial_dir.join("error.txt"), error.as_bytes())?;
                summaries.push(failed_summary(
                    &trial,
                    error.clone(),
                    workload,
                    window,
                    participants,
                ));
                if error.contains("feed_fresh=false") {
                    break;
                }
            }
        }
        fs::write(
            output.join("summary.json"),
            serde_json::to_vec_pretty(&summaries)?,
        )?;
    }
    fs::write(
        output.join("summary.json"),
        serde_json::to_vec_pretty(&summaries)?,
    )?;
    println!("Results: {}", output.display());
    Ok(())
}

#[cfg(test)]
#[path = "light_dex_bench/tests.rs"]
mod tests;
