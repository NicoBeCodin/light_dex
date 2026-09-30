# Light DEX

Light DEX is a local Rust blockchain and order-book simulator with a Tauri 2 + React control plane. It is an experimental project, not a production exchange or a place to trade real funds.

The root crate runs the nodes and implements transactions, consensus, and matching. `control-plane/` launches and monitors a local cluster; its TypeScript protocol types are generated from Rust with `ts-rs`.

## Run locally

You need Rust/Cargo, Node.js/npm, and the [Tauri 2 system prerequisites](https://v2.tauri.app/start/prerequisites/) for your OS.

```bash
cargo test
cd control-plane
npm ci
npm run tauri dev
```

In the app, select **Launch** to build the node binary and start the default seven-node cluster. You can submit transfers and orders, inspect the BTC-USD order book, pause or kill nodes, and start or adjust simulated market participants. The market maker reads Binance's public BTCUSDT depth feed, so it needs network access; it does not place trades on Binance.

The cluster writes local runtime state to `benchmark_results/control-plane-state/`, which is ignored by Git. For participant behavior, limitations, and verification commands, see the [control-plane README](control-plane/README.md).
