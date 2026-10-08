//! Compare cached Merkle-root reads with the former full-state scan.
//! Run with: cargo run --release --bin state_root_bench
use light_dex::{
    blockchain::Transaction,
    exchange::{
        ExchangeState,
        account::{Account, Balance},
        market::{BTC, USD},
        order::{OrderType, Side},
    },
};
use std::{hint::black_box, time::{Duration, Instant}};

fn fixture(accounts: usize, orders: usize) -> ExchangeState {
    let mut state = ExchangeState::with_accounts((0..accounts as u64).map(|id| Account {
            id: id.into(),
            nonce: 0,
            balances: std::collections::HashMap::from([
                (USD, Balance { available: 1_000_000, locked: 0 }),
                (BTC, Balance { available: 1_000_000, locked: 0 }),
            ]),
        }));
    for index in 0..orders {
        let account_id = 1 + (index % (accounts - 1)) as u64;
        let nonce = state.get_nonce_for_account(account_id.into()).unwrap();
        state.execute(&Transaction::PlaceOrder {
            account_id: account_id.into(),
            market_id: 0,
            order_type: OrderType::LO,
            side: Side::Sell,
            price: 100 + index as u64,
            quantity: 1,
            nonce,
        }).unwrap();
    }
    assert!(state.verify_state_tree());
    state
}

fn measure(mut operation: impl FnMut() -> Duration, samples: usize) -> (u128, u128) {
    let mut times = Vec::with_capacity(samples);
    for _ in 0..samples {
        times.push(operation().as_nanos());
    }
    times.sort_unstable();
    (times[samples / 2], times[(samples * 95 / 100).min(samples - 1)])
}

fn main() {
    println!("accounts,resting_orders,operation,p50_ns,p95_ns");
    for (accounts, orders) in [(100, 0), (100, 1_000), (1_000, 1_000), (5_000, 5_000)] {
        let state = fixture(accounts, orders);
        for (operation, p50, p95) in [
            {
                let (p50, p95) = measure(|| {
                    let start = Instant::now();
                    black_box(state.full_state_digest_legacy());
                    start.elapsed()
                }, 200);
                ("legacy_full_scan", p50, p95)
            },
            {
                let (p50, p95) = measure(|| {
                    let start = Instant::now();
                    black_box(state.state_digest());
                    start.elapsed()
                }, 200);
                ("incremental_root_read", p50, p95)
            },
            {
                let (p50, p95) = measure(|| {
                    let copy = state.clone();
                    let start = Instant::now();
                    black_box(copy.verify_state_tree());
                    start.elapsed()
                }, 20);
                ("full_tree_verify", p50, p95)
            },
            {
                let tx = Transaction::Transfer {
                    from: 0.into(), to: 1.into(), asset: USD, amount: 1, nonce: 0,
                };
                let (p50, p95) = measure(|| {
                    let mut copy = state.clone();
                    let start = Instant::now();
                    black_box(copy.execute(black_box(&tx)).unwrap());
                    black_box(copy.state_digest());
                    start.elapsed()
                }, 200);
                ("transfer_plus_root", p50, p95)
            },
        ] {
            println!("{accounts},{orders},{operation},{p50},{p95}");
        }
    }
}
