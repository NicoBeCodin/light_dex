//! Value-keyed memoization: public mutable blocks cannot safely hold an unchecked
//! OnceLock digest. A hash selects a slot; full equality is REQUIRED for a hit.
//! Collisions therefore only cost performance, never change a protocol digest.
use super::{Block, Digest, Transaction};
use std::{
    cell::RefCell,
    hash::{DefaultHasher, Hash, Hasher},
};
struct Memo<T> {
    slots: Vec<Option<(T, Digest)>>,
}
impl<T: Hash + Eq + Clone> Memo<T> {
    fn new(size: usize) -> Self {
        Self {
            slots: vec![None; size],
        }
    }
    fn get_or_compute(&mut self, value: &T, compute: impl FnOnce() -> Digest) -> Digest {
        let mut hash = DefaultHasher::new();
        value.hash(&mut hash);
        let index = hash.finish() as usize % self.slots.len();
        if let Some((key, digest)) = &self.slots[index] {
            if key == value {
                return *digest;
            }
        }
        let digest = compute();
        self.slots[index] = Some((value.clone(), digest));
        digest
    }
}
thread_local! {
    static TRANSACTIONS: RefCell<Memo<Transaction>> = RefCell::new(Memo::new(8192));
    static BLOCKS: RefCell<Memo<Block>> = RefCell::new(Memo::new(32));
}
pub(super) fn transaction(value: &Transaction) -> Digest {
    if value.validate_shape().is_err() {
        return value.digest_uncached();
    }
    TRANSACTIONS.with(|cache| {
        cache
            .borrow_mut()
            .get_or_compute(value, || value.digest_uncached())
    })
}
pub(super) fn block(value: &Block) -> Digest {
    // Keep the memory bound meaningful even for externally supplied blocks.
    if value.transactions.len() > 4096
        || value
            .transactions
            .iter()
            .any(|tx| tx.validate_shape().is_err())
        || value.oracle_updates.len() > 32
        || value.authentication.as_ref().is_some_and(|a| {
            a.chain_id.len() > 64
                || a.signatures.len() > 4096
                || a.signatures.iter().any(|s| s.len() > 128)
        })
        || value.oracle_updates.iter().any(|u| {
            u.observations.len() > 3
                || u.observations.iter().any(|o| {
                    o.symbol.len() > 64
                        || o.bid.len() > 32
                        || o.ask.len() > 32
                        || o.source_time.as_ref().is_some_and(|s| s.len() > 64)
                })
        })
    {
        return value.digest_uncached();
    }
    BLOCKS.with(|cache| {
        cache
            .borrow_mut()
            .get_or_compute(value, || value.digest_uncached())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mutated_values_cannot_reuse_digests() {
        let mut block = Block::new_initial_block();
        let original = block.to_digest();
        assert_eq!(original, block.digest_uncached());
        block.sequence += 1;
        assert_ne!(original, block.to_digest());
        assert_eq!(block.to_digest(), block.digest_uncached());
        let tx = Transaction::ClaimAirdrop {
            account_id: 1.into(),
            nonce: 0,
        };
        let changed = tx.clone().with_nonce(1);
        assert_eq!(tx.to_digest(), tx.digest_uncached());
        assert_ne!(tx.to_digest(), changed.to_digest());
        let mut one_slot = Memo::new(1);
        assert_eq!(
            one_slot.get_or_compute(&tx, || tx.digest_uncached()),
            tx.to_digest()
        );
        assert_eq!(
            one_slot.get_or_compute(&changed, || changed.digest_uncached()),
            changed.to_digest()
        );
    }
}
