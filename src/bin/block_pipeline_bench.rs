//! Offline proposal/validation and backlog costs; excludes networking and disk.
use light_dex::{
    blockchain::{Block, BlockchainState, Transaction},
    exchange::{
        ExchangeState,
        account::{Account, Balance},
        market::USD,
    },
};
use std::{collections::HashMap, hint::black_box, time::Instant};

fn fixture(accounts: usize, pending: usize) -> BlockchainState {
    let mut state = BlockchainState::new();
    state.commit_block(&Block::new_initial_block()).unwrap();
    state.exchange_state = ExchangeState::with_accounts((0..accounts).map(|id| {
        Account {
            id: (id as u64).into(),
            nonce: 0,
            used_nonces: [0; 2],
            balances: HashMap::from([(
                USD,
                Balance {
                    available: 1_000_000_000,
                    locked: 0,
                },
            )])
            .into_iter()
            .collect(),
        }
    }));
    for index in 0..pending {
        assert!(state.add_transaction(&Transaction::Transfer {
            from: ((index % accounts) as u64).into(),
            to: (((index + 1) % accounts) as u64).into(),
            asset: USD,
            amount: 1,
            nonce: (index / accounts) as u64,
        }));
    }
    state
}

fn main() {
    println!("operation,accounts,backlog,samples,p50_ns,p95_ns");
    for (accounts, pending) in [(200, 512), (200, 4096), (200, 16384), (10000, 512)] {
        let mut rows = [Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        for _ in 0..30 {
            let mut state = fixture(accounts, pending);
            let mut follower = fixture(accounts, 0);
            let start = Instant::now();
            let selected = black_box(state.build_executable_transactions());
            rows[0].push(start.elapsed().as_nanos());
            let start = Instant::now();
            let block = black_box(state.build_block().unwrap());
            rows[1].push(start.elapsed().as_nanos());
            let start = Instant::now();
            follower.validate_block(black_box(&block)).unwrap();
            rows[2].push(start.elapsed().as_nanos());
            let start = Instant::now();
            state.validate_block(black_box(&block)).unwrap();
            rows[4].push(start.elapsed().as_nanos());
            // Fix cleanup batch size so the backlog sweep stays comparable.
            let mut cleanup = block;
            cleanup.transactions = selected.into_iter().take(512).collect();
            let start = Instant::now();
            state.remove_included_txs_from_mempool(black_box(&cleanup));
            rows[3].push(start.elapsed().as_nanos());
            assert_eq!(state.mempool.len(), pending - 512);
            black_box(state);
        }
        for (name, mut samples) in [
            "select",
            "build",
            "validate",
            "cleanup_512",
            "validate_cached_proposer",
        ]
        .into_iter()
        .zip(rows)
        {
            samples.sort_unstable();
            println!(
                "{name},{accounts},{pending},{},{},{}",
                samples.len(),
                samples[samples.len() / 2],
                samples[(samples.len() * 95).div_ceil(100) - 1]
            );
        }
    }
}
