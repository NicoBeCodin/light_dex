use super::{
    Block, BlockchainState, LEGACY_STATE_ROOT_VERSION, MERKLE_STATE_ROOT_VERSION, Transaction,
};
use crate::exchange::order::{OrderType, Side};

#[test]
fn quote_action_budget_bounds_backlog_and_low_nonce_can_displace_future_work() {
    let mut chain = BlockchainState::new();
    chain.commit_block(&Block::new_initial_block()).unwrap();
    let quote = |nonce| Transaction::UpdateQuotes {
        account_id: 0.into(),
        market_id: 0,
        nonce,
        cancel: (0..32).collect(),
        reduce: vec![],
        place: vec![],
    };
    for nonce in 1..=15 {
        assert!(chain.add_transaction(&quote(nonce)));
    }
    assert!(chain.mempool.queued_actions() <= BlockchainState::MAX_PENDING_ACTIONS_PER_ACCOUNT);
    assert!(!chain.add_transaction(&quote(16)));
    assert!(chain.add_transaction(&quote(0)));
    chain.mempool.assert_indexes();
    assert_eq!(
        chain
            .build_executable_transactions()
            .first()
            .unwrap()
            .nonce(),
        1
    );
    let block = chain.build_block().unwrap();
    chain.commit_block(&block).unwrap();
    chain.remove_included_txs_from_mempool(&block);
    chain.mempool.assert_indexes();
    assert_eq!(chain.mempool.queued_bytes(), 0);
    assert_eq!(chain.mempool.queued_actions(), 0);
}

#[test]
fn work_budget_bounds_failed_orders_and_follower_reexecutes_same_prefix() {
    let mut proposer = BlockchainState::new();
    proposer.commit_block(&Block::new_initial_block()).unwrap();
    let mut follower = BlockchainState::new();
    follower.commit_block(&Block::new_initial_block()).unwrap();
    for account in 0..20u64 {
        proposer.add_transaction(&Transaction::PlaceOrder {
            account_id: account.into(),
            market_id: 0,
            side: Side::Buy,
            order_type: OrderType::FOK,
            price: 100,
            quantity: 1,
            nonce: 0,
        });
    }
    let block = proposer.build_block().unwrap();
    // Failed FOK previews receive a conservative 512-fill work charge.
    assert_eq!(block.transactions.len(), 7);
    let receipts = follower.commit_block(&block).unwrap();
    assert!(receipts.iter().all(Result::is_err));
    assert_eq!(follower.exchange_state.state_digest(), block.state_root);
    let mut excessive = block.clone();
    excessive.sequence += 1;
    excessive.previous_hash = block.to_digest();
    excessive.transactions = (10..18u64)
        .map(|owner| Transaction::PlaceOrder {
            account_id: owner.into(),
            market_id: 0,
            side: Side::Buy,
            order_type: OrderType::FOK,
            price: 100,
            quantity: 1,
            nonce: 0,
        })
        .collect();
    assert!(
        follower
            .prepare_block(&excessive)
            .err()
            .unwrap()
            .contains("work")
    );
}

#[test]
fn block_batch_matches_individual_execution_including_failed_transactions() {
    let mut state = BlockchainState::new();
    state.commit_block(&Block::new_initial_block()).unwrap();
    for account in 0..20u64 {
        for nonce in 0..10 {
            state.add_transaction(&Transaction::Transfer {
                from: account.into(),
                to: ((account + 1) % 20).into(),
                asset: 0,
                amount: if nonce == 5 { u64::MAX } else { 1 },
                nonce,
            });
        }
    }
    let block = state.build_block().unwrap();
    let mut eager = state.exchange_state.fork();
    for tx in &block.transactions {
        let _ = eager.execute_with_receipt(tx);
    }
    assert_eq!(block.state_root, eager.state_digest());
    assert!(eager.verify_state_tree());
}

#[test]
fn asynchronous_journal_does_not_publish_before_completion_is_consumed() {
    let dir = std::env::temp_dir().join(format!(
        "light-dex-durable-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let path = dir.join("node.json");
    let mut state = BlockchainState::load(path.clone()).unwrap();
    let completed = state
        .persist_commit_async(Block::new_initial_block(), None)
        .unwrap();
    let record = completed.recv().unwrap().unwrap();
    assert_eq!(state.blocks.len(), 0);
    state.commit_durable(record).unwrap();
    assert_eq!(state.blocks.len(), 1);
    drop(state);
    assert_eq!(BlockchainState::load(path).unwrap().blocks.len(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn asynchronous_journal_failure_leaves_committed_state_unchanged() {
    let dir = std::env::temp_dir().join(format!(
        "light-dex-durable-fail-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("node.json");
    let mut state = BlockchainState::load(path.clone()).unwrap();
    let root = state.exchange_state.state_digest();
    // A directory at the journal path makes opening the append fail.
    std::fs::create_dir(BlockchainState::journal_path(&path)).unwrap();
    let completed = state
        .persist_commit_async(Block::new_initial_block(), None)
        .unwrap();
    assert!(completed.recv().unwrap().is_err());
    assert!(state.blocks.is_empty());
    assert_eq!(state.exchange_state.state_digest(), root);
    assert!(
        state
            .persist_commit_async(Block::new_initial_block(), None)
            .unwrap()
            .recv()
            .unwrap()
            .is_err()
    );
    drop(state);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn indexed_selection_matches_arrival_order_with_nonce_gaps_and_conflicts() {
    use rand::{SeedableRng, rngs::StdRng, seq::SliceRandom};
    let mut state = BlockchainState::new();
    state.commit_block(&Block::new_initial_block()).unwrap();
    let mut reference = Vec::new();
    for account in 0..60u64 {
        for nonce in 0..30u64 {
            if account % 3 == 0 && nonce == 7 {
                continue;
            }
            reference.push(Transaction::Transfer {
                from: account.into(),
                to: 99.into(),
                asset: 0,
                amount: 1,
                nonce,
            });
        }
    }
    reference.shuffle(&mut StdRng::seed_from_u64(42));
    for tx in &reference {
        assert!(state.add_transaction(tx));
    }
    for _ in 0..6 {
        state.mempool.assert_indexes();
        let expected = reference
            .iter()
            .take(BlockchainState::MAX_PROPOSAL_TRANSACTIONS)
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(state.build_executable_transactions(), expected);
        if expected.is_empty() {
            break;
        }
        let block = state.build_block().unwrap();
        assert!(
            state
                .commit_block(&block)
                .unwrap()
                .iter()
                .all(Result::is_ok)
        );
        state.remove_included_txs_from_mempool(&block);
        reference.retain(|tx| {
            !block.transactions.contains(tx)
                && state.exchange_state.accounts()[&tx.account_id()].accepts_nonce(tx.nonce())
        });
        assert_eq!(state.mempool.len(), reference.len());
    }
    state.mempool.assert_indexes();
    assert!(state.mempool.is_empty()); // Missing nonce 7 no longer blocks successors.
    assert!(state.build_executable_transactions().is_empty());
}

#[test]
fn failed_nonce_can_be_replaced_and_forward_cursor_survives_cleanup() {
    let mut state = BlockchainState::new();
    state.commit_block(&Block::new_initial_block()).unwrap();
    let bad = Transaction::Transfer {
        from: 0.into(),
        to: 1.into(),
        asset: 0,
        amount: u64::MAX,
        nonce: 0,
    };
    let good = Transaction::Transfer {
        from: 0.into(),
        to: 1.into(),
        asset: 0,
        amount: 1,
        nonce: 0,
    };
    assert!(state.add_transaction(&bad));
    assert!(!state.add_transaction(&good)); // A nonce has one pending owner.
    let first_batch = state.mempool.forward_batch(0, 1);
    let cursor = first_batch[0].0 + 1;
    let block = state.build_block().unwrap();
    assert_eq!(block.transactions, vec![bad.clone()]);
    assert!(state.commit_block(&block).unwrap()[0].is_err());
    state.remove_included_txs_from_mempool(&block);
    assert!(state.add_transaction(&good));
    assert_eq!(state.build_executable_transactions(), vec![good.clone()]);
    assert_eq!(state.mempool.forward_batch(cursor, 64)[0].1, good);
    assert!(!state.add_transaction(&bad)); // Cannot replace a pending nonce.
    state.mempool.assert_indexes();
}

#[test]
fn failed_account_does_not_fill_block_with_its_later_actions() {
    let mut state = BlockchainState::new();
    state.commit_block(&Block::new_initial_block()).unwrap();
    let bad = Transaction::Transfer {
        from: 0.into(),
        to: 1.into(),
        asset: 0,
        amount: u64::MAX,
        nonce: 0,
    };
    assert!(state.add_transaction(&bad));
    for nonce in 1..20 {
        assert!(state.add_transaction(&Transaction::Transfer {
            from: 0.into(),
            to: 1.into(),
            asset: 0,
            amount: 1,
            nonce,
        }));
    }
    let independent = Transaction::Transfer {
        from: 2.into(),
        to: 3.into(),
        asset: 0,
        amount: 1,
        nonce: 0,
    };
    assert!(state.add_transaction(&independent));
    let block = state.build_block().unwrap();
    assert_eq!(block.transactions, vec![bad, independent]);
    let outcomes = state.commit_block(&block).unwrap();
    assert!(outcomes[0].is_err());
    assert!(outcomes[1].is_ok());
    state.remove_included_txs_from_mempool(&block);
    assert_eq!(state.build_executable_transactions().len(), 19);
}

#[test]
fn full_account_queue_still_admits_the_missing_head_nonce() {
    let mut state = BlockchainState::new();
    for nonce in 1..BlockchainState::MAX_PENDING_PER_ACCOUNT as u64 {
        assert!(state.add_transaction(&Transaction::Transfer {
            from: 0.into(),
            to: 1.into(),
            asset: 0,
            amount: 1,
            nonce,
        }));
    }
    assert_eq!(
        state.build_executable_transactions().len(),
        BlockchainState::MAX_PENDING_PER_ACCOUNT - 1
    );
    assert!(state.add_transaction(&Transaction::Transfer {
        from: 0.into(),
        to: 1.into(),
        asset: 0,
        amount: 1,
        nonce: 0,
    }));
    assert_eq!(
        state.mempool.len(),
        BlockchainState::MAX_PENDING_PER_ACCOUNT
    );
    assert_eq!(
        state.build_executable_transactions().len(),
        BlockchainState::MAX_PENDING_PER_ACCOUNT
    );
    state.mempool.assert_indexes();
}

#[test]
fn speculative_block_does_not_publish_and_rejected_root_leaves_parent_intact() {
    let mut leader = BlockchainState::new();
    let mut follower = BlockchainState::new();
    let genesis = Block::new_initial_block();
    leader.commit_block(&genesis).unwrap();
    follower.commit_block(&genesis).unwrap();
    let parent_root = leader.exchange_state.state_digest();
    leader.add_transaction(&Transaction::Transfer {
        from: 0.into(),
        to: 1.into(),
        asset: 0,
        amount: 5,
        nonce: 0,
    });
    let block = leader.build_block().unwrap();
    assert!(leader.prepared_block.is_some());
    assert_eq!(leader.exchange_state.state_digest(), parent_root);
    let mut corrupt = block.clone();
    corrupt.state_root[0] ^= 1;
    assert!(follower.validate_block(&corrupt).is_err());
    assert_eq!(follower.exchange_state.state_digest(), parent_root);
    follower.validate_block(&block).unwrap();
    leader.commit_block(&block).unwrap();
    follower.commit_block(&block).unwrap();
    assert_eq!(
        leader.exchange_state.state_digest(),
        follower.exchange_state.state_digest()
    );
    assert_eq!(leader.exchange_state.state_digest(), block.state_root);
}

#[test]
fn mempool_rejects_far_future_nonces_without_poisoning_retries() {
    let mut state = BlockchainState::new();
    let transfer = |nonce| Transaction::Transfer {
        from: 0.into(),
        to: 1.into(),
        asset: 0,
        amount: 1,
        nonce,
    };
    for nonce in 0..BlockchainState::MAX_PENDING_PER_ACCOUNT as u64 {
        assert!(state.add_transaction(&transfer(nonce)));
    }
    let future = transfer(BlockchainState::MAX_PENDING_PER_ACCOUNT as u64);
    assert!(!state.add_transaction(&future));
    assert!(!state.seen_transactions.contains(&future.to_digest()));
}

#[test]
fn committed_fills_have_stable_coordinates_and_replay() {
    let directory =
        std::env::temp_dir().join(format!("light-dex-trades-{}", rand::random::<u64>()));
    let path = directory.join("node-0.json");
    let mut state = BlockchainState::load(path.clone()).unwrap();
    state.commit_block(&Block::new_initial_block()).unwrap();
    for (account_id, price) in [(1, 100), (2, 101)] {
        assert!(state.add_transaction(&Transaction::PlaceOrder {
            account_id: account_id.into(),
            market_id: 0,
            order_type: OrderType::LO,
            side: Side::Sell,
            price,
            quantity: 1,
            nonce: 0,
        }));
    }
    let block = state.build_block().unwrap();
    state.commit_block(&block).unwrap();
    state.remove_included_txs_from_mempool(&block);
    assert!(state.add_transaction(&Transaction::PlaceOrder {
        account_id: 3.into(),
        market_id: 0,
        order_type: OrderType::IOC,
        side: Side::Buy,
        price: 101,
        quantity: 2,
        nonce: 0,
    }));
    let block = state.build_block().unwrap();
    let receipts = state.commit_block_with_receipts(&block).unwrap();
    let trades = receipts[0].as_ref().unwrap();
    assert_eq!(trades.len(), 2);
    assert_eq!(
        trades.iter().map(|trade| trade.price).collect::<Vec<_>>(),
        vec![100, 101]
    );
    assert_eq!(
        trades
            .iter()
            .map(|trade| trade.fill_index)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert!(
        trades
            .iter()
            .all(|trade| trade.block_sequence == block.sequence && trade.taker_account == 3.into())
    );
    let replay = BlockchainState::load(path.clone()).unwrap();
    assert_eq!(
        replay.recent_trades.iter().cloned().collect::<Vec<_>>(),
        trades.clone()
    );
    std::fs::remove_file(BlockchainState::journal_path(&path)).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn executable_queue_keeps_later_valid_nonce_for_same_account() {
    let mut state = BlockchainState::new();
    let mut account = state.exchange_state.accounts()[&0.into()].clone();
    account.nonce = 1;
    account.used_nonces = [1, 0];
    state.exchange_state.upsert_genesis_account(account);
    for (nonce, amount) in [(0, 1), (1, 2), (2, 3)] {
        state.add_transaction(&Transaction::Transfer {
            from: 0.into(),
            to: 1.into(),
            asset: 0,
            amount,
            nonce,
        });
    }
    let selected = state.build_executable_transactions();
    assert_eq!(
        selected.iter().map(Transaction::nonce).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[test]
fn rejects_wrong_state_root_and_recovers_committed_blocks() {
    let directory = std::env::temp_dir().join(format!("light-dex-chain-{}", rand::random::<u64>()));
    let path = directory.join("node-0.json");
    let mut state = BlockchainState::load(path.clone()).unwrap();
    state.commit_block(&Block::new_initial_block()).unwrap();
    let tx = Transaction::Transfer {
        from: 0.into(),
        to: 1.into(),
        asset: 0,
        amount: 5,
        nonce: 0,
    };
    state.add_transaction(&tx);
    let block = state.build_block().unwrap();
    let mut corrupt = block.clone();
    corrupt.state_root[0] ^= 1;
    assert!(state.commit_block(&corrupt).is_err());
    state.commit_block(&block).unwrap();
    let recovered = BlockchainState::load(path.clone()).unwrap();
    assert_eq!(recovered.blocks.len(), 2);
    assert_eq!(
        recovered.exchange_state.state_digest(),
        state.exchange_state.state_digest()
    );
    assert_eq!(recovered.exchange_state.accounts()[&0.into()].nonce, 1);
    std::fs::remove_file(BlockchainState::journal_path(&path)).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn failed_transaction_keeps_nonce_and_can_be_retried_after_recovery() {
    let directory = std::env::temp_dir().join(format!("light-dex-retry-{}", rand::random::<u64>()));
    let path = directory.join("node-0.json");
    let mut state = BlockchainState::load(path.clone()).unwrap();
    state.commit_block(&Block::new_initial_block()).unwrap();
    let tx = Transaction::Transfer {
        from: 0.into(),
        to: 1.into(),
        asset: 0,
        amount: 2_000_000 * crate::exchange::market::USD_ATOMS,
        nonce: 0,
    };
    assert!(state.add_transaction(&tx));
    let block = state.build_block().unwrap();
    assert!(state.commit_block(&block).unwrap()[0].is_err());
    assert_eq!(state.exchange_state.accounts()[&0.into()].nonce, 0);
    let mut recovered = BlockchainState::load(path.clone()).unwrap();
    assert_eq!(recovered.exchange_state.accounts()[&0.into()].nonce, 0);
    assert!(recovered.add_transaction(&tx));
    std::fs::remove_file(BlockchainState::journal_path(&path)).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn legacy_snapshot_and_interrupted_journal_append_recover() {
    let directory =
        std::env::temp_dir().join(format!("light-dex-legacy-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("node-0.json");
    let genesis = Block::new_initial_block();
    std::fs::write(&path, serde_json::to_vec(&vec![genesis]).unwrap()).unwrap();
    let mut state = BlockchainState::load(path.clone()).unwrap();
    let tx = Transaction::Transfer {
        from: 0.into(),
        to: 1.into(),
        asset: 0,
        amount: 1,
        nonce: 0,
    };
    assert!(state.add_transaction(&tx));
    let block = state.build_block().unwrap();
    state.commit_block(&block).unwrap();
    let journal = BlockchainState::journal_path(&path);
    assert_eq!(
        std::fs::read_to_string(&journal).unwrap().lines().count(),
        1
    );
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap()
        .write_all(b"{\"sequence\":")
        .unwrap();
    let recovered = BlockchainState::load(path.clone()).unwrap();
    assert_eq!(recovered.blocks.len(), 2);
    assert_eq!(
        recovered.exchange_state.state_digest(),
        state.exchange_state.state_digest()
    );
    assert!(std::fs::read_to_string(&journal).unwrap().ends_with('\n'));
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn committed_nonce_prunes_conflicting_mempool_entries() {
    let mut state = BlockchainState::new();
    state.commit_block(&Block::new_initial_block()).unwrap();
    let chosen = Transaction::Transfer {
        from: 0.into(),
        to: 1.into(),
        asset: 0,
        amount: 1,
        nonce: 0,
    };
    let conflict = Transaction::Transfer {
        from: 0.into(),
        to: 2.into(),
        asset: 0,
        amount: 1,
        nonce: 0,
    };
    let next = Transaction::Transfer {
        from: 0.into(),
        to: 1.into(),
        asset: 0,
        amount: 1,
        nonce: 1,
    };
    assert!(state.add_transaction(&chosen));
    assert!(!state.add_transaction(&conflict));
    assert!(state.add_transaction(&next));
    let block = state.build_block().unwrap();
    assert_eq!(block.transactions, vec![chosen.clone(), next.clone()]);
    state.commit_block(&block).unwrap();
    state.remove_included_txs_from_mempool(&block);
    assert!(state.mempool.is_empty());
    assert!(!state.add_transaction(&conflict));
    assert!(!state.add_transaction(&chosen));
}

#[test]
fn old_blocks_replay_and_transition_to_versioned_merkle_roots() {
    let directory =
        std::env::temp_dir().join(format!("light-dex-root-upgrade-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("node-0.json");

    let mut old_genesis = Block::new_initial_block();
    old_genesis.state_root_version = LEGACY_STATE_ROOT_VERSION;
    old_genesis.state_root = BlockchainState::new()
        .exchange_state
        .full_state_digest_legacy();
    let old_hash = old_genesis.to_digest();
    let mut old_json = serde_json::to_value(&old_genesis).unwrap();
    old_json
        .as_object_mut()
        .unwrap()
        .remove("state_root_version");
    std::fs::write(&path, serde_json::to_vec(&vec![old_json]).unwrap()).unwrap();

    let mut state = BlockchainState::load(path.clone()).unwrap();
    assert_eq!(state.blocks[0].to_digest(), old_hash);
    let tx = Transaction::Transfer {
        from: 0.into(),
        to: 1.into(),
        asset: 0,
        amount: 5,
        nonce: 0,
    };
    let mut legacy_exchange = state.exchange_state.clone();
    legacy_exchange.execute(&tx).unwrap();
    let old_block = Block {
        oracle_updates: Vec::new(),
        sequence: 1,
        previous_hash: old_hash,
        transactions: vec![tx],
        state_root: legacy_exchange.full_state_digest_legacy(),
        state_root_version: LEGACY_STATE_ROOT_VERSION,
        authentication: None,
    };
    state.commit_block(&old_block).unwrap();
    let mut recovered = BlockchainState::load(path.clone()).unwrap();
    assert_eq!(recovered.blocks.len(), 2);
    assert_eq!(recovered.blocks[1].to_digest(), old_block.to_digest());

    let next = Transaction::Transfer {
        from: 0.into(),
        to: 1.into(),
        asset: 0,
        amount: 1,
        nonce: 1,
    };
    assert!(recovered.add_transaction(&next));
    let upgraded = recovered.build_block().unwrap();
    assert_eq!(upgraded.state_root_version, MERKLE_STATE_ROOT_VERSION);
    assert_eq!(upgraded.previous_hash, old_block.to_digest());
    recovered.commit_block(&upgraded).unwrap();
    let mut replayed = BlockchainState::load(path.clone()).unwrap();
    assert_eq!(replayed.blocks.len(), 3);
    assert_eq!(
        replayed.exchange_state.state_digest(),
        recovered.exchange_state.state_digest()
    );

    let mut downgrade = upgraded.clone();
    downgrade.sequence += 1;
    downgrade.previous_hash = upgraded.to_digest();
    downgrade.state_root_version = LEGACY_STATE_ROOT_VERSION;
    assert!(
        replayed
            .validate_block(&downgrade)
            .unwrap_err()
            .contains("downgrade")
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn unversioned_merkle_blocks_from_transition_also_replay() {
    let directory = std::env::temp_dir().join(format!(
        "light-dex-unversioned-merkle-{}",
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("node-0.json");
    let mut state = BlockchainState::new();
    let mut genesis = Block::new_initial_block();
    genesis.state_root_version = LEGACY_STATE_ROOT_VERSION;
    state.commit_block(&genesis).unwrap();
    let tx = Transaction::Transfer {
        from: 0.into(),
        to: 1.into(),
        asset: 0,
        amount: 5,
        nonce: 0,
    };
    let mut exchange = state.exchange_state.clone();
    exchange.execute(&tx).unwrap();
    let next = Block {
        oracle_updates: Vec::new(),
        sequence: 1,
        previous_hash: genesis.to_digest(),
        transactions: vec![tx],
        state_root: exchange.state_digest(),
        state_root_version: LEGACY_STATE_ROOT_VERSION,
        authentication: None,
    };
    state.commit_block(&next).unwrap();
    let old_json = state
        .blocks
        .iter()
        .map(|block| {
            let mut value = serde_json::to_value(block).unwrap();
            value.as_object_mut().unwrap().remove("state_root_version");
            value
        })
        .collect::<Vec<_>>();
    std::fs::write(&path, serde_json::to_vec(&old_json).unwrap()).unwrap();
    let mut recovered = BlockchainState::load(path).unwrap();
    assert_eq!(recovered.blocks[1].to_digest(), next.to_digest());
    assert_eq!(
        recovered.exchange_state.state_digest(),
        state.exchange_state.state_digest()
    );
    assert!(recovered.add_transaction(&Transaction::Transfer {
        from: 0.into(),
        to: 1.into(),
        asset: 0,
        amount: 1,
        nonce: 1,
    }));
    assert_eq!(
        recovered.build_block().unwrap().state_root_version,
        MERKLE_STATE_ROOT_VERSION
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn failed_persistence_does_not_publish_prepared_state() {
    let directory =
        std::env::temp_dir().join(format!("light-dex-atomic-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&directory).unwrap();
    let non_directory = directory.join("file");
    std::fs::write(&non_directory, b"not a directory").unwrap();
    let mut state = BlockchainState::load(non_directory.join("node.json")).unwrap();
    let before = state.exchange_state.state_digest();
    state.validate_block(&Block::new_initial_block()).unwrap();
    assert!(state.prepared_block.is_some());
    assert!(state.commit_block(&Block::new_initial_block()).is_err());
    assert!(state.blocks.is_empty());
    assert_eq!(state.exchange_state.state_digest(), before);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn commit_consumes_matching_prepared_result() {
    let mut state = BlockchainState::new();
    let genesis = Block::new_initial_block();
    state.validate_block(&genesis).unwrap();
    assert!(state.prepared_block.is_some());
    state.commit_block(&genesis).unwrap();
    assert!(state.prepared_block.is_none());

    let tx = Transaction::Transfer {
        from: 0.into(),
        to: 1.into(),
        asset: 0,
        amount: 1,
        nonce: 0,
    };
    assert!(state.add_transaction(&tx));
    let block = state.build_block().unwrap();
    state.validate_block(&block).unwrap();
    let prepared_root = state
        .prepared_block
        .as_ref()
        .unwrap()
        .2
        .exchange
        .state_digest();
    let outcomes = state.commit_block(&block).unwrap();
    assert!(outcomes[0].is_ok());
    assert!(state.prepared_block.is_none());
    assert_eq!(state.exchange_state.state_digest(), prepared_root);
    assert_eq!(state.exchange_state.accounts()[&0.into()].nonce, 1);
}

fn btc_oracle_update(bid: &str) -> crate::oracle::OracleUpdate {
    use crate::exchange::market::{BTC, USD};
    use crate::oracle::{OracleObservation, OraclePair, Venue, index_from_observations};
    let pair = OraclePair::new(BTC, USD);
    let observations = [
        OracleObservation {
            venue: Venue::Binance,
            symbol: "BTCUSDT".into(),
            bid: bid.into(),
            ask: "101".into(),
            source_sequence: Some(1),
            source_time: None,
        },
        OracleObservation {
            venue: Venue::Okx,
            symbol: "BTC-USDT".into(),
            bid: "100".into(),
            ask: "101".into(),
            source_sequence: None,
            source_time: Some("1".into()),
        },
        OracleObservation {
            venue: Venue::Coinbase,
            symbol: "BTC-USD".into(),
            bid: "100".into(),
            ask: "101".into(),
            source_sequence: Some(1),
            source_time: None,
        },
    ];
    let index_e8 = index_from_observations(pair, &observations).unwrap();
    crate::oracle::OracleUpdate {
        pair,
        observations: observations.to_vec(),
        index_e8,
    }
}

#[test]
fn oracle_only_block_has_matching_mark_and_root_on_two_replicas() {
    let mut primary = BlockchainState::new();
    let mut replica = BlockchainState::new();
    let genesis = Block::new_initial_block();
    primary.commit_block(&genesis).unwrap();
    replica.commit_block(&genesis).unwrap();
    let block = primary
        .build_block_with_oracle(vec![btc_oracle_update("100")])
        .unwrap();
    assert!(block.transactions.is_empty());
    replica.validate_block(&block).unwrap();
    primary.commit_block(&block).unwrap();
    replica.commit_block(&block).unwrap();
    assert_eq!(
        primary.exchange_state.oracle_marks(),
        replica.exchange_state.oracle_marks()
    );
    assert_eq!(
        primary.exchange_state.state_digest(),
        replica.exchange_state.state_digest()
    );
    assert!(
        primary
            .build_block_with_oracle(vec![btc_oracle_update("100")])
            .is_none()
    );
    assert!(
        primary
            .build_block_with_oracle(vec![btc_oracle_update("99")])
            .is_some()
    );
}

#[test]
fn oracle_tampering_and_duplicate_pair_update_are_rejected() {
    let mut state = BlockchainState::new();
    state.commit_block(&Block::new_initial_block()).unwrap();
    let update = btc_oracle_update("100");
    let block = state.build_block_with_oracle(vec![update.clone()]).unwrap();
    let mut tampered = block.clone();
    tampered.oracle_updates[0].observations[0].bid = "99".into();
    assert_ne!(tampered.to_digest(), block.to_digest());
    assert!(state.validate_block(&tampered).is_err());
    let mut duplicated = block.clone();
    duplicated.oracle_updates.push(update);
    assert!(state.validate_block(&duplicated).is_err());
}

#[test]
fn committed_oracle_mark_replays_from_journal() {
    let directory =
        std::env::temp_dir().join(format!("light-dex-oracle-{}", rand::random::<u64>()));
    let path = directory.join("node-0.json");
    let mut state = BlockchainState::load(path.clone()).unwrap();
    state.commit_block(&Block::new_initial_block()).unwrap();
    let block = state
        .build_block_with_oracle(vec![btc_oracle_update("100")])
        .unwrap();
    state.commit_block(&block).unwrap();
    let recovered = BlockchainState::load(path).unwrap();
    assert_eq!(
        state.exchange_state.oracle_marks(),
        recovered.exchange_state.oracle_marks()
    );
    assert_eq!(
        state.exchange_state.state_digest(),
        recovered.exchange_state.state_digest()
    );
    std::fs::remove_dir_all(directory).unwrap();
}
