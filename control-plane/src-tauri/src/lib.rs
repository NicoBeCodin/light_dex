pub mod controller;
mod binance_depth;
pub mod types;

use controller::ControllerState;
use light_dex::blockchain::Transaction;
use light_dex::exchange::address::Address;
use light_dex::exchange::order::OrderType;
use light_dex::participants::strategies::AgentStrategy;
use std::path::PathBuf;
use tauri::State;
use types::{AgentConfig, ConsensusAlgorithm, ControlSnapshot, LaunchConfig, LoadConfig, MarketMakerConfig, NodeRuntimeSnapshot, PollMode, TargetNode, WalletAccount};

#[tauri::command]
fn default_config() -> LaunchConfig {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let project = manifest
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| manifest.join("../.."));
    LaunchConfig {
        project_dir: project.display().to_string(),
        binary_path: "target/release/light_dex".into(),
        data_dir: "benchmark_results/control-plane-state".into(),
        recover_existing: false,
        node_count: 7,
        block_time_ms: 500,
        timeout_ms: 1500,
        poll_ms: 200,
        poll_mode: PollMode::RoundRobin,
        build_before_launch: true,
        consensus: ConsensusAlgorithm::Pbft,
        claim_limit: 1_000,
        claim_usd: 1_000_000,
        claim_btc: 1_000,
    }
}

#[tauri::command]
fn launch(config: LaunchConfig, state: State<'_, ControllerState>) -> Result<(), String> {
    state.launch(config)
}
#[tauri::command]
fn stop_all(state: State<'_, ControllerState>) {
    state.stop_all();
}
#[tauri::command]
fn submit_transaction(
    transaction: Transaction,
    target: TargetNode,
    state: State<'_, ControllerState>,
) -> Result<(), String> {
    state.submit_transaction(transaction, target)
}
#[tauri::command]
fn create_address(state: State<'_, ControllerState>) -> Result<WalletAccount, String> {
    state.create_address()
}
#[tauri::command]
async fn claim_new_accounts(count: u64, state: State<'_, ControllerState>) -> Result<Vec<WalletAccount>, String> {
    let controller = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || controller.claim_new_accounts(count))
        .await.map_err(|error| error.to_string())?
}
#[tauri::command]
fn airdrop_new_address(from_label: u64, asset: u64, amount: u64, target: TargetNode, state: State<'_, ControllerState>) -> Result<WalletAccount, String> {
    state.airdrop_new_address(from_label, asset, amount, target)
}
#[tauri::command]
async fn fund_address(from_label: u64, to: Address, asset: u64, amount: u64, target: TargetNode, state: State<'_, ControllerState>) -> Result<(), String> {
    let controller = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || controller.fund_address(from_label, to, asset, amount, target))
        .await.map_err(|error| error.to_string())?
}
#[tauri::command]
fn pause_node(
    target: TargetNode,
    millis: u64,
    state: State<'_, ControllerState>,
) -> Result<(), String> {
    state.pause_node(target, millis)
}
#[tauri::command]
fn kill_node(target: TargetNode, state: State<'_, ControllerState>) -> Result<(), String> {
    state.kill_node(target)
}
#[tauri::command]
fn start_load(config: LoadConfig, state: State<'_, ControllerState>) -> Result<(), String> {
    state.start_load(config)
}
#[tauri::command]
fn stop_load(state: State<'_, ControllerState>) {
    state.stop_load();
}
#[tauri::command]
fn start_market_maker(config: MarketMakerConfig, state: State<'_, ControllerState>) -> Result<(), String> {
    state.start_market_maker(config)
}
#[tauri::command]
fn stop_market_maker(state: State<'_, ControllerState>) {
    state.stop_market_maker();
}

#[tauri::command]
fn start_agent(config: AgentConfig, state: State<'_, ControllerState>) -> Result<(), String> {
    state.start_agent(config)
}

#[tauri::command]
fn start_agents(configs: Vec<AgentConfig>, state: State<'_, ControllerState>) -> Result<u64, String> {
    state.start_agents(configs)
}

#[tauri::command]
fn default_participant_fleet() -> Vec<AgentConfig> { participant_fleet_template() }

pub fn participant_fleet_template() -> Vec<AgentConfig> {
    let mut configs = vec![
        AgentConfig { agent_id: 1, account_id: 70, market_id: 0, target: TargetNode::Primary,
            strategy: AgentStrategy::BinanceMirror { levels: 3, refresh_ms: 1000 } },
        AgentConfig { agent_id: 2, account_id: 71, market_id: 0, target: TargetNode::Primary,
            strategy: AgentStrategy::AvellanedaStoikov { gamma: 0.0005, k: 0.02, horizon_seconds: 30.0,
                volatility_window_seconds: 60, target_inventory_btc: 1000, max_inventory_deviation_btc: 5, refresh_ms: 1000 } },
        AgentConfig { agent_id: 3, account_id: 72, market_id: 0, target: TargetNode::Primary,
            strategy: AgentStrategy::AvellanedaStoikov { gamma: 0.0015, k: 0.02, horizon_seconds: 30.0,
                volatility_window_seconds: 60, target_inventory_btc: 1000, max_inventory_deviation_btc: 5, refresh_ms: 1000 } },
    ];
    for (index, (order_type, tps, offset)) in [
        (OrderType::IOC, 1.0, 10), (OrderType::IOC, 1.0, 10),
        (OrderType::LO, 0.5, 10), (OrderType::LO, 0.5, 25),
        (OrderType::FOK, 0.5, 10),
    ].into_iter().enumerate() {
        configs.push(AgentConfig { agent_id: 4 + index as u64, account_id: 73 + index as u64,
            market_id: 0, target: TargetNode::Random,
            strategy: AgentStrategy::RandomOrder { order_type, tps, passive_offset_ticks: offset,
                max_resting_orders: 2, resting_ttl_ms: 5000 } });
    }
    for (index, (lookback_updates, threshold_ticks)) in [(3, 1), (5, 2), (8, 3), (13, 5), (21, 8)].into_iter().enumerate() {
        configs.push(AgentConfig { agent_id: 9 + index as u64, account_id: 78 + index as u64,
            market_id: 0, target: TargetNode::Random,
            strategy: AgentStrategy::TrendFollower { lookback_updates, threshold_ticks, tps: 0.5 } });
    }
    configs
}

#[tauri::command]
fn stop_agent(agent_id: u64, state: State<'_, ControllerState>) -> Result<(), String> {
    state.stop_agent(agent_id)
}
#[tauri::command]
fn stop_agents(agent_ids: Vec<u64>, state: State<'_, ControllerState>) -> Result<(), String> {
    state.stop_agents(agent_ids)
}
#[tauri::command]
fn get_snapshot(state: State<'_, ControllerState>) -> ControlSnapshot {
    state.snapshot()
}

#[tauri::command]
async fn refresh_node_status(node_id: u64, state: State<'_, ControllerState>) -> Result<NodeRuntimeSnapshot, String> {
    let controller = (*state).clone();
    tauri::async_runtime::spawn_blocking(move || controller.refresh_node_status(node_id))
        .await.map_err(|error| error.to_string())?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(ControllerState::default())
        .invoke_handler(tauri::generate_handler![
            default_config,
            launch,
            stop_all,
            submit_transaction,
            create_address,
            claim_new_accounts,
            airdrop_new_address,
            fund_address,
            pause_node,
            kill_node,
            start_load,
            stop_load,
            start_market_maker,
            stop_market_maker,
            start_agent,
            start_agents,
            default_participant_fleet,
            stop_agent,
            stop_agents,
            get_snapshot,
            refresh_node_status
        ])
        .run(tauri::generate_context!())
        .expect("error running control plane");
}

#[cfg(test)]
mod tests {
    use super::*;
    use light_dex::node::NodeStatus;
    use std::collections::HashSet;
    use ts_rs::{Config, TS};

    #[test]
    fn mixed_fleet_has_thirteen_distinct_agents_and_accounts() {
        let fleet = default_participant_fleet();
        assert_eq!(fleet.len(), 13);
        assert_eq!(fleet.iter().map(|agent| agent.agent_id).collect::<HashSet<_>>().len(), 13);
        assert_eq!(fleet.iter().map(|agent| agent.account_id).collect::<HashSet<_>>().len(), 13);
        assert_eq!(fleet.iter().filter(|agent| agent.strategy.is_maker()).count(), 3);
        assert_eq!(fleet.iter().filter(|agent| matches!(agent.strategy, AgentStrategy::RandomOrder { .. })).count(), 5);
        assert_eq!(fleet.iter().filter(|agent| matches!(agent.strategy, AgentStrategy::TrendFollower { .. })).count(), 5);
        for agent in fleet { agent.strategy.validate().unwrap(); }
    }

    #[test]
    fn export_bindings() {
        let output = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../src/bindings");
        if output.exists() {
            std::fs::remove_dir_all(&output).unwrap();
        }
        std::fs::create_dir_all(&output).unwrap();
        let config = Config::from_env().with_out_dir(output);
        LaunchConfig::export_all(&config).unwrap();
        LoadConfig::export_all(&config).unwrap();
        MarketMakerConfig::export_all(&config).unwrap();
        AgentConfig::export_all(&config).unwrap();
        TargetNode::export_all(&config).unwrap();
        ControlSnapshot::export_all(&config).unwrap();
        WalletAccount::export_all(&config).unwrap();
        Transaction::export_all(&config).unwrap();
        NodeStatus::export_all(&config).unwrap();
    }
}
