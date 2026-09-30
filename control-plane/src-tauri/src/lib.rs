mod controller;
mod binance_depth;
mod types;

use controller::ControllerState;
use light_dex::blockchain::Transaction;
use std::path::PathBuf;
use tauri::State;
use types::{ControlSnapshot, LaunchConfig, LoadConfig, MarketMakerConfig, TargetNode};

#[tauri::command]
fn default_config() -> LaunchConfig {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let project = manifest
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| manifest.join("../.."));
    LaunchConfig {
        project_dir: project.display().to_string(),
        binary_path: "target/debug/light_dex".into(),
        data_dir: "benchmark_results/control-plane-state".into(),
        recover_existing: false,
        node_count: 7,
        block_time_ms: 500,
        timeout_ms: 1500,
        poll_ms: 500,
        build_before_launch: true,
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
fn get_snapshot(state: State<'_, ControllerState>) -> ControlSnapshot {
    state.snapshot()
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
            pause_node,
            kill_node,
            start_load,
            stop_load,
            start_market_maker,
            stop_market_maker,
            get_snapshot
        ])
        .run(tauri::generate_context!())
        .expect("error running control plane");
}

#[cfg(test)]
mod tests {
    use super::*;
    use light_dex::node::NodeStatus;
    use ts_rs::{Config, TS};

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
        TargetNode::export_all(&config).unwrap();
        ControlSnapshot::export_all(&config).unwrap();
        Transaction::export_all(&config).unwrap();
        NodeStatus::export_all(&config).unwrap();
    }
}
