#[path = "../../control-plane/src-tauri/src/types.rs"]
mod types;

use light_dex::{blockchain::Transaction, node::NodeStatus};
use std::path::PathBuf;
use ts_rs::{Config, TS};
use types::{ControlSnapshot, LaunchConfig, LoadConfig, MarketMakerConfig, TargetNode};

fn main() {
    let output = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("control-plane/src/bindings");
    if output.exists() {
        std::fs::remove_dir_all(&output).expect("remove old generated bindings");
    }
    std::fs::create_dir_all(&output).expect("create generated bindings directory");
    let config = Config::from_env().with_out_dir(output);
    LaunchConfig::export_all(&config).expect("export LaunchConfig");
    LoadConfig::export_all(&config).expect("export LoadConfig");
    MarketMakerConfig::export_all(&config).expect("export MarketMakerConfig");
    TargetNode::export_all(&config).expect("export TargetNode");
    ControlSnapshot::export_all(&config).expect("export ControlSnapshot");
    Transaction::export_all(&config).expect("export Transaction");
    NodeStatus::export_all(&config).expect("export NodeStatus");
}
