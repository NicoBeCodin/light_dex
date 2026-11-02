//! Compare the same sixteen quote instructions, separately signed/enveloped
//! transactions versus one atomic UpdateQuotes. Execution/root cost only;
//! signature, network, and consensus savings are deliberately not timed here.
use light_dex::{
    blockchain::{Block, BlockchainState, QuoteOrder, Transaction},
    exchange::{
        ExchangeState,
        order::{OrderType, Side},
    },
};
use std::{hint::black_box, time::Instant};

fn main() {
    let samples = 300;
    println!("operation,instructions,samples,p50_ns,p95_ns");
    let mut base = ExchangeState::default();
    for nonce in 0..10_000u64 {
        base.execute(&Transaction::PlaceOrder {
            account_id: 1.into(),
            market_id: 0,
            order_type: OrderType::LO,
            side: Side::Sell,
            price: 200,
            quantity: 1,
            nonce,
        })
        .unwrap();
    }
    for nonce in 0..8u64 {
        base.execute(&Transaction::PlaceOrder {
            account_id: 0.into(),
            market_id: 0,
            order_type: OrderType::LO,
            side: Side::Buy,
            price: 100 - nonce,
            quantity: 1,
            nonce,
        })
        .unwrap();
    }
    let cancel: Vec<_> = base.account_orders(0.into()).iter().map(|o| o.id).collect();
    let place: Vec<_> = (0..8)
        .map(|level| QuoteOrder {
            order_type: OrderType::LO,
            side: Side::Buy,
            price: 101 - level,
            quantity: 1,
            reduce_only: false,
        })
        .collect();
    for batch in [false, true] {
        let mut timings = Vec::new();
        for sample in 0..samples + 20 {
            let mut state = base.fork();
            let start = Instant::now();
            if batch {
                state
                    .execute(&Transaction::UpdateQuotes {
                        reduce: vec![],
                        account_id: 0.into(),
                        market_id: 0,
                        cancel: cancel.clone(),
                        place: place.clone(),
                        nonce: 8,
                    })
                    .unwrap();
            } else {
                for (index, &id) in cancel.iter().enumerate() {
                    state
                        .execute(&Transaction::CancelOrder {
                            account_id: 0.into(),
                            market_id: 0,
                            order_id: id,
                            nonce: 8 + index as u64,
                        })
                        .unwrap();
                }
                for (index, order) in place.iter().enumerate() {
                    state
                        .execute(&Transaction::PlaceOrder {
                            account_id: 0.into(),
                            market_id: 0,
                            side: order.side,
                            order_type: order.order_type,
                            price: order.price,
                            quantity: order.quantity,
                            nonce: 16 + index as u64,
                        })
                        .unwrap();
                }
            }
            black_box(state.state_digest());
            let elapsed = start.elapsed().as_nanos();
            assert_eq!(state.account_orders(0.into()).len(), 8);
            if sample >= 20 {
                timings.push(elapsed);
            }
        }
        timings.sort_unstable();
        println!(
            "{},16,{samples},{},{}",
            if batch {
                "atomic_quotes"
            } else {
                "individual_quotes"
            },
            timings[samples / 2],
            timings[samples * 95 / 100]
        );
    }
    // Real proposals already coalesce Merkle/account writes across individual
    // transactions. Keep this separate from the standalone comparison above.
    for batch in [false, true] {
        let mut build_times = Vec::new();
        let mut validate_times = Vec::new();
        for sample in 0..samples + 20 {
            let mut chain = BlockchainState::new();
            chain.commit_block(&Block::new_initial_block()).unwrap();
            chain.exchange_state = base.fork();
            let mut follower = BlockchainState::new();
            follower.commit_block(&Block::new_initial_block()).unwrap();
            follower.exchange_state = base.fork();
            if batch {
                chain.add_transaction(&Transaction::UpdateQuotes {
                    reduce: vec![],
                    account_id: 0.into(),
                    market_id: 0,
                    cancel: cancel.clone(),
                    place: place.clone(),
                    nonce: 8,
                });
            } else {
                for (index, &id) in cancel.iter().enumerate() {
                    chain.add_transaction(&Transaction::CancelOrder {
                        account_id: 0.into(),
                        market_id: 0,
                        order_id: id,
                        nonce: 8 + index as u64,
                    });
                }
                for (index, order) in place.iter().enumerate() {
                    chain.add_transaction(&Transaction::PlaceOrder {
                        account_id: 0.into(),
                        market_id: 0,
                        side: order.side,
                        order_type: order.order_type,
                        price: order.price,
                        quantity: order.quantity,
                        nonce: 16 + index as u64,
                    });
                }
            }
            let start = Instant::now();
            let block = chain.build_block().unwrap();
            let build_ns = start.elapsed().as_nanos();
            assert_eq!(block.transactions.len(), if batch { 1 } else { 16 });
            let start = Instant::now();
            follower.validate_block(black_box(&block)).unwrap();
            let validate_ns = start.elapsed().as_nanos();
            if sample >= 20 {
                build_times.push(build_ns);
                validate_times.push(validate_ns);
            }
        }
        for (operation, mut times) in [("build", build_times), ("validate", validate_times)] {
            times.sort_unstable();
            println!(
                "{operation}_{},16,{samples},{},{}",
                if batch {
                    "atomic_quotes"
                } else {
                    "individual_quotes"
                },
                times[samples / 2],
                times[samples * 95 / 100]
            );
        }
    }
    amendment_cost(base, &cancel, &place);
}

fn amendment_cost(mut base: ExchangeState, cancel: &[u64], place: &[QuoteOrder]) {
    use light_dex::blockchain::QuoteReduction;
    let initial: Vec<_> = place
        .iter()
        .cloned()
        .map(|mut q| {
            q.quantity = 10;
            q.order_type = OrderType::PostOnly;
            q
        })
        .collect();
    base.execute(&Transaction::UpdateQuotes {
        account_id: 0.into(),
        market_id: 0,
        nonce: 8,
        cancel: cancel.to_vec(),
        reduce: vec![],
        place: initial,
    })
    .unwrap();
    let ids: Vec<_> = base.account_orders(0.into()).iter().map(|o| o.id).collect();
    for amend in [false, true] {
        let tx = if amend {
            Transaction::UpdateQuotes {
                account_id: 0.into(),
                market_id: 0,
                nonce: 9,
                cancel: vec![],
                place: vec![],
                reduce: ids
                    .iter()
                    .map(|&id| QuoteReduction {
                        order_id: id,
                        remaining_quantity: 1,
                    })
                    .collect(),
            }
        } else {
            Transaction::UpdateQuotes {
                account_id: 0.into(),
                market_id: 0,
                nonce: 9,
                cancel: ids.clone(),
                reduce: vec![],
                place: place
                    .iter()
                    .cloned()
                    .map(|mut q| {
                        q.order_type = OrderType::PostOnly;
                        q
                    })
                    .collect(),
            }
        };
        let mut times = Vec::new();
        for sample in 0..320 {
            let mut fork = base.fork();
            let start = Instant::now();
            fork.execute(black_box(&tx)).unwrap();
            black_box(fork.state_digest());
            let elapsed = start.elapsed().as_nanos();
            assert!(
                fork.account_orders(0.into())
                    .iter()
                    .all(|o| o.remaining_quantity == 1)
            );
            if sample >= 20 {
                times.push(elapsed);
            }
        }
        times.sort_unstable();
        println!(
            "{},{},300,{},{}",
            if amend {
                "reduce_eight_quotes"
            } else {
                "replace_eight_smaller_quotes"
            },
            if amend { 8 } else { 16 },
            times[150],
            times[285]
        );
    }
}
