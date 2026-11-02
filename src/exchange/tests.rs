use super::*;
use rand::{Rng, SeedableRng, rngs::StdRng};

/// Focused release probe: isolates the eager test-helper path from the batched
/// tree path used for a block. Fixture construction and forks are untimed.
#[test]
#[ignore]
fn batched_spot_sweep_cost_probe() {
    use std::{hint::black_box, time::Instant};
    let mut base = ExchangeState::default();
    for index in 0..500u64 {
        let maker = 1 + index % 99;
        let nonce = base.get_nonce_for_account(maker.into()).unwrap();
        base.execute(&place(maker, Side::Sell, 100, 1, nonce, OrderType::LO))
            .unwrap();
    }
    let taker = place(0, Side::Buy, 100, 500, 0, OrderType::IOC);
    let mut expected_across_modes = None;
    for mode in ["eager", "block_batch"] {
        let mut times = Vec::with_capacity(100);
        let mut expected_root = None;
        for sample in 0..110 {
            let mut state = base.fork();
            if mode == "block_batch" {
                state.begin_state_batch();
            }
            let started = Instant::now();
            let fills = state.execute_with_receipt(black_box(&taker)).unwrap();
            if mode == "block_batch" {
                state.finish_state_batch();
            }
            let root = black_box(state.state_digest());
            let elapsed = started.elapsed().as_nanos();
            assert_eq!(fills.len(), 500);
            assert_eq!(state.open_order_count(), 0);
            if let Some(expected) = expected_root {
                assert_eq!(root, expected);
            }
            if let Some(expected) = expected_across_modes {
                assert_eq!(root, expected);
            }
            expected_root = Some(root);
            expected_across_modes = Some(root);
            if sample >= 10 {
                times.push(elapsed);
            }
        }
        times.sort_unstable();
        println!("{mode},fills=500,p50_ns={},p95_ns={}", times[50], times[94]);
    }
}

#[test]
fn randomized_spot_sequences_match_eager_and_block_batched_roots() {
    use rand::{Rng, SeedableRng, rngs::StdRng};
    for seed in 0..32u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut eager = ExchangeState::default();
        let mut batched = eager.fork();
        batched.begin_state_batch();
        for step in 0..60 {
            let owner = rng.random_range(1..9u64);
            let nonce = eager.get_nonce_for_account(owner.into()).unwrap();
            let tx = match rng.random_range(0..4) {
                0 => Transaction::Transfer {
                    from: owner.into(),
                    to: ((owner % 8) + 1).into(),
                    asset: USD,
                    amount: rng.random_range(1..100),
                    nonce,
                },
                1 => {
                    let orders = eager.account_orders(owner.into());
                    let order_id = orders
                        .get(rng.random_range(0..orders.len().max(1)))
                        .map_or(u64::MAX, |order| order.id);
                    Transaction::CancelOrder {
                        account_id: owner.into(),
                        market_id: BTC_USD,
                        order_id,
                        nonce,
                    }
                }
                _ => place(
                    owner,
                    if rng.random_bool(0.5) {
                        Side::Buy
                    } else {
                        Side::Sell
                    },
                    rng.random_range(99..103),
                    rng.random_range(1..5),
                    nonce,
                    if rng.random_bool(0.75) {
                        OrderType::LO
                    } else {
                        OrderType::IOC
                    },
                ),
            };
            let first = eager.execute_with_receipt(&tx);
            let second = batched.execute_with_receipt(&tx);
            assert_eq!(first.is_ok(), second.is_ok(), "seed={seed} step={step}");
            assert_eq!(
                eager.get_nonce_for_account(owner.into()).unwrap(),
                batched.get_nonce_for_account(owner.into()).unwrap(),
                "seed={seed} step={step}"
            );
        }
        batched.finish_state_batch();
        assert_eq!(eager.state_digest(), batched.state_digest(), "seed={seed}");
        assert!(eager.verify_state_tree());
        assert!(batched.verify_state_tree());
    }
}

#[test]
fn quote_batch_is_atomic_uses_one_nonce_and_preserves_untouched_priority() {
    use crate::blockchain::QuoteOrder;
    let mut state = ExchangeState::default();
    state
        .execute(&place(1, Side::Sell, 100, 4, 0, OrderType::LO))
        .unwrap();
    state
        .execute(&place(1, Side::Sell, 100, 6, 1, OrderType::LO))
        .unwrap();
    let old: Vec<_> = state
        .account_orders(1.into())
        .iter()
        .map(|o| o.id)
        .collect();
    let quote = QuoteOrder {
        order_type: OrderType::LO,
        side: Side::Sell,
        price: 101,
        quantity: 2,
        reduce_only: false,
    };
    let root = state.state_digest();
    let next = state.next_order_id;
    let mut bad = quote.clone();
    bad.quantity = u64::MAX;
    assert!(
        state
            .execute(&Transaction::UpdateQuotes {
                reduce: vec![],
                account_id: 1.into(),
                market_id: BTC_USD,
                cancel: vec![old[0]],
                place: vec![quote.clone(), bad],
                nonce: 2
            })
            .is_err()
    );
    assert_eq!(state.state_digest(), root);
    assert_eq!(state.next_order_id, next);
    assert_eq!(state.get_nonce_for_account(1.into()).unwrap(), 2);
    assert_eq!(state.account_orders(1.into()).len(), 2);
    assert!(state.verify_state_tree());
    state
        .execute(&Transaction::UpdateQuotes {
            reduce: vec![],
            account_id: 1.into(),
            market_id: BTC_USD,
            cancel: vec![old[0], u64::MAX],
            place: vec![quote],
            nonce: 2,
        })
        .unwrap();
    assert_eq!(state.get_nonce_for_account(1.into()).unwrap(), 3);
    assert_eq!(
        state.order_books[&BTC_USD].asks[&100].front().unwrap().id,
        old[1]
    );
    assert_eq!(state.account_orders(1.into()).len(), 2);
    assert_eq!(state.open_order_count(), 2);
    assert!(state.verify_state_tree());
    let root = state.state_digest();
    assert!(
        state
            .execute(&Transaction::UpdateQuotes {
                reduce: vec![],
                account_id: 2.into(),
                market_id: BTC_USD,
                cancel: vec![old[1]],
                place: vec![],
                nonce: 0
            })
            .is_err()
    );
    assert_eq!(state.state_digest(), root);
}

#[test]
fn batched_account_encoding_and_owner_indexes_match_rebuild() {
    let mut state = ExchangeState::default();
    let original = state.fork();
    assert!(
        state.accounts[&1.into()]
            .balances
            .ptr_eq(&original.accounts[&1.into()].balances)
    );
    state.begin_state_batch();
    for nonce in 0..50 {
        state
            .execute(&place(
                1,
                Side::Sell,
                100 + nonce % 3,
                2,
                nonce,
                OrderType::LO,
            ))
            .unwrap();
    }
    state
        .execute(&place(2, Side::Buy, 102, 47, 0, OrderType::IOC))
        .unwrap();
    let owned: Vec<_> = state
        .account_orders(1.into())
        .into_iter()
        .map(|o| o.id)
        .collect();
    for (nonce, id) in owned.iter().copied().enumerate() {
        state
            .execute(&Transaction::CancelOrder {
                account_id: 1.into(),
                market_id: BTC_USD,
                order_id: id,
                nonce: 50 + nonce as u64,
            })
            .unwrap();
    }
    // Materializing an intermediate root must not lose subsequent dirty writes.
    assert!(state.verify_state_tree());
    state
        .execute(&Transaction::Transfer {
            from: 1.into(),
            to: 2.into(),
            asset: USD,
            amount: 1,
            nonce: 50 + owned.len() as u64,
        })
        .unwrap();
    state.finish_state_batch();
    let root = state.state_digest();
    state.rebuild_state_tree();
    assert_eq!(state.state_digest(), root);
    assert_eq!(state.open_order_count(), 0);
    assert!(state.account_orders(1.into()).is_empty());
    assert_eq!(original.get_nonce_for_account(1.into()).unwrap(), 0);
    assert_ne!(original.state_digest(), root);
}

#[test]
fn overlarge_sweep_and_failed_fok_are_atomic_and_metered() {
    let mut state = ExchangeState::default();
    for nonce in 0..513 {
        state
            .execute(&place(1, Side::Sell, 100, 1, nonce, OrderType::LO))
            .unwrap();
    }
    let root = state.state_digest();
    assert_eq!(
        state.execute(&place(2, Side::Buy, 100, 513, 0, OrderType::IOC)),
        Err(ExchangeError::WorkLimit)
    );
    assert_eq!(state.state_digest(), root);
    assert!(state.last_execution_work() >= 2048);
    let fills = state
        .execute_with_receipt(&place(2, Side::Buy, 100, 512, 0, OrderType::IOC))
        .unwrap();
    assert_eq!(fills.len(), 512);
    let root = state.state_digest();
    assert!(
        state
            .execute(&place(2, Side::Buy, 100, 2, 1, OrderType::FOK))
            .is_err()
    );
    assert_eq!(state.state_digest(), root);
    assert!(state.last_execution_work() >= 2048);
    assert!(state.verify_state_tree());
}

#[test]
fn cross_perp_quote_batch_updates_portfolio_indexes_and_rolls_back_bad_tail() {
    use crate::blockchain::QuoteOrder;
    use crate::oracle::OraclePair;
    let mut state = ExchangeState::default();
    state
        .create_perp_market(
            1,
            BTC,
            USD,
            DecimalStep::new(1, 1).unwrap(),
            DecimalStep::new(4, 1).unwrap(),
            10,
        )
        .unwrap();
    state.oracle_marks.insert(
        OraclePair::new(BTC, USD),
        OracleMark {
            index_e8: 100 * 100_000_000,
            update_digest: [1; 32],
        },
    );
    state.rebuild_state_tree();
    state
        .execute(&Transaction::SetMarginMode {
            account_id: 0.into(),
            mode: MarginMode::Cross,
            nonce: 0,
        })
        .unwrap();
    state
        .execute(&Transaction::DepositPerpCollateral {
            account_id: 0.into(),
            market_id: 1,
            amount: 100 * USD_ATOMS,
            nonce: 1,
        })
        .unwrap();
    let bid = QuoteOrder {
        order_type: OrderType::LO,
        side: Side::Buy,
        price: 990,
        quantity: 10_000,
        reduce_only: false,
    };
    let ask = QuoteOrder {
        side: Side::Sell,
        price: 1010,
        ..bid.clone()
    };
    state
        .execute(&Transaction::UpdateQuotes {
            reduce: vec![],
            account_id: 0.into(),
            market_id: 1,
            cancel: vec![],
            place: vec![bid.clone(), ask.clone()],
            nonce: 2,
        })
        .unwrap();
    assert_eq!(state.get_nonce_for_account(0.into()).unwrap(), 3);
    assert_eq!(state.account_orders(0.into()).len(), 2);
    assert_eq!(state.perp_accounts_for_market(1), vec![0.into()]);
    assert!(state.perp_risk_for(0.into())[0].initial_required_usd_atoms > 0);
    let old: Vec<_> = state
        .account_orders(0.into())
        .into_iter()
        .map(|o| o.id)
        .collect();
    let root = state.state_digest();
    assert!(
        state
            .execute(&Transaction::UpdateQuotes {
                reduce: vec![],
                account_id: 0.into(),
                market_id: 1,
                cancel: old.clone(),
                place: vec![
                    bid,
                    QuoteOrder {
                        quantity: u64::MAX,
                        ..ask
                    }
                ],
                nonce: 3
            })
            .is_err()
    );
    assert_eq!(state.state_digest(), root);
    assert_eq!(state.account_orders(0.into()).len(), 2);
    state
        .execute(&Transaction::UpdateQuotes {
            reduce: vec![],
            account_id: 0.into(),
            market_id: 1,
            cancel: old,
            place: vec![],
            nonce: 3,
        })
        .unwrap();
    assert_eq!(
        state.perp_risk_for(0.into())[0].initial_required_usd_atoms,
        0
    );
    assert!(state.verify_state_tree());
}

#[test]
fn speculative_fork_shares_storage_and_isolates_nested_order_queues() {
    let mut parent = ExchangeState::default();
    for nonce in 0..200 {
        parent
            .execute(&place(1, Side::Sell, 100, 1, nonce, OrderType::LO))
            .unwrap();
    }
    let root = parent.state_digest();
    let mut fork = parent.fork();
    assert!(parent.accounts.ptr_eq(&fork.accounts));
    assert!(parent.order_books.ptr_eq(&fork.order_books));
    assert!(parent.order_locations.ptr_eq(&fork.order_locations));
    assert!(
        parent.order_books[&BTC_USD]
            .asks
            .ptr_eq(&fork.order_books[&BTC_USD].asks)
    );
    assert!(parent.order_books[&BTC_USD].asks[&100].ptr_eq(&fork.order_books[&BTC_USD].asks[&100]));

    let fills = fork
        .execute_with_receipt(&place(2, Side::Buy, 100, 10, 0, OrderType::IOC))
        .unwrap();
    assert_eq!(fills.len(), 10);
    fork.execute(&Transaction::CancelOrder {
        account_id: 1.into(),
        market_id: BTC_USD,
        order_id: 100,
        nonce: 200,
    })
    .unwrap();
    assert_eq!(parent.order_books[&BTC_USD].asks[&100].len(), 200);
    assert_eq!(fork.order_books[&BTC_USD].asks[&100].len(), 189);
    assert_eq!(parent.get_nonce_for_account(2.into()).unwrap(), 0);
    assert_eq!(parent.state_digest(), root);
    assert_ne!(fork.state_digest(), root);
    assert!(parent.verify_state_tree());
    assert!(fork.verify_state_tree());
    drop(fork);
    assert_eq!(parent.state_digest(), root);
}

#[test]
fn catalog_spot_trade_charges_taker_fee_and_conserves_assets() {
    let mut exchange = ExchangeState::with_claim_faucet(FaucetConfig {
        max_claims: 2,
        usd_per_claim: 1_000 * USD_ATOMS,
        btc_per_claim: BTC_ATOMS,
    })
    .unwrap();
    for id in 0..2 {
        exchange
            .execute(&Transaction::ClaimAirdrop {
                account_id: id.into(),
                nonce: 0,
            })
            .unwrap();
    }
    // ETH spot is market 2, with 4-decimal base lots and a 2 bp taker fee.
    exchange
        .execute(&Transaction::PlaceOrder {
            account_id: 0.into(),
            market_id: 2,
            order_type: OrderType::LO,
            side: Side::Sell,
            price: 10_000,
            quantity: 10_000,
            nonce: 1,
        })
        .unwrap();
    exchange
        .execute(&Transaction::PlaceOrder {
            account_id: 1.into(),
            market_id: 2,
            order_type: OrderType::FOK,
            side: Side::Buy,
            price: 10_000,
            quantity: 10_000,
            nonce: 1,
        })
        .unwrap();
    assert_eq!(exchange.insurance_balance(market::ETH), 2);
    assert_eq!(
        exchange.accounts[&AccountId::from(1)].balances[&market::ETH].available,
        10_000 + 10_000 - 2
    );
    assert!(exchange.verify_state_tree());
}

#[test]
fn oracle_marks_are_independent_by_asset_pair_and_committed_in_tree() {
    use crate::exchange::market::{ETH, USD};
    use crate::oracle::{
        OracleObservation, OraclePair, OracleUpdate, Venue, index_from_observations,
    };
    let mut state = ExchangeState::default();
    state
        .create_spot_market(
            10,
            ETH,
            USD,
            DecimalStep::new(2, 1).unwrap(),
            DecimalStep::new(4, 1).unwrap(),
        )
        .unwrap();
    let make_update = |base_asset, symbols: [&str; 3], bid: &str, ask: &str| {
        let pair = OraclePair::new(base_asset, USD);
        let observations = [
            OracleObservation {
                venue: Venue::Binance,
                symbol: symbols[0].into(),
                bid: bid.into(),
                ask: ask.into(),
                source_sequence: None,
                source_time: None,
            },
            OracleObservation {
                venue: Venue::Okx,
                symbol: symbols[1].into(),
                bid: bid.into(),
                ask: ask.into(),
                source_sequence: None,
                source_time: None,
            },
            OracleObservation {
                venue: Venue::Coinbase,
                symbol: symbols[2].into(),
                bid: bid.into(),
                ask: ask.into(),
                source_sequence: None,
                source_time: None,
            },
        ];
        OracleUpdate {
            pair,
            index_e8: index_from_observations(pair, &observations).unwrap(),
            observations: observations.to_vec(),
        }
    };
    let btc = make_update(BTC, ["BTCUSDT", "BTC-USDT", "BTC-USD"], "100", "101");
    let eth = make_update(ETH, ["ETHUSDT", "ETH-USDT", "ETH-USD"], "2000", "2001");
    state.apply_oracle_update(&btc).unwrap();
    let btc_mark = state.oracle_mark(btc.pair).unwrap();
    state.apply_oracle_update(&eth).unwrap();
    assert_eq!(state.oracle_mark(btc.pair), Some(btc_mark));
    assert_eq!(state.oracle_mark(eth.pair).unwrap().index_e8, eth.index_e8);
    assert_ne!(btc_mark.index_e8, eth.index_e8);
    assert!(state.verify_state_tree());
    let root = state.state_digest();
    let changed_eth = make_update(ETH, ["ETHUSDT", "ETH-USDT", "ETH-USD"], "2001", "2002");
    state.apply_oracle_update(&changed_eth).unwrap();
    assert_ne!(root, state.state_digest());
    assert_eq!(state.oracle_mark(btc.pair), Some(btc_mark));
}

fn place(
    account_id: u64,
    side: Side,
    price: u64,
    quantity: u64,
    nonce: u64,
    order_type: OrderType,
) -> Transaction {
    Transaction::PlaceOrder {
        account_id: account_id.into(),
        market_id: BTC_USD,
        side,
        price,
        quantity,
        nonce,
        order_type,
    }
}
fn balance(exchange: &ExchangeState, account: u64, asset: u64) -> (u64, u64) {
    let b = &exchange.accounts[&account.into()].balances[&asset];
    (b.available, b.locked)
}

fn execute_reference(
    state: &mut ExchangeState,
    tx: &Transaction,
) -> Result<Vec<ExecutedTrade>, ExchangeError> {
    let mut staged = state.clone();
    let trades = match tx {
        Transaction::ReduceOrder { .. }
        | Transaction::UpdateQuotes { .. }
        | Transaction::ClaimAirdrop { .. }
        | Transaction::DepositPerpCollateral { .. }
        | Transaction::WithdrawPerpCollateral { .. }
        | Transaction::PlacePerpOrder { .. }
        | Transaction::SetMarginMode { .. }
        | Transaction::LiquidatePerp { .. } => staged.execute_with_receipt(tx)?,
        Transaction::Transfer {
            from,
            to,
            asset,
            amount,
            nonce,
        } => {
            staged.transfer(*from, *to, *asset, *amount, *nonce)?;
            Vec::new()
        }
        Transaction::PlaceOrder {
            account_id,
            market_id,
            order_type,
            side,
            price,
            quantity,
            nonce,
        } => staged.place_order(
            *account_id,
            *market_id,
            *order_type,
            *side,
            *price,
            *quantity,
            *nonce,
        )?,
        Transaction::CancelOrder {
            account_id,
            market_id,
            order_id,
            nonce,
        } => {
            staged.cancel_order(*account_id, *market_id, *order_id, *nonce)?;
            Vec::new()
        }
    };
    *state = staged;
    state.rebuild_state_tree();
    Ok(trades)
}

#[test]
fn staged_execution_matches_whole_state_clone_reference() {
    let mut rng = StdRng::seed_from_u64(0x5eed_2026);
    let mut fast = ExchangeState::default();
    let mut reference = ExchangeState::default();
    for _ in 0..600 {
        let account_id = rng.random_range(0..20);
        let nonce =
            reference.accounts[&account_id.into()].nonce + u64::from(rng.random_ratio(1, 8));
        let tx = match rng.random_range(0..5) {
            0 => Transaction::Transfer {
                from: account_id.into(),
                to: rng.random_range(0..20).into(),
                asset: rng.random_range(0..2),
                amount: rng.random_range(1..500),
                nonce,
            },
            1 => {
                let owned = reference.order_books[&BTC_USD]
                    .bids
                    .values()
                    .chain(reference.order_books[&BTC_USD].asks.values())
                    .flat_map(|level| level.iter())
                    .find(|order| order.account == account_id.into())
                    .map(|order| order.id);
                Transaction::CancelOrder {
                    account_id: account_id.into(),
                    market_id: BTC_USD,
                    order_id: owned
                        .unwrap_or_else(|| rng.random_range(0..reference.next_order_id + 2)),
                    nonce,
                }
            }
            _ => place(
                account_id,
                if rng.random_bool(0.5) {
                    Side::Buy
                } else {
                    Side::Sell
                },
                rng.random_range(90..110),
                rng.random_range(1..4),
                nonce,
                match rng.random_range(0..3) {
                    0 => OrderType::LO,
                    1 => OrderType::IOC,
                    _ => OrderType::FOK,
                },
            ),
        };
        assert_eq!(
            fast.execute_with_receipt(&tx),
            execute_reference(&mut reference, &tx),
            "{tx:?}"
        );
        assert_eq!(fast.state_digest(), reference.state_digest(), "{tx:?}");
        assert_eq!(
            fast.order_locations.len(),
            reference.order_locations.len(),
            "{tx:?}"
        );
        let mut rebuilt = fast.clone();
        rebuilt.rebuild_state_tree();
        assert_eq!(fast.state_digest(), rebuilt.state_digest(), "{tx:?}");
    }
}

#[test]
fn state_tree_tracks_resting_orders_fills_and_cancellation() {
    let mut state = ExchangeState::default();
    let initial = state.state_digest();
    assert!(
        state
            .state_tree
            .get_value_hash(StateKey::Account(0.into()))
            .is_some()
    );

    state
        .execute(&place(1, Side::Sell, 100, 2, 0, OrderType::LO))
        .unwrap();
    assert!(
        state
            .state_tree
            .get_value_hash(StateKey::Order(0))
            .is_some()
    );
    let after_rest = state.state_digest();
    assert_ne!(initial, after_rest);

    state
        .execute(&place(2, Side::Buy, 100, 1, 0, OrderType::IOC))
        .unwrap();
    let partial = state.state_digest();
    assert_ne!(after_rest, partial);
    assert!(
        state
            .state_tree
            .get_value_hash(StateKey::Order(0))
            .is_some()
    );

    state
        .execute(&Transaction::CancelOrder {
            account_id: 1.into(),
            market_id: BTC_USD,
            order_id: 0,
            nonce: 1,
        })
        .unwrap();
    assert!(
        state
            .state_tree
            .get_value_hash(StateKey::Order(0))
            .is_none()
    );

    let mut rebuilt = state.clone();
    rebuilt.rebuild_state_tree();
    assert_eq!(state.state_digest(), rebuilt.state_digest());
}

#[test]
fn state_tree_root_is_independent_of_insertion_order() {
    let mut forward = StateTree::new();
    let mut reverse = StateTree::new();
    for id in 0..64 {
        forward.put(StateKey::Account(id.into()), &id.to_be_bytes());
    }
    for id in (0u64..64).rev() {
        reverse.put(StateKey::Account(id.into()), &id.to_be_bytes());
    }
    assert_eq!(forward.root(), reverse.root());
    for id in 0..64 {
        assert!(forward.remove(StateKey::Account(id.into())));
    }
    assert_eq!(forward.root(), StateTree::new().root());
}

#[test]
fn genesis_account_update_keeps_commitment_in_sync() {
    let mut state = ExchangeState::default();
    let before = state.state_digest();
    let mut account = state.accounts()[&0.into()].clone();
    account.balances.get_mut(&USD).unwrap().available += 1;
    state.upsert_genesis_account(account);
    assert_ne!(state.state_digest(), before);
    assert!(state.verify_state_tree());
}

#[test]
fn claim_faucet_mints_with_a_committed_claim_limit() {
    let faucet = FaucetConfig {
        max_claims: 2,
        usd_per_claim: 50,
        btc_per_claim: 3,
    };
    let mut state = ExchangeState::with_claim_faucet(faucet).unwrap();
    assert!(state.accounts.is_empty());
    assert_eq!(state.faucet_remaining_claims(), Some(2));
    let first = address::Address::fixture(901);
    let second = address::Address::fixture(902);
    state
        .execute(&Transaction::ClaimAirdrop {
            account_id: first,
            nonce: 0,
        })
        .unwrap();
    assert_eq!(state.accounts[&first].nonce, 1);
    assert_eq!(state.accounts[&first].balances[&USD].available, 50);
    assert_eq!(state.accounts[&first].balances[&BTC].available, 3);
    assert_eq!(state.faucet_remaining_claims(), Some(1));
    let root = state.state_digest();
    assert_eq!(
        state.execute(&Transaction::ClaimAirdrop {
            account_id: first,
            nonce: 0
        }),
        Err(ExchangeError::AlreadyClaimed)
    );
    assert_eq!(state.state_digest(), root);
    state
        .execute(&Transaction::ClaimAirdrop {
            account_id: second,
            nonce: 0,
        })
        .unwrap();
    assert_eq!(state.faucet_remaining_claims(), Some(0));
    assert_eq!(
        state.execute(&Transaction::ClaimAirdrop {
            account_id: address::Address::fixture(903),
            nonce: 0
        }),
        Err(ExchangeError::FaucetExhausted)
    );
    assert!(state.verify_state_tree());
}

#[test]
fn failed_transfer_keeps_sender_nonce_and_balance() {
    let mut exchange = ExchangeState::default();
    let initial = balance(&exchange, 0, USD);
    assert!(
        exchange
            .execute(&Transaction::Transfer {
                from: 0.into(),
                to: 10_000.into(),
                asset: USD,
                amount: 1_000_000 * USD_ATOMS + 1,
                nonce: 0
            })
            .is_err()
    );
    assert_eq!(exchange.accounts[&0.into()].nonce, 0);
    assert_eq!(balance(&exchange, 0, USD), initial);
}

#[test]
fn transfer_creates_address_account_only_after_success() {
    let mut exchange = ExchangeState::default();
    let recipient = crate::exchange::address::Address::fixture(10_000);
    assert!(!exchange.accounts.contains_key(&recipient));
    let transfer = Transaction::Transfer {
        from: 0.into(),
        to: recipient,
        asset: USD,
        amount: 25,
        nonce: 0,
    };
    exchange.execute(&transfer).unwrap();
    assert_eq!(exchange.accounts[&recipient].nonce, 0);
    assert_eq!(exchange.accounts[&recipient].balances[&USD].available, 25);
    assert_eq!(exchange.accounts[&0.into()].nonce, 1);
    assert!(exchange.verify_state_tree());
    assert!(exchange.execute(&transfer).is_err());
    assert_eq!(exchange.accounts[&recipient].balances[&USD].available, 25);
}

#[test]
fn resting_match_and_cancel_conserve_balances() {
    let mut exchange = ExchangeState::default();
    exchange
        .execute(&place(0, Side::Sell, 100, 3, 0, OrderType::LO))
        .unwrap();
    assert_eq!(balance(&exchange, 0, BTC), (1_000 * BTC_ATOMS - 3, 3));
    assert_eq!(
        exchange.order_books[&BTC_USD].asks[&100][0].remaining_quantity,
        3
    );
    exchange
        .execute(&place(1, Side::Buy, 120, 2, 0, OrderType::LO))
        .unwrap();
    assert_eq!(balance(&exchange, 0, BTC), (1_000 * BTC_ATOMS - 3, 1));
    assert_eq!(
        balance(&exchange, 0, USD),
        (1_000_000 * USD_ATOMS + 2_000, 0)
    );
    assert_eq!(balance(&exchange, 1, BTC), (1_000 * BTC_ATOMS + 2, 0));
    assert_eq!(
        balance(&exchange, 1, USD),
        (1_000_000 * USD_ATOMS - 2_000, 0)
    );
    exchange
        .execute(&Transaction::CancelOrder {
            account_id: 0.into(),
            market_id: BTC_USD,
            order_id: 0,
            nonce: 1,
        })
        .unwrap();
    assert_eq!(balance(&exchange, 0, BTC), (1_000 * BTC_ATOMS - 2, 0));
    assert!(exchange.order_books[&BTC_USD].asks.is_empty());
}

#[test]
fn buy_reservation_partial_fill_and_ioc_release() {
    let mut exchange = ExchangeState::default();
    exchange
        .execute(&place(0, Side::Buy, 100, 4, 0, OrderType::LO))
        .unwrap();
    assert_eq!(
        balance(&exchange, 0, USD),
        (1_000_000 * USD_ATOMS - 4_000, 4_000)
    );
    exchange
        .execute(&place(1, Side::Sell, 90, 2, 0, OrderType::IOC))
        .unwrap();
    assert_eq!(
        balance(&exchange, 0, USD),
        (1_000_000 * USD_ATOMS - 4_000, 2_000)
    );
    assert_eq!(balance(&exchange, 0, BTC), (1_000 * BTC_ATOMS + 2, 0));
    assert_eq!(
        balance(&exchange, 1, USD),
        (1_000_000 * USD_ATOMS + 2_000, 0)
    );
    assert_eq!(balance(&exchange, 1, BTC), (1_000 * BTC_ATOMS - 2, 0));
    exchange
        .execute(&place(2, Side::Buy, 90, 1, 0, OrderType::IOC))
        .unwrap();
    assert_eq!(balance(&exchange, 2, USD), (1_000_000 * USD_ATOMS, 0));
    assert!(exchange.order_books[&BTC_USD].asks.is_empty());
}

#[test]
fn fok_failure_is_atomic() {
    let mut exchange = ExchangeState::default();
    let before = exchange.state_digest();
    assert_eq!(
        exchange.execute(&place(0, Side::Buy, 100, 1, 0, OrderType::FOK)),
        Err(ExchangeError::FokUnfilled)
    );
    assert_eq!(exchange.state_digest(), before);
    assert_eq!(exchange.accounts[&0.into()].nonce, 0);
}

#[test]
fn sell_reserves_base_only_and_rejects_self_trade() {
    let mut exchange = ExchangeState::default();
    exchange
        .execute(&place(0, Side::Sell, 100_000, 1, 0, OrderType::LO))
        .unwrap();
    assert_eq!(balance(&exchange, 0, BTC), (1_000 * BTC_ATOMS - 1, 1));
    let before = exchange.state_digest();
    assert_eq!(
        exchange.execute(&place(0, Side::Buy, 100_000, 1, 1, OrderType::LO)),
        Err(ExchangeError::SelfTrade)
    );
    assert_eq!(exchange.state_digest(), before);
}

#[test]
fn one_lot_btc_trade_and_tenth_dollar_tick_settle_without_rounding() {
    let mut exchange = ExchangeState::default();
    let market = &exchange.markets[&BTC_USD];
    assert_eq!(market.price.format_u64(1_000_001), "100000.1");
    assert_eq!(market.quantity.format_u64(1), "0.0001");
    assert_eq!(market.quote_atoms(1_000_001, 1), Some(10_000_010));

    let buyer_usd = balance(&exchange, 0, USD).0;
    let buyer_btc = balance(&exchange, 0, BTC).0;
    let seller_usd = balance(&exchange, 1, USD).0;
    let seller_btc = balance(&exchange, 1, BTC).0;
    exchange
        .execute(&place(1, Side::Sell, 1_000_001, 1, 0, OrderType::LO))
        .unwrap();
    assert_eq!(balance(&exchange, 1, BTC), (seller_btc - 1, 1));
    exchange
        .execute(&place(0, Side::Buy, 1_000_001, 1, 0, OrderType::IOC))
        .unwrap();
    assert_eq!(balance(&exchange, 0, USD), (buyer_usd - 10_000_010, 0));
    assert_eq!(balance(&exchange, 1, USD), (seller_usd + 10_000_010, 0));
    assert_eq!(balance(&exchange, 0, BTC), (buyer_btc + 1, 0));
    assert_eq!(balance(&exchange, 1, BTC), (seller_btc - 1, 0));
    assert!(exchange.verify_state_tree());
}
#[test]
fn market_registration_commits_metadata_and_requires_oracle_for_perp() {
    use super::market_config::DecimalStep;

    let mut exchange = ExchangeState::default();
    let original_root = exchange.state_digest();
    let cents = DecimalStep::new(2, 1).unwrap();
    let whole = DecimalStep::new(0, 1).unwrap();
    exchange
        .create_perp_market(1, BTC, USD, cents, whole, 10)
        .unwrap();
    assert_ne!(exchange.state_digest(), original_root);
    assert!(exchange.verify_state_tree());
    assert_eq!(
        exchange.create_perp_market(1, BTC, USD, cents, whole, 10),
        Err(ExchangeError::MarketAlreadyExists)
    );
    assert_eq!(
        exchange.create_perp_market(2, BTC, USD, cents, whole, 0),
        Err(ExchangeError::InvalidMarketConfig)
    );
    let place = Transaction::PlaceOrder {
        account_id: 0.into(),
        market_id: 1,
        order_type: OrderType::LO,
        side: Side::Buy,
        price: 100_00,
        quantity: 1,
        nonce: 0,
    };
    assert_eq!(
        exchange.execute(&place),
        Err(ExchangeError::OracleUnavailable)
    );

    exchange
        .create_spot_market(3, BTC, USD, cents, whole)
        .unwrap();
    let decimal_spot = Transaction::PlaceOrder {
        account_id: 0.into(),
        market_id: 3,
        order_type: OrderType::LO,
        side: Side::Buy,
        price: 100_00,
        quantity: 1,
        nonce: 0,
    };
    assert_eq!(
        exchange.execute(&decimal_spot),
        Err(ExchangeError::UnsupportedMarketSettlement)
    );
}

#[test]
fn perp_order_uses_its_committed_pair_mark() {
    use crate::oracle::{
        OracleObservation, OraclePair, OracleUpdate, Venue, index_from_observations,
    };

    let mut exchange = ExchangeState::default();
    exchange
        .create_perp_market(
            1,
            BTC,
            USD,
            DecimalStep::new(1, 1).unwrap(),
            DecimalStep::new(4, 1).unwrap(),
            10,
        )
        .unwrap();
    let pair = OraclePair::new(BTC, USD);
    let observations = [
        OracleObservation {
            venue: Venue::Binance,
            symbol: "BTCUSDT".into(),
            bid: "99.9".into(),
            ask: "100.1".into(),
            source_sequence: Some(1),
            source_time: None,
        },
        OracleObservation {
            venue: Venue::Okx,
            symbol: "BTC-USDT".into(),
            bid: "99.9".into(),
            ask: "100.1".into(),
            source_sequence: None,
            source_time: None,
        },
        OracleObservation {
            venue: Venue::Coinbase,
            symbol: "BTC-USD".into(),
            bid: "99.9".into(),
            ask: "100.1".into(),
            source_sequence: Some(1),
            source_time: None,
        },
    ];
    let update = OracleUpdate {
        pair,
        index_e8: index_from_observations(pair, &observations).unwrap(),
        observations: observations.to_vec(),
    };
    exchange.apply_oracle_update(&update).unwrap();
    exchange
        .execute(&Transaction::DepositPerpCollateral {
            account_id: 0.into(),
            market_id: 1,
            amount: 100 * USD_ATOMS,
            nonce: 0,
        })
        .unwrap();
    exchange
        .execute(&Transaction::PlaceOrder {
            account_id: 0.into(),
            market_id: 1,
            order_type: OrderType::LO,
            side: Side::Buy,
            price: 1000,
            quantity: 10_000,
            nonce: 1,
        })
        .unwrap();
    assert_eq!(exchange.accounts[&0.into()].nonce, 2);
    assert_eq!(
        exchange.oracle_mark(pair).unwrap().index_e8,
        100 * 100_000_000
    );
    assert!(exchange.verify_state_tree());
}

#[test]
fn cross_pool_backs_resting_orders_in_two_perp_markets_and_withdrawal() {
    use crate::exchange::market::ETH;
    use crate::oracle::{
        OracleObservation, OraclePair, OracleUpdate, Venue, index_from_observations,
    };
    let mut exchange = ExchangeState::default();
    for (market_id, base) in [(1, BTC), (3, ETH)] {
        exchange
            .create_perp_market(
                market_id,
                base,
                USD,
                DecimalStep::new(2, 1).unwrap(),
                DecimalStep::new(4, 1).unwrap(),
                10,
            )
            .unwrap();
        let pair = OraclePair::new(base, USD);
        let name = if base == BTC { "BTC" } else { "ETH" };
        let observations = [
            OracleObservation {
                venue: Venue::Binance,
                symbol: format!("{name}USDT"),
                bid: "99.99".into(),
                ask: "100.01".into(),
                source_sequence: None,
                source_time: None,
            },
            OracleObservation {
                venue: Venue::Okx,
                symbol: format!("{name}-USDT"),
                bid: "99.99".into(),
                ask: "100.01".into(),
                source_sequence: None,
                source_time: None,
            },
            OracleObservation {
                venue: Venue::Coinbase,
                symbol: format!("{name}-USD"),
                bid: "99.99".into(),
                ask: "100.01".into(),
                source_sequence: None,
                source_time: None,
            },
        ];
        exchange
            .apply_oracle_update(&OracleUpdate {
                pair,
                index_e8: index_from_observations(pair, &observations).unwrap(),
                observations: observations.to_vec(),
            })
            .unwrap();
    }
    exchange
        .execute(&Transaction::SetMarginMode {
            account_id: 0.into(),
            mode: MarginMode::Cross,
            nonce: 0,
        })
        .unwrap();
    exchange
        .execute(&Transaction::DepositPerpCollateral {
            account_id: 0.into(),
            market_id: 1,
            amount: 30 * USD_ATOMS,
            nonce: 1,
        })
        .unwrap();
    for (market_id, nonce) in [(1, 2), (3, 3)] {
        exchange
            .execute(&Transaction::PlacePerpOrder {
                account_id: 0.into(),
                market_id,
                order_type: OrderType::LO,
                side: Side::Buy,
                price: 10_000,
                quantity: 10_000,
                reduce_only: false,
                nonce,
            })
            .unwrap();
    }
    assert_eq!(
        exchange.cross_collateral(0.into()),
        30 * i128::from(USD_ATOMS)
    );
    assert_eq!(
        exchange.execute(&Transaction::WithdrawPerpCollateral {
            account_id: 0.into(),
            market_id: 1,
            amount: 11 * USD_ATOMS,
            nonce: 4,
        }),
        Err(ExchangeError::InsufficientBalance)
    );
    exchange
        .execute(&Transaction::WithdrawPerpCollateral {
            account_id: 0.into(),
            market_id: 1,
            amount: 10 * USD_ATOMS,
            nonce: 4,
        })
        .unwrap();
    assert_eq!(
        exchange.cross_collateral(0.into()),
        20 * i128::from(USD_ATOMS)
    );
    assert!(exchange.verify_state_tree());
}

#[test]
fn perp_collateral_deposit_moves_usd_and_commits_both_leaves() {
    let mut exchange = ExchangeState::default();
    exchange
        .create_perp_market(
            1,
            BTC,
            USD,
            DecimalStep::new(1, 1).unwrap(),
            DecimalStep::new(4, 1).unwrap(),
            10,
        )
        .unwrap();
    let address = 0.into();
    let initial_usd = balance(&exchange, 0, USD).0;
    let initial_root = exchange.state_digest();

    exchange
        .execute(&Transaction::DepositPerpCollateral {
            account_id: address,
            market_id: 1,
            amount: USD_ATOMS,
            nonce: 0,
        })
        .unwrap();

    assert_eq!(balance(&exchange, 0, USD), (initial_usd - USD_ATOMS, 0));
    assert_eq!(exchange.accounts[&address].nonce, 1);
    let isolated = &exchange.perp_accounts[&(1, address)];
    assert_eq!(isolated.collateral(), i128::from(USD_ATOMS));
    assert_eq!(isolated.position().size(), 0);
    assert_ne!(exchange.state_digest(), initial_root);
    assert!(exchange.verify_state_tree());
}

#[test]
fn rejected_perp_deposits_leave_wallet_position_nonce_and_root_unchanged() {
    let mut exchange = ExchangeState::default();
    exchange
        .create_perp_market(
            1,
            BTC,
            USD,
            DecimalStep::new(1, 1).unwrap(),
            DecimalStep::new(4, 1).unwrap(),
            10,
        )
        .unwrap();
    let address = 0.into();
    let usd = balance(&exchange, 0, USD);
    let root = exchange.state_digest();
    let cases = [
        (1, 0, 0, ExchangeError::InvalidOrder),
        (0, 1, 0, ExchangeError::UnsupportedMarketSettlement),
        (99, 1, 0, ExchangeError::MarketNotFound),
        (1, usd.0 + 1, 0, ExchangeError::InsufficientBalance),
        (1, USD_ATOMS, 128, ExchangeError::InvalidNonce),
    ];
    for (market_id, amount, nonce, expected) in cases {
        assert_eq!(
            exchange.execute(&Transaction::DepositPerpCollateral {
                account_id: address,
                market_id,
                amount,
                nonce,
            }),
            Err(expected)
        );
        assert_eq!(balance(&exchange, 0, USD), usd);
        assert_eq!(exchange.accounts[&address].nonce, 0);
        assert!(exchange.perp_accounts.is_empty());
        assert_eq!(exchange.state_digest(), root);
        assert!(exchange.verify_state_tree());
    }
}
