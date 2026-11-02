//! Focused single-process perp costs. Fixture cloning is outside each sample.
use light_dex::{
    blockchain::Transaction,
    exchange::{
        ExchangeState,
        account::{Account, Balance},
        market::{BTC, USD},
        market_config::DecimalStep,
        order::{OrderType, Side},
        perp::MarginMode,
    },
    oracle::{OracleObservation, OraclePair, OracleUpdate, Venue, index_from_observations},
};
use std::{collections::HashMap, env, fs, hint::black_box, path::PathBuf, time::Instant};

fn update(bid: &str, ask: &str) -> OracleUpdate {
    let pair = OraclePair::new(BTC, USD);
    let observations = [
        (Venue::Binance, "BTCUSDT"),
        (Venue::Okx, "BTC-USDT"),
        (Venue::Coinbase, "BTC-USD"),
    ]
    .map(|(venue, symbol)| OracleObservation {
        venue,
        symbol: symbol.into(),
        bid: bid.into(),
        ask: ask.into(),
        source_sequence: None,
        source_time: None,
    });
    OracleUpdate {
        pair,
        index_e8: index_from_observations(pair, &observations).unwrap(),
        observations: observations.to_vec(),
    }
}

fn state(
    cross: bool,
    low_margin: bool,
    resting: bool,
    open: bool,
    account_count: u64,
) -> ExchangeState {
    let mut state = if account_count == 100 {
        ExchangeState::default()
    } else {
        ExchangeState::with_accounts((0..account_count).map(|id| {
            Account {
                id: id.into(),
                nonce: 0,
                used_nonces: [0; 2],
                balances: HashMap::from([
                    (
                        USD,
                        Balance {
                            available: 1_000_000_000_000,
                            locked: 0,
                        },
                    ),
                    (
                        BTC,
                        Balance {
                            available: 10_000_000,
                            locked: 0,
                        },
                    ),
                ])
                .into_iter()
                .collect(),
            }
        }))
    };
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
    state.apply_oracle_update(&update("999", "1001")).unwrap();
    if cross {
        state
            .execute(&Transaction::SetMarginMode {
                account_id: 0.into(),
                mode: MarginMode::Cross,
                nonce: 0,
            })
            .unwrap();
    }
    for id in 0..3 {
        let amount = if low_margin && id == 0 {
            100_000_000
        } else {
            100_000_000_000
        };
        state
            .execute(&Transaction::DepositPerpCollateral {
                account_id: id.into(),
                market_id: 1,
                amount,
                nonce: if cross && id == 0 { 1 } else { 0 },
            })
            .unwrap();
    }
    if resting || open {
        state
            .execute(&Transaction::PlacePerpOrder {
                account_id: 1.into(),
                market_id: 1,
                order_type: OrderType::LO,
                side: Side::Sell,
                price: 10_000,
                quantity: 10_000,
                reduce_only: false,
                nonce: 1,
            })
            .unwrap();
    }
    if open {
        state
            .execute(&Transaction::PlacePerpOrder {
                account_id: 0.into(),
                market_id: 1,
                order_type: OrderType::FOK,
                side: Side::Buy,
                price: 10_000,
                quantity: 10_000,
                reduce_only: false,
                nonce: if cross { 2 } else { 1 },
            })
            .unwrap();
    }
    if low_margin && open {
        state.apply_oracle_update(&update("699", "701")).unwrap();
    }
    state
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = env::args().collect::<Vec<_>>();
    let samples: usize = args
        .windows(2)
        .find(|arg| arg[0] == "--samples")
        .and_then(|arg| arg[1].parse().ok())
        .unwrap_or(200);
    if !(1..=10_000).contains(&samples) {
        return Err("samples must be 1–10000".into());
    }
    let out = args
        .windows(2)
        .find(|arg| arg[0] == "--out")
        .map(|arg| PathBuf::from(&arg[1]));
    let account_count: u64 = args
        .windows(2)
        .find(|arg| arg[0] == "--accounts")
        .and_then(|arg| arg[1].parse().ok())
        .unwrap_or(100);
    if !(3..=100_000).contains(&account_count) {
        return Err("accounts must be 3–100000".into());
    }
    let only = args
        .windows(2)
        .find(|arg| arg[0] == "--only")
        .map(|arg| arg[1].as_str());
    let mut csv = String::from("operation,samples,median_ns,p95_ns\n");
    for operation in [
        "isolated_passive_lo",
        "isolated_crossing_fok",
        "cross_margin_crossing_fok",
        "perp_cancel",
        "perp_risk_snapshot",
        "perp_state_digest",
        "oracle_update",
        "partial_liquidation",
    ] {
        if only.is_some_and(|requested| requested != operation) {
            continue;
        }
        let base = match operation {
            "isolated_crossing_fok" | "perp_cancel" => {
                state(false, false, true, false, account_count)
            }
            "cross_margin_crossing_fok" => state(true, false, true, false, account_count),
            "perp_risk_snapshot" => state(true, false, false, true, account_count),
            "partial_liquidation" => state(false, true, false, true, account_count),
            _ => state(false, false, false, false, account_count),
        };
        let mut times = Vec::with_capacity(samples);
        for _ in 0..samples {
            let mut fixture = base.clone();
            let nonce = fixture.get_nonce_for_account(0.into()).unwrap();
            let started = Instant::now();
            match operation {
                "isolated_passive_lo" => fixture
                    .execute(&Transaction::PlacePerpOrder {
                        account_id: 0.into(),
                        market_id: 1,
                        order_type: OrderType::LO,
                        side: Side::Buy,
                        price: 9_900,
                        quantity: 10_000,
                        reduce_only: false,
                        nonce,
                    })
                    .unwrap(),
                "isolated_crossing_fok" | "cross_margin_crossing_fok" => fixture
                    .execute(&Transaction::PlacePerpOrder {
                        account_id: 0.into(),
                        market_id: 1,
                        order_type: OrderType::FOK,
                        side: Side::Buy,
                        price: 10_000,
                        quantity: 10_000,
                        reduce_only: false,
                        nonce,
                    })
                    .unwrap(),
                "perp_cancel" => fixture
                    .execute(&Transaction::CancelOrder {
                        account_id: 1.into(),
                        market_id: 1,
                        order_id: 0,
                        nonce: 2,
                    })
                    .unwrap(),
                "perp_risk_snapshot" => {
                    black_box(fixture.perp_risk_snapshots());
                }
                "perp_state_digest" => {
                    black_box(fixture.state_digest());
                }
                "oracle_update" => fixture
                    .apply_oracle_update(&update("1000", "1002"))
                    .unwrap(),
                "partial_liquidation" => fixture
                    .execute(&Transaction::LiquidatePerp {
                        liquidator: 2.into(),
                        target: 0.into(),
                        market_id: 1,
                        quantity: 5_000,
                        nonce: 1,
                    })
                    .unwrap(),
                _ => unreachable!(),
            }
            times.push(started.elapsed().as_nanos());
        }
        times.sort_unstable();
        let median = times[samples / 2];
        let p95 = times[((samples - 1) as f64 * 0.95).ceil() as usize];
        csv.push_str(&format!("{operation},{samples},{median},{p95}\n"));
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
