# Light DEX control plane

Run `npm run tauri dev` here, then launch a node cluster in the Control plane tab. The Participants tab contains three Rust-backed behaviors:

- **Market maker:** reads Binance Spot's public `btcusdt@depth5@100ms` partial-book stream and refreshes its BTC-USD quotes at the configured interval. It posts one BTC at each requested bid and ask level (1–5 levels per side), cancelling stale prices before replacing them. Updating the settings keeps the same account; deactivating cancels its remaining quotes. The default account is 90. Use an account with no existing open orders.
- **Random transactions:** sends transfers among multiple accounts, either random pairs or a round-robin rotation. The target TPS is an attempted send rate, not a guaranteed commit rate. The single-account selected-transaction preset was removed. Multiple sender accounts allow several transactions to enter one block.
- **Taker:** repeatedly sends a one-BTC IOC order at the best visible opposite price. The default account is 80. Taker and random transfers share one sender slot; starting one replaces the other.

The market maker's strategy lives in the root Rust library at `../src/participants/market_maker.rs`; its feed and execution lifecycle live in `src-tauri/src/binance_depth.rs` and `src-tauri/src/controller.rs`. TypeScript only configures and displays participants.

This is a simulation. Binance BTCUSDT is treated as nominal BTC-USD, not converted through an FX feed. The chain uses whole-dollar integer prices, so Binance's finer price levels are mapped to distinct $1 ticks; displayed quotes do not reproduce the external book cent-for-cent. The feed is public market data and no Binance trading credentials are used. A stale feed clears desired quotes and cancels existing ones. Quote transaction failures stop the maker and surface an error so remaining orders can be inspected.

Verification:

```bash
cargo test --manifest-path ../Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml market_maker_quotes_live_depth_and_cancels_on_stop -- --ignored
npm run build
```

The ignored market-maker test requires Binance connectivity and a previously built `../target/debug/light_dex` node binary.
