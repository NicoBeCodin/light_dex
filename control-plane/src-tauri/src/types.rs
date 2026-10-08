use light_dex::node::{AgentBalance, NodeId, NodeStatus};
use light_dex::exchange::address::Address;
use light_dex::exchange::order::Side;
use light_dex::participants::strategies::AgentStrategy;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct LaunchConfig {
    pub project_dir: String,
    pub binary_path: String,
    pub data_dir: String,
    pub recover_existing: bool,
    pub node_count: u64,
    pub block_time_ms: u64,
    pub timeout_ms: u64,
    pub poll_ms: u64,
    pub poll_mode: PollMode,
    pub build_before_launch: bool,
    pub consensus: ConsensusAlgorithm,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum PollMode {
    All,
    RoundRobin,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct AgentConfig {
    pub agent_id: u64,
    pub account_id: u64,
    pub market_id: u64,
    pub strategy: AgentStrategy,
    pub target: TargetNode,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct AgentSnapshot {
    pub agent_id: u64,
    pub account_id: u64,
    pub strategy: AgentStrategy,
    pub target: TargetNode,
    pub state: String,
    pub pending: usize,
    pub pending_action: Option<String>,
    pub own_open_orders: usize,
    pub last_error: Option<String>,
    pub confirmed_nonce: Option<u64>,
    pub balance: Option<AgentBalance>,
    pub feed_age_ms: Option<u64>,
    pub group_id: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ConsensusAlgorithm { Pbft, Hotstuff }

impl ConsensusAlgorithm {
    #[allow(dead_code)] // also included by the standalone ts-rs export binary
    pub fn as_str(self) -> &'static str {
        match self { Self::Pbft => "pbft", Self::Hotstuff => "hotstuff" }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetNode {
    Random,
    Primary,
    Node { node_id: NodeId },
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct LoadConfig {
    pub preset: LoadPreset,
    pub target: TargetNode,
    pub tps: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoadPreset {
    RoundRobinTransfers {
        first_account: u64,
        account_count: u64,
        asset: u64,
        amount: u64,
    },
    RandomTransfers {
        first_account: u64,
        account_count: u64,
        asset: u64,
        amount: u64,
    },
    BookTaker {
        account_id: u64,
        market_id: u64,
        side: Side,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct LoadSnapshot {
    pub running: bool,
    pub kind: Option<String>,
    pub tps: Option<f64>,
    pub pending_accounts: usize,
    pub failed_accounts: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct MarketMakerConfig {
    pub account_id: u64,
    pub levels: usize,
    pub refresh_ms: u64,
    pub target: TargetNode,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct MarketMakerSnapshot {
    pub running: bool,
    pub stopping: bool,
    pub account_id: Option<u64>,
    pub refresh_ms: Option<u64>,
    pub last_update_id: Option<u64>,
    pub feed_age_ms: Option<u64>,
    pub desired_bids: Vec<u64>,
    pub desired_asks: Vec<u64>,
    pub own_open_orders: usize,
    pub pending: bool,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct NodeRuntimeSnapshot {
    pub node_id: NodeId,
    pub pid: u32,
    pub state: String,
    pub address: Option<String>,
    pub tx_rate: f64,
    pub block_rate: f64,
    pub status: Option<NodeStatus>,
    pub last_status_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct MetricsSnapshot {
    pub alive_nodes: usize,
    pub responsive_nodes: usize,
    pub submitted: u64,
    pub submit_tps: f64,
    pub commit_tps: f64,
    pub blocks_per_second: f64,
    pub commit_latency_ms: f64,
    pub height_min: Option<usize>,
    pub height_max: Option<usize>,
    pub views: Vec<u64>,
    pub primaries: Vec<NodeId>,
    pub chain_consistency: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct WalletAccount {
    pub label: u64,
    pub address: Address,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ControlSnapshot {
    pub running: bool,
    pub launching: bool,
    pub stopping: bool,
    pub nodes: Vec<NodeRuntimeSnapshot>,
    pub metrics: MetricsSnapshot,
    pub load: LoadSnapshot,
    pub market_maker: MarketMakerSnapshot,
    pub agents: Vec<AgentSnapshot>,
    pub wallet: Vec<WalletAccount>,
    pub logs: Vec<String>,
    pub active_data_dir: Option<String>,
}
