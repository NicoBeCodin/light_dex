#[path = "../../control-plane/src-tauri/src/types.rs"]
mod types;

use light_dex::{blockchain::Transaction, node::NodeStatus};
use std::path::PathBuf;
use ts_rs::{Config, TS};
use types::{
    AgentConfig, ControlSnapshot, LaunchConfig, LoadConfig, MarketMakerConfig, RemoteChoice,
    RemotePlacement, TargetNode,
};

fn main() {
    let output = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("control-plane/src/bindings");
    if output.exists() {
        std::fs::remove_dir_all(&output).expect("remove old generated bindings");
    }
    std::fs::create_dir_all(&output).expect("create generated bindings directory");
    let config = Config::from_env().with_out_dir(output.clone());
    LaunchConfig::export_all(&config).expect("export LaunchConfig");
    RemotePlacement::export_all(&config).expect("export RemotePlacement");
    RemoteChoice::export_all(&config).expect("export RemoteChoice");
    LoadConfig::export_all(&config).expect("export LoadConfig");
    MarketMakerConfig::export_all(&config).expect("export MarketMakerConfig");
    AgentConfig::export_all(&config).expect("export AgentConfig");
    TargetNode::export_all(&config).expect("export TargetNode");
    ControlSnapshot::export_all(&config).expect("export ControlSnapshot");
    Transaction::export_all(&config).expect("export Transaction");
    NodeStatus::export_all(&config).expect("export NodeStatus");
    // ts-rs can leave a space before a wrapped newline in large struct types.
    // Normalize generated files so binding regeneration stays diff-clean.
    for entry in std::fs::read_dir(output).expect("list generated bindings") {
        let path = entry.expect("binding entry").path();
        if path.extension().is_some_and(|extension| extension == "ts") {
            let source = std::fs::read_to_string(&path).expect("read generated binding");
            let normalized = source
                .lines()
                .map(str::trim_end)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            if normalized != source {
                std::fs::write(&path, normalized).expect("normalize generated binding");
            }
        }
    }
}
