use light_dex::node::{NodeId, NodeStatus};
use light_dex::exchange::order::Side;
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
    pub build_before_launch: bool,
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
pub struct ControlSnapshot {
    pub running: bool,
    pub launching: bool,
    pub stopping: bool,
    pub nodes: Vec<NodeRuntimeSnapshot>,
    pub metrics: MetricsSnapshot,
    pub load: LoadSnapshot,
    pub market_maker: MarketMakerSnapshot,
    pub logs: Vec<String>,
    pub active_data_dir: Option<String>,
}
