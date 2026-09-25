use std::{env, net::SocketAddr};
use light_dex::{
    blockchain::BlockchainState,
    consensus::pbft::{Consensus, Pbft},
    network::Network,
    node::{Node, NodeId},
};

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let node_id: NodeId = arg_value(&args, "--node")
        .expect("missing --node")
        .parse()
        .expect("invalid --node");

    let parent_addr: SocketAddr = arg_value(&args, "--parent")
        .expect("missing --parent")
        .parse()
        .expect("invalid --parent");

    let block_time_ms: u64 = arg_value(&args, "--block-time-ms")
        .unwrap_or_else(|| "500".to_string())
        .parse()
        .expect("invalid --block-time-ms");

    let timeout_ms: u64 = arg_value(&args, "--timeout-ms")
        .unwrap_or_else(|| "1500".to_string())
        .parse()
        .expect("invalid --timeout-ms");

    let pid = std::process::id();

    let network =
        Network::new(node_id, parent_addr).expect("failed creating network");

    let state = BlockchainState::new();

    let mut consensus = Pbft::new(node_id, 1);
    consensus.set_timeout(timeout_ms);

    let mut node = Node::new(
        node_id,
        pid,
        network,
        state,
        consensus,
        block_time_ms,
    )
    .expect("failed creating node");

    println!("Initialized node {node_id} pid={pid}");

    node.register_with_parent()
        .expect("failed registering with controller");

    node.discover_peers()
        .expect("failed discovering peers");

    node.wait_for_start()
        .expect("failed waiting for START");

    node.run().expect("node failed");
}
