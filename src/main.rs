use node::Node;
use std::collections::HashMap;
use std::env;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::*;

use crate::blockchain::BlockchainState;
use crate::consensus::pbft::Pbft;
use crate::network::Network;
use crate::node::NodeId;

pub mod blockchain;
pub mod consensus;
pub mod message;
pub mod network;
pub mod node;

fn main() {
    let args: Vec<String> = env::args().collect();

    let pid = std::process::id();
    let n;
    if args.contains(&"--child".to_string()) {
        let node_id: NodeId = args[2].parse().expect("Failed getting count");
        let parent_addr: SocketAddr = args[3].parse().expect("Failed getting count");
        let pid = std::process::id();

        let network = Network::new(node_id, parent_addr).expect("Failed creating network");
        let blockchain_state = BlockchainState::new();
        let consensus = Pbft::new(node_id, 1);

        let mut node = Node::new(node_id, pid, network, blockchain_state, consensus)
            .expect("Failed initializeing node");
        println!("Initialized node: {node:?}");
        node.register_with_parent().unwrap();
        node.discover_peers().unwrap();
        node.wait_for_start().unwrap();
        node.run().unwrap();

        //Finished
        return;
    } else {
        println!("Parent PID: {pid}");
        n = args[1].parse().expect("Expected number");
    }

    let listener = TcpListener::bind("127.0.0.1:0").expect("Failed binding on localhost");
    let parent_addr = listener.local_addr().expect("Failed getting local address");
    let executable = env::current_exe().expect("Couldn't locate current executable");

    //Spawn child(s)
    let mut children = Vec::new();
    for i in 0..n {
        let child = Command::new(&executable)
            .arg("--child")
            .arg(i.to_string())
            .arg(parent_addr.to_string())
            .spawn()
            .expect("Failed launching child process");
        children.push(child);
    }

    //Connect to each child and get their SocketAddr
    let mut nodes: HashMap<u32, SocketAddr> = HashMap::new();
    for _ in &children {
        let (stream, _) = listener.accept().expect("Failed listening");
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("Failed reading line");
        let parts: Vec<&str> = line.trim().split_whitespace().collect();
        if parts[0] == "REGISTER" {
            let node_id: u32 = parts[1].parse().expect("Failed getting node id");
            let node_address: SocketAddr = parts[2].parse().expect("Failed getting node address");
            nodes.insert(node_id, node_address);
            println!("")
        }
    }

    //make all nodes know about each other
    for (node_id, node_address) in &nodes {
        let mut stream =
            TcpStream::connect(node_address).expect("Failed connecting to node address");
        for (target_node_id, target_node_adress) in &nodes {
            if node_id == target_node_id {
                continue;
            }
            let message = format!("REGISTER {} {}\n", target_node_id, target_node_adress);
            let _ = stream.write_all(message.as_bytes());
            // println!("Sent {message} to  {target_node_id}");
        }
        let done_message = format!("DONE\n");
        let _ = stream.write_all(done_message.as_bytes());
    }

    let mut ready_streams = Vec::new();

    for _ in 0..nodes.len() {
        let (stream, _) = listener.accept().unwrap();

        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();

        reader.read_line(&mut line).unwrap();

        let parts: Vec<&str> = line.split_whitespace().collect();

        if parts.first() == Some(&"READY") {
            println!("Node {} is ready", parts[1]);

            ready_streams.push(stream);
        }
    }

    for mut stream in ready_streams {
        stream.write_all(b"START\n").unwrap();
    }

    for child in &mut children {
        child.wait().expect("Failed waiting for child");
    }

    return;
}
