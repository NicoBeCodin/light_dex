//! Replay every signed validator journal in a stopped benchmark session.
use light_dex::{blockchain::BlockchainState, crypto::SessionManifest};
use std::{path::PathBuf, sync::Arc};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("usage: audit_journals DATA_DIR NODE_COUNT")?,
    );
    let count: u64 = std::env::args()
        .nth(2)
        .ok_or("missing NODE_COUNT")?
        .parse()?;
    let manifest = Arc::new(SessionManifest::load(&directory.join("manifest.json"))?);
    let mut chains = Vec::new();
    for id in 0..count {
        let state = BlockchainState::load_signed(
            directory.join(format!("node-{id}.json")),
            manifest.clone(),
        )
        .map_err(|error| format!("node {id} replay failed: {error}"))?;
        let digests = state
            .blocks
            .iter()
            .map(|block| block.to_digest())
            .collect::<Vec<_>>();
        let root = state.exchange_state.state_digest();
        println!(
            "node={id} height={} root={}",
            digests.len(),
            light_dex::message::hex_encode(&root)
        );
        chains.push((id, digests, root));
    }
    let min_height = chains
        .iter()
        .map(|(_, blocks, _)| blocks.len())
        .min()
        .unwrap_or(0);
    let max_height = chains
        .iter()
        .map(|(_, blocks, _)| blocks.len())
        .max()
        .unwrap_or(0);
    if max_height - min_height > 2 {
        return Err("height skew exceeds two blocks".into());
    }
    for sequence in 0..min_height {
        let expected = chains[0].1[sequence];
        if let Some((id, _, _)) = chains
            .iter()
            .find(|(_, blocks, _)| blocks[sequence] != expected)
        {
            return Err(format!("conflicting block at sequence {sequence} on node {id}").into());
        }
    }
    if min_height == max_height && chains.iter().any(|(_, _, root)| *root != chains[0].2) {
        return Err("exchange roots differ at the same height".into());
    }
    println!("AUDIT_OK common_height={min_height} max_height={max_height}");
    Ok(())
}
