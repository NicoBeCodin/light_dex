use super::{AccountId, Digest, MarketId, OrderId};
use crate::oracle::OraclePair;
use sha2::{Digest as _, Sha256};
use std::sync::OnceLock;
mod batch;

type StateRoot = [u8; 32];
#[derive(Debug, Clone)]
enum PendingValue {
    Bytes(Arc<[u8]>),
    Account(Arc<super::Account>),
}
thread_local! {
    static ACCOUNT_ENCODING_SCRATCH: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(Vec::new());
}
impl PendingValue {
    fn hash(&self) -> Digest {
        match self {
            Self::Bytes(bytes) => hash_value(bytes),
            Self::Account(account) => ACCOUNT_ENCODING_SCRATCH.with(|scratch| {
                let mut bytes = scratch.borrow_mut();
                super::canonical::encode_account_into(account, &mut bytes);
                hash_value(&bytes)
            }),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum StateKey {
    Account(AccountId),
    Order(OrderId),
    Market(MarketId),
    PerpAccount(MarketId, AccountId),
    NextOrderId,
    FaucetClaims,
    OracleMark(OraclePair),
    MarginMode(AccountId),
    CrossCollateral(AccountId),
    InsuranceAsset(u64),
    BadDebtUsd,
    FundingClock(MarketId),
}

use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub(super) struct StateTree {
    root: Option<Arc<Node>>,
    // Speculative block execution coalesces writes by key. Persistent maps keep
    // transaction rollback/forks cheap even while a block has dirty leaves.
    batching: bool,
    pending: im::HashMap<StateKey, Option<PendingValue>>,
    materialized: OnceLock<Option<Arc<Node>>>,
}

#[derive(Debug)]
enum Node {
    Leaf {
        key_hash: Digest,
        value_hash: Digest,
        hash: OnceLock<Digest>,
    },
    Branch {
        split_bit: u16,
        min_key: Digest,
        left: Arc<Node>,
        right: Arc<Node>,
        hash: OnceLock<Digest>,
    },
}

impl Node {
    fn hash(&self) -> Digest {
        match self {
            Node::Leaf {
                key_hash,
                value_hash,
                hash,
            } => *hash
                .get_or_init(|| hash_parts(&[b"light-dex/state/leaf/v1", key_hash, value_hash])),
            Node::Branch {
                split_bit,
                left,
                right,
                hash,
                ..
            } => *hash.get_or_init(|| {
                hash_parts(&[
                    b"light-dex/state/branch/v1",
                    &split_bit.to_be_bytes(),
                    &left.hash(),
                    &right.hash(),
                ])
            }),
        }
    }
    fn min_key(&self) -> Digest {
        match self {
            Node::Leaf { key_hash, .. } => *key_hash,
            Node::Branch { min_key, .. } => *min_key,
        }
    }
}

impl StateTree {
    pub(super) fn new() -> Self {
        Self::default()
    }
    pub(super) fn root(&self) -> StateRoot {
        static EMPTY_ROOT: OnceLock<Digest> = OnceLock::new();
        self.current_root()
            .as_ref()
            .map(|node| node.hash())
            .unwrap_or_else(|| {
                *EMPTY_ROOT.get_or_init(|| hash_parts(&[b"light-dex/state/empty/v1"]))
            })
    }

    fn current_root(&self) -> &Option<Arc<Node>> {
        if self.pending.is_empty() {
            return &self.root;
        }
        self.materialized.get_or_init(|| {
            let started = std::time::Instant::now();
            if self.pending.len() < 64 {
                let mut root = self.root.clone();
                for (key, value) in &self.pending {
                    let key = hash_key(*key);
                    root = match (root, value) {
                        (Some(node), Some(value)) => Some(insert(&node, key, value.hash())),
                        (None, Some(value)) => Some(leaf(key, value.hash())),
                        (Some(node), None) => remove(&node, &key).0,
                        (None, None) => None,
                    };
                }
                crate::profile::record(
                    "root_small_apply",
                    started.elapsed(),
                    self.pending.len() as u64,
                );
                return root;
            }
            let mut changes: Vec<_> = self
                .pending
                .iter()
                .map(|(key, value)| (hash_key(*key), value.as_ref().map(PendingValue::hash)))
                .collect();
            crate::profile::record(
                "root_encode_values",
                started.elapsed(),
                changes.len() as u64,
            );
            let sort_started = std::time::Instant::now();
            changes.sort_unstable_by_key(|(key, _)| *key);
            crate::profile::record("root_sort", sort_started.elapsed(), changes.len() as u64);
            let apply_started = std::time::Instant::now();
            let root = batch::apply(self.root.as_ref(), &changes);
            crate::profile::record("root_apply", apply_started.elapsed(), changes.len() as u64);
            root
        })
    }

    pub(super) fn begin_batch(&mut self) {
        self.batching = true;
    }
    pub(super) fn is_batching(&self) -> bool {
        self.batching
    }

    pub(super) fn finish_batch(&mut self) {
        self.root = self.current_root().clone();
        let hash_started = std::time::Instant::now();
        if let Some(root) = &self.root {
            root.hash();
        }
        crate::profile::record(
            "root_hash",
            hash_started.elapsed(),
            self.pending.len() as u64,
        );
        self.pending.clear();
        self.materialized.take();
        self.batching = false;
    }

    #[cfg(test)]
    pub(super) fn get_value_hash(&self, key: StateKey) -> Option<Digest> {
        let key_hash = hash_key(key);
        let mut node = self.current_root().as_deref()?;
        loop {
            match node {
                Node::Leaf {
                    key_hash: stored_key,
                    value_hash,
                    ..
                } => {
                    return (*stored_key == key_hash).then_some(*value_hash);
                }
                Node::Branch {
                    split_bit,
                    left: left_node,
                    right: right_node,
                    ..
                } => {
                    node = if bit_at(&key_hash, *split_bit) == 0 {
                        left_node
                    } else {
                        right_node
                    };
                }
            }
        }
    }
    pub(super) fn put(&mut self, key: StateKey, value: &[u8]) {
        if self.batching {
            self.pending
                .insert(key, Some(PendingValue::Bytes(Arc::from(value))));
            self.materialized.take();
            return;
        }
        let key_hash = hash_key(key);
        let value_hash = hash_value(value);
        self.root = Some(match &self.root {
            Some(root) => insert(root, key_hash, value_hash),
            None => leaf(key_hash, value_hash),
        });
        // Outside a block batch preserve the original eager-update contract:
        // state_digest() must not unexpectedly pay for a cold genesis tree.
        self.root.as_ref().unwrap().hash();
    }

    /// Encode only the final value in a block, not each intermediate balance.
    /// Accounts contain persistent balance maps, so staging this version does
    /// not copy all assets. Intermediate root reads remain exact.
    pub(super) fn put_account(&mut self, account: &super::Account) {
        if self.batching {
            self.pending.insert(
                StateKey::Account(account.id),
                Some(PendingValue::Account(Arc::new(account.clone()))),
            );
            self.materialized.take();
        } else {
            self.put(
                StateKey::Account(account.id),
                &super::canonical::encode_account(account),
            );
        }
    }

    pub(super) fn remove(&mut self, key: StateKey) -> bool {
        if self.batching {
            let existed = match self.pending.get(&key) {
                Some(value) => value.is_some(),
                None => contains(self.root.as_deref(), &hash_key(key)),
            };
            self.pending.insert(key, None);
            self.materialized.take();
            return existed;
        }
        let Some(root) = &self.root else {
            return false;
        };
        let (next, removed) = remove(root, &hash_key(key));
        self.root = next;
        if let Some(root) = &self.root {
            root.hash();
        }
        removed
    }
}

fn contains(mut node: Option<&Node>, key: &Digest) -> bool {
    while let Some(current) = node {
        match current {
            Node::Leaf { key_hash, .. } => return key_hash == key,
            Node::Branch {
                split_bit,
                left,
                right,
                ..
            } => {
                node = Some(if bit_at(key, *split_bit) == 0 {
                    left
                } else {
                    right
                });
            }
        }
    }
    false
}

fn leaf(key_hash: Digest, value_hash: Digest) -> Arc<Node> {
    Arc::new(Node::Leaf {
        key_hash,
        value_hash,
        hash: OnceLock::new(),
    })
}

fn branch(split_bit: u16, left: Arc<Node>, right: Arc<Node>) -> Arc<Node> {
    let min_key = left.min_key();
    Arc::new(Node::Branch {
        split_bit,
        min_key,
        left,
        right,
        hash: OnceLock::new(),
    })
}

fn first_differing_bit(a: &Digest, b: &Digest) -> Option<u16> {
    a.iter().zip(b).enumerate().find_map(|(index, (&x, &y))| {
        let difference = x ^ y;
        (difference != 0).then(|| (index * 8 + difference.leading_zeros() as usize) as u16)
    })
}

fn join(new_leaf: Arc<Node>, existing: Arc<Node>, split_bit: u16) -> Arc<Node> {
    if bit_at(&new_leaf.min_key(), split_bit) == 0 {
        branch(split_bit, new_leaf, existing)
    } else {
        branch(split_bit, existing, new_leaf)
    }
}

fn insert(node: &Arc<Node>, key_hash: Digest, value_hash: Digest) -> Arc<Node> {
    match node.as_ref() {
        Node::Leaf {
            key_hash: existing_key,
            value_hash: existing_value,
            ..
        } => {
            if *existing_key == key_hash {
                if *existing_value == value_hash {
                    Arc::clone(node)
                } else {
                    leaf(key_hash, value_hash)
                }
            } else {
                let split_bit = first_differing_bit(existing_key, &key_hash)
                    .expect("Different keys must differ in a bit");
                join(leaf(key_hash, value_hash), Arc::clone(node), split_bit)
            }
        }
        Node::Branch {
            split_bit,
            min_key,
            left: left_node,
            right: right_node,
            ..
        } => {
            if let Some(difference) = first_differing_bit(min_key, &key_hash) {
                if difference < *split_bit {
                    return join(leaf(key_hash, value_hash), Arc::clone(node), difference);
                }
            }
            if bit_at(&key_hash, *split_bit) == 0 {
                let replacement = insert(left_node, key_hash, value_hash);
                if Arc::ptr_eq(left_node, &replacement) {
                    Arc::clone(node)
                } else {
                    branch(*split_bit, replacement, Arc::clone(right_node))
                }
            } else {
                let replacement = insert(right_node, key_hash, value_hash);
                if Arc::ptr_eq(right_node, &replacement) {
                    Arc::clone(node)
                } else {
                    branch(*split_bit, Arc::clone(left_node), replacement)
                }
            }
        }
    }
}

fn remove(node: &Arc<Node>, key_hash: &Digest) -> (Option<Arc<Node>>, bool) {
    match node.as_ref() {
        Node::Leaf {
            key_hash: existing_key,
            ..
        } => {
            if existing_key == key_hash {
                (None, true)
            } else {
                (Some(Arc::clone(node)), false)
            }
        }

        Node::Branch {
            split_bit,
            left,
            right,
            ..
        } => {
            if bit_at(key_hash, *split_bit) == 0 {
                let (replacement, removed) = remove(left, key_hash);

                if !removed {
                    return (Some(Arc::clone(node)), false);
                }

                // If the left subtree disappeared, this branch now
                // has only one child and must collapse.
                let next = match replacement {
                    Some(left) => branch(*split_bit, left, Arc::clone(right)),
                    None => Arc::clone(right),
                };

                (Some(next), true)
            } else {
                let (replacement, removed) = remove(right, key_hash);

                if !removed {
                    return (Some(Arc::clone(node)), false);
                }

                let next = match replacement {
                    Some(right) => branch(*split_bit, Arc::clone(left), right),
                    None => Arc::clone(left),
                };

                (Some(next), true)
            }
        }
    }
}

fn bit_at(hash: &Digest, index: u16) -> u8 {
    debug_assert!(index < 256);
    (hash[(index / 8) as usize] >> (7 - (index % 8))) & 1
}

fn hash_parts(parts: &[&[u8]]) -> Digest {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn hash_key(key: StateKey) -> Digest {
    const DOMAIN: &[u8] = b"light-dex/state/key/v2";

    match key {
        StateKey::Account(id) => hash_parts(&[DOMAIN, &[0], &id.bytes()]),
        StateKey::Order(id) => hash_parts(&[DOMAIN, &[1], &id.to_be_bytes()]),
        StateKey::Market(id) => hash_parts(&[DOMAIN, &[2], &id.to_be_bytes()]),
        StateKey::NextOrderId => hash_parts(&[DOMAIN, &[3]]),
        StateKey::FaucetClaims => hash_parts(&[DOMAIN, &[4]]),
        StateKey::PerpAccount(market, account) => {
            hash_parts(&[DOMAIN, &[5], &market.to_be_bytes(), &account.bytes()])
        }
        StateKey::OracleMark(pair) => hash_parts(&[
            DOMAIN,
            &[6],
            &pair.base_asset.to_be_bytes(),
            &pair.quote_asset.to_be_bytes(),
        ]),
        StateKey::MarginMode(account) => hash_parts(&[DOMAIN, &[7], &account.bytes()]),
        StateKey::CrossCollateral(account) => hash_parts(&[DOMAIN, &[8], &account.bytes()]),
        StateKey::InsuranceAsset(asset) => hash_parts(&[DOMAIN, &[9], &asset.to_be_bytes()]),
        StateKey::BadDebtUsd => hash_parts(&[DOMAIN, &[10]]),
        StateKey::FundingClock(market) => hash_parts(&[DOMAIN, &[11], &market.to_be_bytes()]),
    }
}

fn hash_value(value: &[u8]) -> Digest {
    hash_parts(&[
        b"light-dex/state/value/v1",
        &(value.len() as u64).to_be_bytes(),
        value,
    ])
}

#[cfg(test)]
mod batching_tests {
    use super::*;
    #[test]
    fn coalesced_updates_match_eager_roots_and_forks_are_isolated() {
        let mut eager = StateTree::new();
        let mut batched = StateTree::new();
        let mut random = 42u64;
        for round in 0..20 {
            batched.begin_batch();
            let parent = batched.clone();
            let parent_root = parent.root();
            for step in 0..200u64 {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                let key = StateKey::Order((random >> 32) % 80);
                if random % 4 == 0 {
                    assert_eq!(eager.remove(key), batched.remove(key));
                } else {
                    let value = (random ^ step).to_be_bytes();
                    eager.put(key, &value);
                    batched.put(key, &value);
                }
                if step % 29 == 0 {
                    assert_eq!(eager.root(), batched.root(), "round {round} step {step}");
                    assert_eq!(eager.get_value_hash(key), batched.get_value_hash(key));
                }
            }
            batched.finish_batch();
            assert_eq!(eager.root(), batched.root());
            assert_eq!(parent_root, parent.root());
        }
    }
    #[test]
    fn remove_then_reinsert_and_discarded_dirty_fork() {
        let mut tree = StateTree::new();
        tree.put(StateKey::NextOrderId, b"old");
        tree.begin_batch();
        tree.put(StateKey::NextOrderId, b"new");
        let snapshot = tree.clone();
        assert!(tree.remove(StateKey::NextOrderId));
        assert!(!tree.remove(StateKey::NextOrderId));
        assert_ne!(tree.root(), snapshot.root());
        tree.put(StateKey::NextOrderId, b"new");
        tree.finish_batch();
        assert_eq!(tree.root(), snapshot.root());
    }
}
