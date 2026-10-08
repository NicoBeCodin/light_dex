//! Deterministic, single-process ExchangeState scaling benchmark.
//! No nodes, network, persistence, or Binance feed are involved.
use light_dex::{
    blockchain::Transaction,
    exchange::{
        ExchangeState,
        account::{Account, Balance},
        market::{BTC, BTC_USD, USD},
        order::{Order, OrderType, Side},
    },
};
use std::{
    env, fs,
    hint::black_box,
    path::PathBuf,
    time::{Duration, Instant},
};

struct Row {
    group: &'static str,
    operation: &'static str,
    accounts: usize,
    book_orders: usize,
    fills: usize,
    queue_position: &'static str,
    samples: usize,
    p50_ns: u128,
    p95_ns: u128,
    p99_ns: u128,
    mean_ns: u128,
}

#[derive(Clone, Copy, Debug)]
struct FixtureConfig {
    accounts: usize,
    resting_orders: usize,
    price_levels: usize,
    history_transfers: usize,
    history_trades: usize,
}

/// Build exactly `accounts` funded accounts and `resting_orders` live asks.
/// Historical trades are executed before measuring, so they do not reduce the
/// requested final book size. Account 0 is reserved as the crossing-order taker.
fn make_fixture(config: FixtureConfig) -> Result<ExchangeState, String> {
    if !(2..=100_000).contains(&config.accounts) {
        return Err("accounts must be between 2 and 100000".into());
    }
    if !(1..=1_000_000).contains(&config.price_levels) {
        return Err("price-levels must be between 1 and 1000000".into());
    }
    if config.resting_orders > 0 && config.price_levels > config.resting_orders {
        return Err("price-levels cannot exceed resting-orders".into());
    }
    if config.history_transfers > 1_000_000 || config.history_trades > 100_000 {
        return Err(
            "history-transfers must be at most 1000000 and history-trades at most 100000".into(),
        );
    }
    let total_orders = config
        .resting_orders
        .checked_add(config.history_trades)
        .ok_or("order count overflow")?;
    if total_orders > 1_000_000 {
        return Err("resting-orders plus history-trades must be at most 1000000".into());
    }

    // Fund the synthetic genesis enough that fixture size, not balances,
    // controls the workload. The constructor commits all accounts atomically.
    let mut state = make_accounts_with_balances(config.accounts, 1_000_000_000_000, 1_000_000_000);

    for index in 0..config.history_transfers {
        let from = (index % config.accounts) as u64;
        let nonce = state
            .get_nonce_for_account(from.into())
            .map_err(|e| format!("{e:?}"))?;
        state
            .execute(&Transaction::Transfer {
                from: from.into(),
                to: ((from + 1) % config.accounts as u64).into(),
                asset: USD,
                amount: 1,
                nonce,
            })
            .map_err(|e| format!("fixture transfer: {e:?}"))?;
    }

    // Round trips leave account/nonce/trade history without consuming any of
    // the orders requested for the final fixture.
    for index in 0..config.history_trades {
        let seller = 1 + (index % (config.accounts - 1)) as u64;
        let seller_nonce = state
            .get_nonce_for_account(seller.into())
            .map_err(|e| format!("{e:?}"))?;
        state
            .execute(&place(
                seller,
                Side::Sell,
                100,
                1,
                seller_nonce,
                OrderType::LO,
            ))
            .map_err(|e| format!("fixture maker order: {e:?}"))?;
        let buyer_nonce = state
            .get_nonce_for_account(0.into())
            .map_err(|e| format!("{e:?}"))?;
        let fills = state
            .execute_with_receipt(&place(0, Side::Buy, 100, 1, buyer_nonce, OrderType::IOC))
            .map_err(|e| format!("fixture trade: {e:?}"))?;
        if fills.len() != 1 {
            return Err("fixture trade did not make one fill".into());
        }
    }

    for index in 0..config.resting_orders {
        let seller = 1 + (index % (config.accounts - 1)) as u64;
        let nonce = state
            .get_nonce_for_account(seller.into())
            .map_err(|e| format!("{e:?}"))?;
        let price = 100 + (index % config.price_levels) as u64;
        state
            .execute(&place(seller, Side::Sell, price, 1, nonce, OrderType::LO))
            .map_err(|e| format!("fixture resting order: {e:?}"))?;
    }

    let actual_orders: usize = state.order_books()[&BTC_USD]
        .asks
        .values()
        .map(|orders| orders.len())
        .sum();
    if actual_orders != config.resting_orders {
        return Err(format!(
            "fixture has {actual_orders} orders, expected {}",
            config.resting_orders
        ));
    }
    let actual_levels = state.order_books()[&BTC_USD].asks.len();
    let expected_levels = config.price_levels.min(config.resting_orders);
    if actual_levels != expected_levels {
        return Err(format!(
            "fixture has {actual_levels} levels, expected {expected_levels}"
        ));
    }
    Ok(state)
}

fn measure(
    rows: &mut Vec<Row>,
    group: &'static str,
    operation: &'static str,
    accounts: usize,
    book_orders: usize,
    fills: usize,
    queue_position: &'static str,
    samples: usize,
    mut sample: impl FnMut() -> Duration,
) {
    for _ in 0..10.min(samples) {
        black_box(sample());
    }
    let mut times = (0..samples)
        .map(|_| sample().as_nanos())
        .collect::<Vec<_>>();
    times.sort_unstable();
    let percentile = |p: f64| times[((times.len() - 1) as f64 * p).ceil() as usize];
    rows.push(Row {
        group,
        operation,
        accounts,
        book_orders,
        fills,
        queue_position,
        samples,
        p50_ns: percentile(0.50),
        p95_ns: percentile(0.95),
        p99_ns: percentile(0.99),
        mean_ns: times.iter().sum::<u128>() / times.len() as u128,
    });
}

fn make_accounts(count: usize) -> ExchangeState {
    make_accounts_with_balances(count, 1_000_000, 1_000)
}

fn make_accounts_with_balances(count: usize, usd: u64, btc: u64) -> ExchangeState {
    ExchangeState::with_accounts((0..count as u64).map(|id| Account {
                id: id.into(),
                nonce: 0,
                balances: std::collections::HashMap::from([
                    (
                        USD,
                        Balance {
                            available: usd,
                            locked: 0,
                        },
                    ),
                    (
                        BTC,
                        Balance {
                            available: btc,
                            locked: 0,
                        },
                    ),
                ]),
            }))
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

fn make_asks(count: usize, single_price: bool) -> ExchangeState {
    let mut state = ExchangeState::default();
    for index in 0..count {
        let account = 1 + (index % 99) as u64;
        let nonce = state.accounts()[&account.into()].nonce;
        let price = if single_price {
            100
        } else {
            100 + index as u64
        };
        state
            .execute(&place(account, Side::Sell, price, 1, nonce, OrderType::LO))
            .unwrap();
    }
    state
}

fn run(samples: usize) -> Vec<Row> {
    let mut rows = Vec::new();

    // The account map grows while the touched-account set stays at two.
    for accounts in [100, 1_000, 10_000, 100_000] {
        let base = make_accounts(accounts);
        let count = if accounts >= 100_000 {
            samples.min(60)
        } else {
            samples
        };
        let tx = Transaction::Transfer {
            from: 0.into(),
            to: 1.into(),
            asset: USD,
            amount: 1,
            nonce: 0,
        };
        measure(
            &mut rows,
            "accounts",
            "transfer",
            accounts,
            0,
            0,
            "na",
            count,
            || {
                let mut state = base.clone();
                let start = Instant::now();
                black_box(state.execute(black_box(&tx)).unwrap());
                start.elapsed()
            },
        );
        measure(
            &mut rows,
            "accounts",
            "state_digest",
            accounts,
            0,
            0,
            "na",
            count,
            || {
                let start = Instant::now();
                black_box(base.state_digest());
                start.elapsed()
            },
        );
        measure(
            &mut rows,
            "accounts",
            "state_clone",
            accounts,
            0,
            0,
            "na",
            count,
            || {
                let start = Instant::now();
                black_box(base.clone());
                start.elapsed()
            },
        );
    }
    // Unique price levels exercise the BTreeMap, resting-order lookup and digest.
    for depth in [0, 100, 1_000, 10_000] {
        let base = make_asks(depth, false);
        let count = if depth >= 10_000 {
            samples.min(100)
        } else {
            samples
        };
        let passive = place(0, Side::Buy, 50, 1, 0, OrderType::LO);
        let crossing = place(0, Side::Buy, 100, 1, 0, OrderType::IOC);
        for (operation, tx) in [("passive_lo", &passive), ("crossing_ioc_1_fill", &crossing)] {
            if depth == 0 && operation == "crossing_ioc_1_fill" {
                continue;
            }
            measure(
                &mut rows,
                "price_levels",
                operation,
                100,
                depth,
                usize::from(operation == "crossing_ioc_1_fill"),
                "na",
                count,
                || {
                    let mut state = base.clone();
                    let start = Instant::now();
                    let trades = state.execute_with_receipt(black_box(tx)).unwrap();
                    black_box(trades);
                    start.elapsed()
                },
            );
        }
        measure(
            &mut rows,
            "price_levels",
            "state_digest",
            100,
            depth,
            0,
            "na",
            count,
            || {
                let start = Instant::now();
                black_box(base.state_digest());
                start.elapsed()
            },
        );
        measure(
            &mut rows,
            "price_levels",
            "state_clone",
            100,
            depth,
            0,
            "na",
            count,
            || {
                let start = Instant::now();
                black_box(base.clone());
                start.elapsed()
            },
        );
    }

    // One price level, growing FIFO queue; the cancellation target changes position.
    for depth in [10, 100, 1_000, 10_000] {
        let base = make_asks(depth, true);
        let count = if depth >= 10_000 {
            samples.min(100)
        } else {
            samples
        };
        for (position, index) in [("front", 0), ("middle", depth / 2), ("back", depth - 1)] {
            let account = 1 + (index % 99) as u64;
            let tx = Transaction::CancelOrder {
                account_id: account.into(),
                market_id: BTC_USD,
                order_id: index as u64,
                nonce: base.accounts()[&account.into()].nonce,
            };
            measure(
                &mut rows,
                "queue",
                "cancel",
                100,
                depth,
                0,
                position,
                count,
                || {
                    let mut state = base.clone();
                    let start = Instant::now();
                    black_box(state.execute(black_box(&tx)).unwrap());
                    start.elapsed()
                },
            );
            measure(
                &mut rows,
                "queue",
                "book_only_cancel",
                100,
                depth,
                0,
                position,
                count,
                || {
                    let mut book = base.order_books()[&BTC_USD].clone();
                    let start = Instant::now();
                    black_box(
                        book.cancel_order(index as u64, Side::Sell, 100, account.into())
                            .unwrap(),
                    );
                    start.elapsed()
                },
            );
        }
    }

    // Matching across multiple makers includes preview, balance settlement and book mutation.
    for fills in [1, 10, 50, 100, 500] {
        let base = make_asks(fills, true);
        let count = if fills >= 500 {
            samples.min(100)
        } else {
            samples
        };
        for kind in [OrderType::IOC, OrderType::FOK] {
            let name = if kind == OrderType::IOC {
                "match_ioc"
            } else {
                "match_fok"
            };
            let tx = place(0, Side::Buy, 100, fills as u64, 0, kind);
            measure(
                &mut rows,
                "fills",
                name,
                100,
                fills,
                fills,
                "na",
                count,
                || {
                    let mut state = base.clone();
                    let start = Instant::now();
                    let trades = state.execute_with_receipt(black_box(&tx)).unwrap();
                    let elapsed = start.elapsed();
                    assert_eq!(trades.len(), fills);
                    black_box(trades);
                    elapsed
                },
            );
        }
        let order = Order {
            id: fills as u64,
            account: 0.into(),
            market: BTC_USD,
            side: Side::Buy,
            price: 100,
            original_quantity: fills as u64,
            remaining_quantity: fills as u64,
        };
        measure(
            &mut rows,
            "fills",
            "book_only_ioc",
            100,
            fills,
            fills,
            "na",
            count,
            || {
                let mut book = base.order_books()[&BTC_USD].clone();
                let start = Instant::now();
                black_box(
                    book.apply_order(black_box(order.clone()), OrderType::IOC)
                        .unwrap(),
                );
                start.elapsed()
            },
        );
        measure(
            &mut rows,
            "fills",
            "preview_only_ioc",
            100,
            fills,
            fills,
            "na",
            count,
            || {
                let start = Instant::now();
                black_box(
                    base.order_books()[&BTC_USD]
                        .preview_order(black_box(&order), OrderType::IOC)
                        .unwrap(),
                );
                start.elapsed()
            },
        );
    }

    // Long-lived execution, rather than one transaction on a cloned fixture.
    for (operation, iterations) in [("transfer_stream", 200_000), ("passive_lo_stream", 20_000)] {
        let mut state = ExchangeState::default();
        let start = Instant::now();
        for index in 0..iterations {
            let account = (index % 100) as u64;
            let nonce = state.accounts()[&account.into()].nonce;
            let tx = if operation == "transfer_stream" {
                Transaction::Transfer {
                    from: account.into(),
                    to: ((account + 1) % 100).into(),
                    asset: USD,
                    amount: 1,
                    nonce,
                }
            } else {
                place(
                    account,
                    Side::Sell,
                    100 + index as u64,
                    1,
                    nonce,
                    OrderType::LO,
                )
            };
            black_box(state.execute(black_box(&tx)).unwrap());
        }
        let elapsed = start.elapsed().as_nanos();
        let per_op = elapsed / iterations as u128;
        rows.push(Row {
            group: "stream",
            operation,
            accounts: 100,
            book_orders: if operation == "passive_lo_stream" {
                iterations
            } else {
                0
            },
            fills: 0,
            queue_position: "na",
            samples: iterations,
            p50_ns: 0,
            p95_ns: 0,
            p99_ns: 0,
            mean_ns: per_op,
        });
    }
    rows
}

fn run_custom(config: FixtureConfig, samples: usize) -> Result<Vec<Row>, String> {
    let base = make_fixture(config)?;
    let count = samples;
    let mut rows = Vec::new();
    let transfer = Transaction::Transfer {
        from: 0.into(),
        to: 1.into(),
        asset: USD,
        amount: 1,
        nonce: base
            .get_nonce_for_account(0.into())
            .map_err(|e| format!("{e:?}"))?,
    };
    let passive = place(0, Side::Buy, 1, 1, transfer.nonce(), OrderType::LO);

    for (operation, tx) in [("transfer", &transfer), ("passive_lo", &passive)] {
        measure(
            &mut rows,
            "custom",
            operation,
            config.accounts,
            config.resting_orders,
            0,
            "na",
            count,
            || {
                let mut state = base.clone();
                let start = Instant::now();
                black_box(state.execute_with_receipt(black_box(tx)).unwrap());
                start.elapsed()
            },
        );
    }

    if let Some((&best_ask, level)) = base.order_books()[&BTC_USD].asks.first_key_value() {
        let crossing = place(0, Side::Buy, best_ask, 1, transfer.nonce(), OrderType::IOC);
        measure(
            &mut rows,
            "custom",
            "crossing_ioc_1_fill",
            config.accounts,
            config.resting_orders,
            1,
            "na",
            count,
            || {
                let mut state = base.clone();
                let start = Instant::now();
                let fills = state.execute_with_receipt(black_box(&crossing)).unwrap();
                let elapsed = start.elapsed();
                assert_eq!(fills.len(), 1);
                black_box(fills);
                elapsed
            },
        );

        let order = level.back().expect("nonempty price level");
        let cancel = Transaction::CancelOrder {
            account_id: order.account,
            market_id: BTC_USD,
            order_id: order.id,
            nonce: base
                .get_nonce_for_account(order.account)
                .map_err(|e| format!("{e:?}"))?,
        };
        measure(
            &mut rows,
            "custom",
            "cancel_best_level_back",
            config.accounts,
            config.resting_orders,
            0,
            "back",
            count,
            || {
                let mut state = base.clone();
                let start = Instant::now();
                black_box(state.execute(black_box(&cancel)).unwrap());
                start.elapsed()
            },
        );
    }

    measure(
        &mut rows,
        "custom",
        "state_digest",
        config.accounts,
        config.resting_orders,
        0,
        "na",
        count,
        || {
            let start = Instant::now();
            black_box(base.state_digest());
            start.elapsed()
        },
    );
    measure(
        &mut rows,
        "custom",
        "state_clone",
        config.accounts,
        config.resting_orders,
        0,
        "na",
        count,
        || {
            let start = Instant::now();
            black_box(base.clone());
            start.elapsed()
        },
    );
    Ok(rows)
}

fn optional_usize(args: &[String], flag: &str) -> Result<Option<usize>, String> {
    let Some(index) = args.iter().position(|arg| arg == flag) else {
        return Ok(None);
    };
    let value = args
        .get(index + 1)
        .ok_or_else(|| format!("missing value for {flag}"))?;
    value
        .parse()
        .map(Some)
        .map_err(|_| format!("invalid integer for {flag}: {value}"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = env::args().collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "Usage: exchange_scaling_bench [--samples N] [--out FILE] [--accounts N --resting-orders K [--price-levels L] [--history-transfers T] [--history-trades F]]\nWithout fixture flags, runs the original scaling sweep. Custom mode benchmarks one exact-size fixture. Account 0 is the taker; the other accounts place one-unit resting asks."
        );
        return Ok(());
    }
    let out = args
        .windows(2)
        .find(|pair| pair[0] == "--out")
        .map(|pair| PathBuf::from(&pair[1]));
    let samples = optional_usize(&args, "--samples")?.unwrap_or(300);
    if !(20..=2_000).contains(&samples) {
        return Err("samples must be 20–2000".into());
    }
    let accounts = optional_usize(&args, "--accounts")?;
    let resting_orders = optional_usize(&args, "--resting-orders")?;
    let price_levels = optional_usize(&args, "--price-levels")?;
    let history_transfers = optional_usize(&args, "--history-transfers")?;
    let history_trades = optional_usize(&args, "--history-trades")?;
    let custom = accounts.is_some()
        || resting_orders.is_some()
        || price_levels.is_some()
        || history_transfers.is_some()
        || history_trades.is_some();
    let rows = if custom {
        let resting_orders = resting_orders.unwrap_or(0);
        let history_trades = history_trades.unwrap_or(0);
        run_custom(
            FixtureConfig {
                accounts: accounts.unwrap_or(100),
                resting_orders,
                price_levels: price_levels
                    .unwrap_or(resting_orders.saturating_add(history_trades).max(1)),
                history_transfers: history_transfers.unwrap_or(0),
                history_trades,
            },
            samples,
        )?
    } else {
        run(samples)
    };
    let mut csv = String::from(
        "group,operation,accounts,book_orders,fills,queue_position,samples,p50_ns,p95_ns,p99_ns,mean_ns\n",
    );
    for row in rows {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{}\n",
            row.group,
            row.operation,
            row.accounts,
            row.book_orders,
            row.fills,
            row.queue_position,
            row.samples,
            row.p50_ns,
            row.p95_ns,
            row.p99_ns,
            row.mean_ns
        ));
    }
    if let Some(path) = out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, csv)?;
        eprintln!("Wrote {}", path.display());
    } else {
        print!("{csv}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_has_exact_requested_size_after_history() {
        let config = FixtureConfig {
            accounts: 12,
            resting_orders: 37,
            price_levels: 4,
            history_transfers: 25,
            history_trades: 7,
        };
        let state = make_fixture(config).unwrap();
        assert_eq!(state.accounts().len(), 12);
        assert_eq!(state.order_books()[&BTC_USD].asks.len(), 4);
        assert_eq!(
            state.order_books()[&BTC_USD]
                .asks
                .values()
                .map(|orders| orders.len())
                .sum::<usize>(),
            37
        );
        assert_eq!(state.accounts()[&0.into()].nonce, 10);
        assert_eq!(state.next_order_id(), 51); // Two order IDs per historical match.
        assert_eq!(run_custom(config, 20).unwrap().len(), 6);
    }

    #[test]
    fn empty_book_and_invalid_level_count() {
        let empty = FixtureConfig {
            accounts: 2,
            resting_orders: 0,
            price_levels: 1,
            history_transfers: 0,
            history_trades: 3,
        };
        assert_eq!(run_custom(empty, 20).unwrap().len(), 4);
        assert!(
            make_fixture(FixtureConfig {
                resting_orders: 2,
                price_levels: 3,
                ..empty
            })
            .is_err()
        );
    }
}
