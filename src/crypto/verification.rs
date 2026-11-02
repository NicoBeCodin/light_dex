//! A small process-wide pool, not a pool per connection or block. Only immutable
//! cryptographic checks run here; state-dependent admission stays on the owner.
use super::*;
use std::sync::{Arc, Mutex, OnceLock, mpsc};

// A proposal is authenticated on the network thread before it reaches the
// state owner. Rechecking every transaction during state validation used to
// take the verification-cache lock once per signature. This bounded cache
// remembers only a *successfully verified complete block* for this chain.
// The block digest commits to the signed transaction bytes and signatures;
// the manifest chain ID is part of the key as an additional domain boundary.
type VerifiedBlock = ([u8; 32], crate::blockchain::Digest);
fn verified_blocks() -> &'static Mutex<super::cache::Cache<VerifiedBlock, ()>> {
    static BLOCKS: OnceLock<Mutex<super::cache::Cache<VerifiedBlock, ()>>> = OnceLock::new();
    BLOCKS.get_or_init(|| Mutex::new(super::cache::Cache::new(256)))
}
fn block_cache_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("LIGHT_DEX_BLOCK_VERIFY_CACHE").as_deref() != Ok("0"))
}

type Job = Box<dyn FnOnce() + Send + 'static>;
fn pool() -> &'static mpsc::SyncSender<Job> {
    static POOL: OnceLock<mpsc::SyncSender<Job>> = OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<Job>(256);
        let rx = Arc::new(Mutex::new(rx));
        let workers = std::env::var("LIGHT_DEX_VERIFY_WORKERS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(2)
            .clamp(1, 8);
        for index in 0..workers {
            let rx = rx.clone();
            std::thread::Builder::new()
                .name(format!("verify-{index}"))
                .spawn(move || {
                    loop {
                        let job = rx.lock().unwrap().recv();
                        let Ok(job) = job else {
                            break;
                        };
                        job();
                    }
                })
                .expect("verification worker");
        }
        tx
    })
}

/// Returns the job on saturation so the caller can reject rather than create
/// an unbounded queue. Jobs must not synchronously wait on this same pool.
pub(crate) fn try_verify_job(job: Job) -> Result<(), mpsc::TrySendError<Job>> {
    pool().try_send(job)
}

pub(crate) fn verify_block_transactions(
    block: &Block,
    manifest: &SessionManifest,
) -> io::Result<()> {
    if block.transactions.len() > crate::blockchain::BlockchainState::MAX_PROPOSAL_TRANSACTIONS {
        return Err(invalid("transaction count limit"));
    }
    let auth = block
        .authentication
        .as_ref()
        .ok_or_else(|| invalid("unsigned block"))?;
    if auth.signatures.len() != block.transactions.len() {
        return Err(invalid("signature count mismatch"));
    }
    if auth.chain_id != manifest.chain_id {
        return Err(invalid("block chain ID mismatch"));
    }
    let key = block_cache_enabled()
        .then(|| {
            manifest
                .chain_id_bytes()
                .map(|chain| (chain, block.to_digest()))
        })
        .transpose()?;
    if key.is_some_and(|key| verified_blocks().lock().unwrap().get(&key).is_some()) {
        crate::profile::record("auth_block_cache_hit", std::time::Duration::ZERO, 1);
        return Ok(());
    }
    if block.transactions.len() < 32 {
        for (tx, signature) in block.transactions.iter().zip(&auth.signatures) {
            verify_transaction(
                &SignedTransaction {
                    transaction: tx.clone(),
                    signature: signature.clone(),
                },
                manifest,
            )?;
        }
        if let Some(key) = key {
            verified_blocks().lock().unwrap().insert(key, ());
            crate::profile::record("auth_block_cache_miss", std::time::Duration::ZERO, 1);
        }
        return Ok(());
    }
    let (tx, rx) = mpsc::channel();
    let manifest = Arc::new(manifest.clone());
    let mut count = 0;
    for (transactions, signatures) in block
        .transactions
        .chunks(32)
        .zip(auth.signatures.chunks(32))
    {
        let items: Vec<_> = transactions
            .iter()
            .cloned()
            .zip(signatures.iter().cloned())
            .map(|(transaction, signature)| SignedTransaction {
                transaction,
                signature,
            })
            .collect();
        let manifest = manifest.clone();
        let tx = tx.clone();
        let job: Job = Box::new(move || {
            let result = items
                .iter()
                .try_for_each(|signed| verify_transaction(signed, &manifest));
            let _ = tx.send(result);
        });
        // Under saturation the caller performs bounded work instead of blocking
        // another worker behind a full queue or bypassing authentication.
        if let Err(error) = try_verify_job(job) {
            match error {
                mpsc::TrySendError::Full(job) | mpsc::TrySendError::Disconnected(job) => job(),
            }
        }
        count += 1;
    }
    for _ in 0..count {
        rx.recv()
            .map_err(|_| invalid("verification worker closed"))??;
    }
    if let Some(key) = key {
        verified_blocks().lock().unwrap().insert(key, ());
        crate::profile::record("auth_block_cache_miss", std::time::Duration::ZERO, 1);
    }
    Ok(())
}
