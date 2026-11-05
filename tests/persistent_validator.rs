//! Opt-in process regression for fixed-peer startup and same-identity restart.
use light_dex::{
    blockchain::BlockchainState,
    crypto::{FaucetTerms, SessionManifest, create_session_with_market_ids},
};
use std::{
    fs,
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
struct Cluster {
    directory: PathBuf,
    children: Vec<Option<Child>>,
    addresses: Vec<String>,
}
impl Drop for Cluster {
    fn drop(&mut self) {
        for child in &mut self.children {
            if let Some(child) = child {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}
impl Cluster {
    fn start(&mut self, id: usize) {
        let data = self.directory.join(format!("data-{id}"));
        fs::create_dir_all(&data).unwrap();
        let output = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(data.join("process.log"))
            .unwrap();
        self.children[id] = Some(
            Command::new(env!("CARGO_BIN_EXE_light_dex"))
                .args([
                    "--node",
                    &id.to_string(),
                    "--consensus",
                    "hotstuff2",
                    "--recover-hotstuff2",
                    "--listen",
                    &self.addresses[id],
                    "--block-time-ms",
                    "100",
                    "--timeout-ms",
                    "1000",
                ])
                .arg("--peers-file")
                .arg(self.directory.join("peers.json"))
                .arg("--chain-config")
                .arg(self.directory.join("session/manifest.json"))
                .arg("--validator-key")
                .arg(
                    self.directory
                        .join(format!("session/keys/validator-{id}.key")),
                )
                .arg("--data-dir")
                .arg(&data)
                .env("LIGHT_DEX_AUTH_KEY", "07".repeat(32))
                .env("LIGHT_DEX_FAST_CERT_HANDOFF", "1")
                .env("LIGHT_DEX_COMPACT_PROPOSALS", "1")
                .env("LIGHT_DEX_CHECKPOINT_INTERVAL", "2")
                .env("LIGHT_DEX_RETAINED_BLOCKS", "4")
                .stdout(Stdio::from(output.try_clone().unwrap()))
                .stderr(Stdio::from(output))
                .spawn()
                .unwrap(),
        );
    }
    fn stop(&mut self, id: usize) {
        let mut child = self.children[id].take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }
    fn wait_committed(&mut self, minimum: usize) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let mut ready = true;
            for id in 0..4 {
                let log = fs::read_to_string(self.directory.join(format!("data-{id}/process.log")))
                    .unwrap_or_default();
                assert!(
                    self.children[id]
                        .as_mut()
                        .unwrap()
                        .try_wait()
                        .unwrap()
                        .is_none(),
                    "validator {id} exited: {log}"
                );
                let endpoint = log
                    .lines()
                    .filter_map(|line| line.strip_prefix("READ_API "))
                    .last();
                ready &= endpoint
                    .and_then(read_height)
                    .is_some_and(|height| height >= minimum);
                assert!(
                    Instant::now() < deadline,
                    "validator {id} did not commit: {log}"
                );
            }
            if ready {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}
#[test]
#[ignore = "spawns four validator processes; run explicitly"]
fn fixed_peer_validator_restarts_without_creating_a_new_chain() {
    let directory =
        std::env::temp_dir().join(format!("ldex-process-recovery-{}", rand::random::<u64>()));
    fs::create_dir(&directory).unwrap();
    let listeners = (0..4)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect::<Vec<_>>();
    let addresses = listeners
        .iter()
        .map(|l| l.local_addr().unwrap().to_string())
        .collect::<Vec<_>>();
    let peers = addresses
        .iter()
        .enumerate()
        .map(|(id, a)| (id.to_string(), a.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    fs::write(
        directory.join("peers.json"),
        serde_json::to_vec(&peers).unwrap(),
    )
    .unwrap();
    drop(listeners);
    let manifest = Arc::new(
        create_session_with_market_ids(
            &directory.join("session"),
            "hotstuff2",
            4,
            FaucetTerms::default(),
            Some(4),
            vec![0, 1, 2, 3, 8, 9],
        )
        .unwrap(),
    );
    let mut cluster = Cluster {
        directory,
        children: (0..4).map(|_| None).collect(),
        addresses,
    };
    cluster.start(0);
    cluster.start(1);
    std::thread::sleep(Duration::from_secs(2));
    cluster.start(2);
    cluster.start(3);
    cluster.wait_committed(1);
    submit_claim(&cluster.addresses[0], &manifest);
    cluster.wait_committed(2);
    cluster.stop(0);
    cluster.start(0);
    std::thread::sleep(Duration::from_secs(2));
    cluster.wait_committed(2);
    for id in 0..4 {
        cluster.stop(id);
    }
    let first = BlockchainState::load_signed(
        cluster.directory.join("data-0/node-0.json"),
        manifest.clone(),
    )
    .unwrap();
    assert_eq!(first.blocks[0].sequence, 0);
    drop(first);
    for id in 0..4 {
        cluster.start(id);
    }
    std::thread::sleep(Duration::from_secs(2));
    submit_claim(&cluster.addresses[0], &manifest);
    cluster.wait_committed(3);
    // Force disk compaction and require the offline validator to recover from a
    // certified snapshot, rather than assuming all history remains in RAM.
    cluster.stop(0);
    for minimum in 4..=12 {
        submit_claim(&cluster.addresses[1], &manifest);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let log = fs::read_to_string(cluster.directory.join("data-1/process.log")).unwrap();
            if log.contains(&format!("committed seq {} ", minimum - 1)) {
                break;
            }
            assert!(Instant::now() < deadline, "online quorum stopped: {log}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    cluster.start(0);
    cluster.wait_committed(12);
    std::thread::sleep(Duration::from_secs(1));
    let mut roots = Vec::new();
    for id in 0..4 {
        cluster.stop(id);
        let path = cluster.directory.join(format!("data-{id}/node-{id}.json"));
        let state = BlockchainState::load_signed(path.clone(), manifest.clone()).unwrap();
        assert!(state.base_height > 0);
        assert_eq!(state.height(), 12);
        roots.push(state.exchange_state.state_digest());
        drop(state);
        // An interrupted newest snapshot must fall back to the previous one
        // and replay the retained journal suffix without losing commits.
        if id == 1 {
            fs::write(path.with_extension("checkpoint"), b"broken snapshot").unwrap();
            let fallback = BlockchainState::load_signed(path, manifest.clone()).unwrap();
            assert_eq!(fallback.height(), 12);
            assert_eq!(fallback.exchange_state.state_digest(), roots[0]);
        }
    }
    assert!(roots.windows(2).all(|r| r[0] == r[1]));
    assert_eq!(
        manifest.chain_id,
        SessionManifest::load(&cluster.directory.join("session/manifest.json"))
            .unwrap()
            .chain_id
    );
}

fn submit_claim(address: &str, manifest: &SessionManifest) {
    use std::io::Write;
    let key = light_dex::crypto::new_key().unwrap();
    let transaction = light_dex::blockchain::Transaction::ClaimAirdrop {
        account_id: light_dex::exchange::address::Address::from_public_key(&key.verifying_key()),
        nonce: 0,
    };
    let signed = light_dex::crypto::sign_transaction(transaction, manifest, &key).unwrap();
    let line = light_dex::message::sign_line(
        &light_dex::message::Message::SignedTransaction(signed),
        &[7; 32],
    )
    .unwrap();
    std::net::TcpStream::connect(address)
        .unwrap()
        .write_all(line.as_bytes())
        .unwrap();
}

fn read_height(endpoint: &str) -> Option<usize> {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(endpoint).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    write!(stream,"GET /v1/node/summary HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n","07".repeat(32)).ok()?;
    let mut text = String::new();
    stream.read_to_string(&mut text).ok()?;
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    serde_json::from_str::<serde_json::Value>(&text[start..=end]).ok()?["height"]
        .as_u64()
        .map(|h| h as usize)
}
