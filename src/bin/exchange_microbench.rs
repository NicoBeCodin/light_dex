//! Isolated exchange-operation costs; does not start a cluster or use Binance.
use light_dex::{
    blockchain::Transaction,
    exchange::{
        ExchangeState,
        order::{OrderType, Side},
    },
};
use std::{env, fs, path::PathBuf, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = env::args().collect::<Vec<_>>();
    let out = args
        .windows(2)
        .find(|pair| pair[0] == "--out")
        .map(|pair| PathBuf::from(&pair[1]));
    let samples: usize = args
        .windows(2)
        .find(|pair| pair[0] == "--samples")
        .and_then(|pair| pair[1].parse().ok())
        .unwrap_or(20);
    if samples == 0 || samples > 1000 {
        return Err("samples must be 1–1000".into());
    }
    let mut csv = String::from("operation,book_orders,samples,median_ns,p95_ns\n");
    for depth in [0, 25, 100, 250] {
        let mut base = ExchangeState::default();
        for index in 0..depth {
            let account = 1 + index % 50;
            let nonce = base
                .get_nonce_for_account(account.into())
                .map_err(|error| format!("{error:?}"))?;
            base.execute(&Transaction::PlaceOrder {
                account_id: account.into(),
                market_id: 0,
                order_type: OrderType::LO,
                side: Side::Sell,
                price: 90_000 + index,
                quantity: 1,
                nonce,
            })
            .map_err(|error| format!("{error:?}"))?;
        }
        for operation in [
            "transfer",
            "passive_lo",
            "crossing_ioc",
            "crossing_fok",
            "cancel",
            "state_digest",
        ] {
            if operation == "cancel" && depth == 0 {
                continue;
            }
            let mut times = Vec::with_capacity(samples);
            for _ in 0..samples {
                let mut state = base.clone(); // Fixture cloning is outside the timed operation.
                let started = Instant::now();
                if operation == "state_digest" {
                    std::hint::black_box(state.state_digest());
                } else {
                    let transaction = match operation {
                        "transfer" => Transaction::Transfer {
                            from: 0.into(),
                            to: 1.into(),
                            asset: 0,
                            amount: 1,
                            nonce: 0,
                        },
                        "passive_lo" => Transaction::PlaceOrder {
                            account_id: 0.into(),
                            market_id: 0,
                            order_type: OrderType::LO,
                            side: Side::Buy,
                            price: 80_000,
                            quantity: 1,
                            nonce: 0,
                        },
                        "crossing_ioc" | "crossing_fok" => Transaction::PlaceOrder {
                            account_id: 0.into(),
                            market_id: 0,
                            order_type: if operation == "crossing_ioc" {
                                OrderType::IOC
                            } else {
                                OrderType::FOK
                            },
                            side: Side::Buy,
                            price: 90_000,
                            quantity: 1,
                            nonce: 0,
                        },
                        "cancel" => Transaction::CancelOrder {
                            account_id: 1.into(),
                            market_id: 0,
                            order_id: 0,
                            nonce: state
                                .get_nonce_for_account(1.into())
                                .map_err(|error| format!("{error:?}"))?,
                        },
                        _ => unreachable!(),
                    };
                    // The zero-depth crossing tests measure a valid empty-book IOC,
                    // while FOK needs resting liquidity and is skipped below.
                    if !(operation == "crossing_fok" && depth == 0) {
                        state
                            .execute(&transaction)
                            .map_err(|error| format!("{error:?}"))?;
                    }
                }
                times.push(started.elapsed().as_nanos());
            }
            if operation == "crossing_fok" && depth == 0 {
                continue;
            }
            times.sort_unstable();
            let median = times[times.len() / 2];
            let p95 = times[((times.len() - 1) as f64 * 0.95).ceil() as usize];
            csv.push_str(&format!("{operation},{depth},{samples},{median},{p95}\n"));
        }
    }
    if let Some(path) = out {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, csv)?;
    } else {
        print!("{csv}");
    }
    Ok(())
}
