//! Bounded, process-local acceleration only: never persisted or trusted on replay.
use super::*;
use std::{
    collections::{HashMap, VecDeque},
    hash::Hash,
    sync::{Arc, Mutex, OnceLock},
};

pub(super) struct Cache<K, V> {
    entries: HashMap<K, V>,
    fifo: VecDeque<K>,
    capacity: usize,
}
impl<K: Eq + Hash + Clone, V> Cache<K, V> {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            fifo: VecDeque::new(),
            capacity,
        }
    }
    pub(super) fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key)
    }
    pub(super) fn insert(&mut self, key: K, value: V) {
        if self.entries.contains_key(&key) {
            return;
        }
        if self.entries.len() == self.capacity {
            if let Some(old) = self.fifo.pop_front() {
                self.entries.remove(&old);
            }
        }
        self.fifo.push_back(key.clone());
        self.entries.insert(key, value);
    }
}

pub(crate) fn decoded_public_key(bytes: [u8; 32]) -> io::Result<VerifyingKey> {
    static KEYS: OnceLock<Mutex<Cache<[u8; 32], VerifyingKey>>> = OnceLock::new();
    let cache = KEYS.get_or_init(|| Mutex::new(Cache::new(8192)));
    if let Some(key) = cache.lock().unwrap().get(&bytes).copied() {
        return Ok(key);
    }
    let key = VerifyingKey::from_bytes(&bytes).map_err(io::Error::other)?;
    if key.is_weak() {
        return Err(invalid("weak public key"));
    }
    cache.lock().unwrap().insert(bytes, key);
    Ok(key)
}

type TxBytes = (Transaction, Arc<Vec<u8>>);
pub(super) fn transaction_bytes(tx: &Transaction, chain: [u8; 32]) -> io::Result<Arc<Vec<u8>>> {
    static BYTES: OnceLock<Mutex<Cache<([u8; 32], [u8; 32]), TxBytes>>> = OnceLock::new();
    let cache = BYTES.get_or_init(|| Mutex::new(Cache::new(32768)));
    let key = (chain, tx.to_digest());
    if let Some((cached, bytes)) = cache.lock().unwrap().get(&key) {
        // Do not let a transaction digest encoding bug alias signing payloads.
        if cached == tx {
            return Ok(bytes.clone());
        }
    }
    let bytes = Arc::new(signing_bytes(TX_DOMAIN, chain, tx)?);
    cache
        .lock()
        .unwrap()
        .insert(key, (tx.clone(), bytes.clone()));
    Ok(bytes)
}

/// Exact signature + key + digest of the domain-separated canonical message.
/// Successful strict verification only. Hashing the entire message prevents
/// altered votes, transactions or chain IDs from reusing a previous success.
pub(super) fn verify_cached(
    key: &VerifyingKey,
    bytes: &[u8],
    signature: &[u8; 64],
) -> io::Result<()> {
    type Verification = ([u8; 32], [u8; 32], [u8; 64]);
    static VERIFIED: OnceLock<Mutex<Cache<Verification, ()>>> = OnceLock::new();
    let cache = VERIFIED.get_or_init(|| Mutex::new(Cache::new(65536)));
    let identity = (key.to_bytes(), Sha256::digest(bytes).into(), *signature);
    if cache.lock().unwrap().get(&identity).is_some() {
        return Ok(());
    }
    key.verify_strict(bytes, &Signature::from_bytes(signature))
        .map_err(|_| invalid("invalid signature"))?;
    cache.lock().unwrap().insert(identity, ());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_signature_cache_does_not_authorize_mutated_payloads() {
        let key = SigningKey::from_bytes(&[17; 32]);
        let bytes = b"domain/chain/message";
        let sig = key.sign(bytes).to_bytes();
        verify_cached(&key.verifying_key(), bytes, &sig).unwrap();
        verify_cached(&key.verifying_key(), bytes, &sig).unwrap();
        assert!(verify_cached(&key.verifying_key(), b"domain/other-chain/message", &sig).is_err());
        assert!(
            verify_cached(
                &SigningKey::from_bytes(&[18; 32]).verifying_key(),
                bytes,
                &sig
            )
            .is_err()
        );
        let mut altered = sig;
        altered[0] ^= 1;
        assert!(verify_cached(&key.verifying_key(), bytes, &altered).is_err());
        let hex = hex_encode(&sig);
        assert!(verify(&key.verifying_key(), bytes, &hex.to_uppercase()).is_err());
        assert!(hex_decode::<2>("a€").is_err()); // UTF-8 must not panic at a byte boundary.
    }
    #[test]
    fn cache_capacity_is_bounded() {
        let mut cache = Cache::new(2);
        for i in 0..100 {
            cache.insert(i, i);
        }
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.fifo.len(), 2);
        assert!(cache.get(&97).is_none());
        assert_eq!(cache.get(&99), Some(&99));
    }
    #[test]
    fn cached_transaction_bytes_match_canonical_encoding() {
        let tx = Transaction::ClaimAirdrop {
            account_id: 1.into(),
            nonce: 0,
        };
        for chain in [[1; 32], [2; 32]] {
            let expected = signing_bytes(TX_DOMAIN, chain, &tx).unwrap();
            assert_eq!(transaction_bytes(&tx, chain).unwrap().as_ref(), &expected);
            assert_eq!(transaction_bytes(&tx, chain).unwrap().as_ref(), &expected);
        }
    }
    #[test]
    #[ignore = "release-mode timing probe; no network or filesystem"]
    fn signature_cost_probe() {
        use std::{hint::black_box, time::Instant};
        let key = SigningKey::from_bytes(&[19; 32]);
        let tx = Transaction::Transfer {
            from: Address::from_public_key(&key.verifying_key()),
            to: 1.into(),
            asset: 0,
            amount: 1,
            nonce: 0,
        };
        let bytes = transaction_bytes(&tx, [1; 32]).unwrap();
        let signature = key.sign(&bytes).to_bytes();
        let hex = hex_encode(&signature);
        let parsed = Signature::from_bytes(&signature);
        for cached in [false, true] {
            let start = Instant::now();
            for _ in 0..10000 {
                if cached {
                    verify(&key.verifying_key(), black_box(&bytes), black_box(&hex)).unwrap();
                } else {
                    key.verifying_key()
                        .verify_strict(black_box(&bytes), black_box(&parsed))
                        .unwrap();
                }
            }
            println!(
                "VERIFY_COST cached={cached} mean_ns={}",
                start.elapsed().as_nanos() / 10000
            );
        }
    }
}
