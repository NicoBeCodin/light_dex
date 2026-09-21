use node::Node;
use rand::Rng;
use std::collections::HashMap;
use std::time::Duration;
use std::{env, thread};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::*;

use crate::blockchain::{BlockchainState, Transaction};
use crate::consensus::pbft::Pbft;
use crate::message::Message;
use crate::network::Network;
use crate::node::{ControlMessage, NodeId};

pub mod blockchain;
pub mod consensus;
pub mod message;
pub mod network;
pub mod node;

fn send_message_to_node(addr: SocketAddr, message: Message) -> std::io::Result<()> {
    let mut stream = TcpStream::connect(addr)?;

    let mut serialized = message.to_str()?;
    serialized.push('\n');

    stream.write_all(serialized.as_bytes())?;

    Ok(())
}

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

    let mut rng = rand::rng();

    let mut transaction_counter = 0u64;

    let mut alive = vec![true; children.len()];

    let max_faults = children.len().saturating_sub(1) / 3;

    let mut killed = 0usize;

    loop {
        thread::sleep(Duration::from_millis(250));

        let alive_nodes: Vec<usize> = alive
            .iter()
            .enumerate()
            .filter_map(
                |(index, is_alive)| {
                    if *is_alive {
                        Some(index)
                    } else {
                        None
                    }
                },
            )
            .collect();

        if alive_nodes.is_empty() {
            break;
        }

        // ----------------------
        // Send random transaction
        // ----------------------

        let target_index = rng.random_range(0..alive_nodes.len());

        let target_node = alive_nodes[target_index];

        let transaction = Transaction::new_with(format!(
            "TX {} sent to node {}",
            transaction_counter, target_node
        ));

        transaction_counter += 1;

        if let Some(addr) = nodes.get(&(target_node as u32)) {
            let _ = send_message_to_node(*addr, Message::Transaction(transaction));
        }

        // ----------------------
        // Occasionally fault node
        // ----------------------

        if transaction_counter % 30 == 0 {
            let victim_index = rng.random_range(0..alive_nodes.len());

            let victim = alive_nodes[victim_index];

            if rng.random_bool(0.8) {
                // Temporary pause.

                println!("SIMULATION: pausing node {}", victim);

                if let Some(addr) = nodes.get(&(victim as u32)) {
                    let _ = send_message_to_node(
                        *addr,
                        Message::Control(ControlMessage::Pause { millis: 5000 }),
                    );
                }
            } else if killed < max_faults {
                // Permanent crash.

                println!("SIMULATION: killing node {}", victim);

                match children[victim].kill() {
                    Ok(_) => {
                        alive[victim] = false;
                        killed += 1;
                    }

                    Err(error) => {
                        eprintln!("Couldn't kill node {}: {}", victim, error);
                    }
                }
            }
        }
    }

    for child in &mut children {
        child.wait().expect("Failed waiting for child");
    }

    return;
}
