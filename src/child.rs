use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
};

//Where we write the child process
#[derive(Debug)]
pub struct Node {
    node_id: u32,
    pid: u32,
    parent_address: SocketAddr,
    listener: TcpListener,
    peers: HashMap<u32, SocketAddr>,
}

impl Node {
    pub fn init(
        node_id: u32,
        pid: u32,
        parent_address: SocketAddr,
    ) -> Result<Node, std::io::Error> {
        let listener = TcpListener::bind("127.0.0.1:0").expect("Failed binding on localhost");
        Ok(Node {
            node_id,
            pid,
            parent_address,
            listener,
            peers: HashMap::new(),
        })
    }
    //After initializeation for communicating with parent
    pub fn register_with_parent(&self) -> Result<(), std::io::Error> {
        let mut stream =
            TcpStream::connect(self.parent_address).expect("Failed connecting to parent adress");
        let message = format!(
            "REGISTER {} {} \n",
            self.node_id,
            self.listener.local_addr()?
        );
        stream.write_all(message.as_bytes());
        Ok(())
    }

    pub fn discover_peers(&mut self) -> Result<(), std::io::Error> {
        let (stream, _) = self.listener.accept().expect("Failed accepting");
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            reader.read_line(&mut line);
            line.trim();
            let parts: Vec<&str> = line.split_whitespace().collect();
            match parts.first() {
                Some(&"REGISTER") => {
                    let node_id: u32 = parts[1].parse().expect("Failed getting node_id");
                    if node_id == self.node_id {
                        continue;
                    }
                    let node_address: SocketAddr =
                        parts[2].parse().expect("Failed getting node_address");

                    self.peers.insert(node_id, node_address);
                }

                Some(&"DONE") => break,
                _ => {}
            }
        }
        let n_peers = self.peers.len();
        println!("Discovered {n_peers} peers");
        Ok(())
    }
}
