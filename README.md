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

In the app, choose PBFT (default) or experimental HotStuff, then select **Launch** to build the node binary and start the default seven-node cluster. You can submit transfers and orders, inspect the BTC-USD order book, pause or kill nodes, and start or adjust simulated market participants. The market maker reads Binance's public BTCUSDT depth feed, so it needs network access; it does not place trades on Binance.

The cluster writes local runtime state to `benchmark_results/control-plane-state/`, which is ignored by Git. Each launch creates a fresh Ed25519 session with a public `manifest.json`, restricted validator/account keys under `keys/`, a controller-only `wallet.json` mapping numeric UI labels to public-key addresses, and certified commits in append-only `node-N.json.journal` files. Protocol v2 uses 32-byte Ed25519 public keys as account addresses throughout transactions, balances, nonces, orders, and state commitments. A transfer to a valid new address creates its initially empty on-chain account. The controller can create an unfunded address or use **Airdrop new address** to generate one and submit a funding transfer. Numbers are local wallet labels, never account identities on chain. Nodes check transaction, validator-message, quorum-certificate, and catch-up signatures. Current v2 signed sessions can be replayed with `BlockchainState::load_signed(path, manifest)`; live restart is disabled because votes and locks are not durably journaled. Pre-v2 signed and unsigned archives are not supported by this address migration and are left untouched. The Participants tab can launch, tune, and stop fleets of Binance-observing market makers, local-book trend followers, and random order agents. See the [control-plane README](control-plane/README.md) for usage and limitations. The older `scenarios/` JSON examples are retained as reference inputs.

New signed blocks use versioned incremental Merkle state roots with address-keyed account leaves. Do not run old and new node binaries in the same cluster: protocol v2 changes transaction encoding, genesis state, and consensus hashes; mixed-version voting is unsupported.

Signed protocol-v2 sessions use a `light-dex/block/v3` hash domain and state-root version 2, commit to their chain ID and transaction signatures, and store a PBFT or HotStuff commit certificate beside each block. This is a major Light DEX protocol update, not a separate project or chain product; existing session files are intentionally not migrated. PBFT view changes carry signed requests; HotStuff QCs and new-view messages carry signed votes. Signatures authenticate evidence, but this simulator does not yet implement durable consensus safety state or a full PBFT prepared-certificate view-change protocol. It should not be treated as a production Byzantine fault tolerant exchange. All local processes currently run under the same OS user, so file permissions prevent accidental exposure and Git commits but do not isolate a compromised local process from other session keys.

The [signature protocol notes and focused timings](research/signatures_2026-10-08.md) describe the preceding signed format; rerun the optional Ed25519 microbenchmark for v2 with `cargo run --release --bin signature_microbench -- 1000` before quoting current timing figures.

## Benchmarks

The opt-in headless runner starts the same live 13-agent fleet as the control plane, then varies consensus, node count, timeout and extra offered load. It never runs as part of `cargo test` or normal app startup. It requires network access to Binance's public depth feed and stores per-trial chains and raw measurements under the ignored `benchmark_results/runs/` directory.

```bash
cargo build --release --bin light_dex --bin exchange_microbench --bin exchange_scaling_bench
cargo build --release --manifest-path control-plane/src-tauri/Cargo.toml --bin light-dex-bench
./control-plane/src-tauri/target/release/light-dex-bench smoke
./control-plane/src-tauri/target/release/light-dex-bench pilot
./control-plane/src-tauri/target/release/light-dex-bench stress-high --workload transfer --in-flight-per-account 4
./target/release/exchange_microbench --out benchmark_results/exchange_microbench.csv
./target/release/exchange_scaling_bench --samples 500 --out benchmark_results/exchange_scaling.csv
python3 benchmarks/render.py --summary benchmark_results/runs/<pilot-id>/summary.json --supplement benchmark_results/runs/<load-id>/summary.json --micro benchmark_results/exchange_microbench.csv --out benchmark_results/figures
```

The pilot is a roughly 20–30 minute, local-only experiment; use `--measure-sec` and `--warmup-sec` to adjust trial lengths. The mixed-order workload keeps one transaction in flight per account because its orders depend on changing book state. `--workload transfer` permits a bounded nonce pipeline (`--in-flight-per-account 1..32`) for cleaner consensus throughput stress. The runner also offers `stress`, `timeout-probe`, and `profile` modes. For an isolated capacity trial with more participants and a growing order book, use for example:

```bash
./control-plane/src-tauri/target/release/light-dex-bench capacity --algorithm hotstuff --nodes 20 --participants 30 --workload resting --in-flight-per-account 8 --offered-tps 200 --warmup-sec 25 --measure-sec 90
```

`capacity` also accepts `--workload transfer` or `mixed`; its validity checks require the driver to deliver at least 95% of the target and every node to remain responsive during measurement. See the [benchmark methodology and curated figures](research/benchmarks/part4/README.md) for results, interpretation, and limitations.

Those earlier node-throughput figures were measured before validator and account signatures were added. Use them as historical baselines; rerun the opt-in trials before claiming capacity for the signed protocol. The focused signature timings above do not substitute for end-to-end throughput tests.

The separate [exchange-state scaling results](research/benchmarks/part4/exchange_scaling_2026-10-04.md) compare account counts, resting-book size, FIFO cancellation positions, and multi-fill execution without starting nodes or accessing Binance.

To measure one tailored exchange fixture instead of the built-in sweep, use:

```bash
cargo run --release --bin exchange_scaling_bench -- \
  --accounts 1000 --resting-orders 10000 --price-levels 100 \
  --history-transfers 5000 --history-trades 200 \
  --samples 300 --out benchmark_results/my_exchange.csv
```

This creates exactly 1,000 funded accounts and 10,000 live one-BTC ask orders across 100 price levels. The transfer/trade history advances real nonces and balances before the final resting orders are placed. Account 0 is reserved as the taker. Fixture creation and per-sample cloning are outside the timed transaction calls; the CSV separately reports transaction, digest, and clone costs. Without these fixture flags, the original scaling sweep still runs.
