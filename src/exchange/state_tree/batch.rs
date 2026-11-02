//! Merge a sorted write set into the compressed binary trie. Unlike repeated
//! insert/remove, a shared ancestor is reconstructed at most once per batch.
//! Hash domains, split bits, leaf encoding and canonical roots are unchanged.
use super::*;

pub(super) fn apply(
    old: Option<&Arc<Node>>,
    changes: &[(Digest, Option<Digest>)],
) -> Option<Arc<Node>> {
    if changes.is_empty() {
        return old.cloned();
    }
    let first = changes.first().unwrap().0;
    let last = changes.last().unwrap().0;
    let mut split = first_differing_bit(&first, &last).unwrap_or(256);
    if let Some(node) = old {
        split = split.min(first_differing_bit(&node.min_key(), &first).unwrap_or(256));
        if let Node::Branch { split_bit, .. } = node.as_ref() {
            split = split.min(*split_bit);
        }
    }
    if split == 256 {
        debug_assert_eq!(changes.len(), 1);
        return changes[0].1.map(|value| match old {
            Some(node) => insert(node, first, value),
            None => leaf(first, value),
        });
    }
    let middle = changes.partition_point(|(key, _)| bit_at(key, split) == 0);
    let (old_left, old_right) = match old.map(Arc::as_ref) {
        Some(Node::Branch {
            split_bit,
            left,
            right,
            ..
        }) if *split_bit == split => (Some(left), Some(right)),
        Some(node) if bit_at(&node.min_key(), split) == 0 => (old, None),
        Some(_) => (None, old),
        None => (None, None),
    };
    let left = apply(old_left, &changes[..middle]);
    let right = apply(old_right, &changes[middle..]);
    match (left, right) {
        (Some(left), Some(right)) => {
            if let Some(Node::Branch {
                split_bit,
                left: a,
                right: b,
                ..
            }) = old.map(Arc::as_ref)
            {
                if *split_bit == split && Arc::ptr_eq(a, &left) && Arc::ptr_eq(b, &right) {
                    return old.cloned();
                }
            }
            Some(branch(split, left, right))
        }
        (left, right) => left.or(right),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    #[test]
    #[ignore = "release timing probe, no nodes or external feeds"]
    fn bulk_update_cost_probe() {
        use rand::seq::SliceRandom;
        use std::{hint::black_box, time::Instant};
        let mut root = None;
        for id in 0..10000 {
            let key = hash_key(StateKey::Order(id));
            root = Some(match root {
                Some(node) => insert(&node, key, [1; 32]),
                None => leaf(key, [1; 32]),
            });
        }
        root.as_ref().unwrap().hash();
        println!("method,dirty_keys,p50_ns,p95_ns");
        for count in [16, 32, 64, 512, 4096] {
            let mut changes: Vec<_> = (0..count)
                .map(|id| {
                    let target = if id % 2 == 0 { id } else { 10000 + id };
                    (
                        hash_key(StateKey::Order(target)),
                        (id % 5 != 0).then_some([2; 32]),
                    )
                })
                .collect();
            changes.shuffle(&mut StdRng::seed_from_u64(123));
            let mut expected = None;
            for bulk in [false, true] {
                let mut times = Vec::new();
                for sample in 0..220 {
                    let mut changes = changes.clone();
                    let start = Instant::now();
                    let result = if bulk {
                        changes.sort_unstable_by_key(|(key, _)| *key);
                        apply(root.as_ref(), &changes)
                    } else {
                        let mut result = root.clone();
                        for &(key, value) in &changes {
                            result = match (result, value) {
                                (Some(node), Some(value)) => Some(insert(&node, key, value)),
                                (None, Some(value)) => Some(leaf(key, value)),
                                (Some(node), None) => remove(&node, &key).0,
                                (None, None) => None,
                            };
                        }
                        result
                    };
                    let digest = result.as_ref().unwrap().hash();
                    black_box(digest);
                    let elapsed = start.elapsed().as_nanos();
                    if let Some(expected) = expected {
                        assert_eq!(digest, expected);
                    } else {
                        expected = Some(digest);
                    }
                    if sample >= 20 {
                        times.push(elapsed);
                    }
                }
                times.sort_unstable();
                println!(
                    "{},{count},{},{}",
                    if bulk { "bulk" } else { "sequential" },
                    times[100],
                    times[190]
                );
            }
        }
    }

    #[test]
    fn bulk_matches_eager_across_forks_deletions_and_intermediate_reads() {
        let mut rng = StdRng::seed_from_u64(0xB017);
        let mut eager = StateTree::new();
        let mut bulk = eager.clone();
        for _ in 0..80 {
            let parent = bulk.clone();
            let parent_root = parent.root();
            bulk.begin_batch();
            for step in 0..120 {
                let key = StateKey::Order(rng.random_range(0..500));
                if rng.random_bool(0.4) {
                    assert_eq!(eager.remove(key), bulk.remove(key));
                } else {
                    let value = rng.random::<u64>().to_be_bytes();
                    eager.put(key, &value);
                    bulk.put(key, &value);
                }
                if step % 31 == 0 {
                    assert_eq!(eager.root(), bulk.root());
                }
            }
            bulk.finish_batch();
            assert_eq!(eager.root(), bulk.root());
            assert_eq!(parent.root(), parent_root);
        }
    }

    #[test]
    fn unchanged_batch_shares_root_and_full_delete_collapses() {
        let mut tree = StateTree::new();
        for id in 0..32 {
            tree.put(StateKey::Order(id), &[1]);
        }
        let original = tree.root.clone().unwrap();
        tree.begin_batch();
        for id in 0..32 {
            tree.put(StateKey::Order(id), &[1]);
        }
        tree.remove(StateKey::Order(999));
        tree.finish_batch();
        assert!(Arc::ptr_eq(&original, tree.root.as_ref().unwrap()));
        tree.begin_batch();
        for id in 0..32 {
            tree.remove(StateKey::Order(id));
        }
        tree.finish_batch();
        assert_eq!(tree.root(), StateTree::new().root());
    }
}
