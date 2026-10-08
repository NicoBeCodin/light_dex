//! Opt-in Ed25519 protocol overhead measurement; key generation is untimed.
use std::{env, fs, path::PathBuf, time::Instant};

use light_dex::{
    blockchain::Transaction,
    consensus::{
        ConsensusMessage,
        hotstuff::{HotStuffMessage, Phase, QuorumCertificate},
        quorum_size,
    },
    crypto::{
        create_session, read_secret, sign_consensus, sign_transaction, verify_hotstuff_qc,
        verify_transaction,
    },
    exchange::address::Address,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let samples: usize = env::args().nth(1).as_deref().unwrap_or("1000").parse()?;
    if samples == 0 {
        return Err("samples must be positive".into());
    }
    println!("validators,samples,transaction_sign_us,transaction_verify_us,commit_qc_verify_us");
    for nodes in [4, 7, 13, 20] {
        let dir = env::temp_dir().join(format!(
            "light-dex-signature-bench-{}",
            rand::random::<u128>()
        ));
        fs::create_dir(&dir)?;
        let run = measure(&dir, nodes, samples);
        fs::remove_dir_all(&dir)?;
        let (sign, verify, qc) = run?;
        println!("{nodes},{samples},{sign:.3},{verify:.3},{qc:.3}");
    }
    Ok(())
}

fn measure(
    dir: &PathBuf,
    nodes: u64,
    samples: usize,
) -> Result<(f64, f64, f64), Box<dyn std::error::Error>> {
    let manifest = create_session(dir, "hotstuff", nodes)?;
    let account_key = read_secret(&dir.join("keys/account-0.key"))?;
    let transaction = Transaction::Transfer {
        from: Address::from_public_key(&account_key.verifying_key()),
        to: Address::from_public_key(&read_secret(&dir.join("keys/account-1.key"))?.verifying_key()),
        asset: 0,
        amount: 1,
        nonce: 0,
    };
    let started = Instant::now();
    for _ in 0..samples {
        std::hint::black_box(sign_transaction(
            transaction.clone(),
            &manifest,
            &account_key,
        )?);
    }
    let sign = started.elapsed().as_secs_f64() * 1_000_000.0 / samples as f64;
    let signed = sign_transaction(transaction, &manifest, &account_key)?;
    let started = Instant::now();
    for _ in 0..samples {
        std::hint::black_box(verify_transaction(&signed, &manifest)?);
    }
    let verify = started.elapsed().as_secs_f64() * 1_000_000.0 / samples as f64;

    let digest = [7; 32];
    let mut qc = QuorumCertificate {
        view: 0,
        height: 1,
        phase: Phase::Commit,
        digest,
        voters: Vec::new(),
        signatures: Vec::new(),
    };
    for id in 0..quorum_size(nodes) {
        let key = read_secret(&dir.join(format!("keys/validator-{id}.key")))?;
        let vote = ConsensusMessage::Hotstuff(HotStuffMessage::Vote {
            view: 0,
            height: 1,
            phase: Phase::Commit,
            digest,
        });
        qc.voters.push(id);
        qc.signatures
            .push(sign_consensus(vote, id, &manifest, &key)?.signature);
    }
    let started = Instant::now();
    for _ in 0..samples {
        std::hint::black_box(verify_hotstuff_qc(&qc, &manifest)?);
    }
    let qc_verify = started.elapsed().as_secs_f64() * 1_000_000.0 / samples as f64;
    Ok((sign, verify, qc_verify))
}
