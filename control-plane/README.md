# Light DEX control plane

From this directory, run `npm ci` and `npm run tauri dev`. The UI has four tabs: **Overview** for validator status and per-node controls, **Market** for the BTC-USD order book and recent trades, **Participants** for agent fleets, and **Operations** for cluster configuration, wallets, manual transactions, fault injection, and logs. Launch a cluster in Operations. The UI and Rust controller share protocol types generated with `ts-rs` (`npm run bindings`). Nothing sends orders to Binance; the public BTCUSDT depth stream is input data only.

The Simulation card selects PBFT (default) or HotStuff before launch. The choice is fixed for that run. Every launch creates a new protocol-v2 signed session and displays its chain ID and authentication rejection count. The controller wallet keeps local numeric labels, address mappings, and private keys; nodes only see public-key addresses. The **Wallet addresses** card can create an unfunded address or **Airdrop new address** from a funded wallet (USD or BTC) to a newly generated address. The latter submits a transaction; wait for it to commit before using that wallet as a sender. The manual Transfer form also accepts a recipient address copied from the wallet list. The controller holds account signing keys; each node loads its own validator key and verifies account transactions and signed peer traffic. Live restart is disabled for both algorithms until votes and locks are persisted. Keys and session data are kept in the selected node data directory; keep it private and do not publish it. Old signed-session files are not compatible with protocol v2 and are not deleted or migrated automatically.

HotStuff replicas can fetch missing committed blocks from peers after falling behind. Catch-up verifies continuity, block hashes, transaction signatures, state roots, and Ed25519 quorum certificates before applying blocks. PBFT and HotStuff certificates contain independently verifiable signatures. The current PBFT view-change logic still lacks prepared-certificate transfer, so signatures alone do not establish complete Byzantine safety.

## Participants

The **Participant fleet** card lets you add agents with distinct funded accounts, edit each preset, and activate them together. **Load 13-agent mix** prepares, but does not launch, a fleet of three market makers (one Binance depth mirror and two Avellaneda–Stoikov variants), five random-order agents (IOC/FOK/LO), and five trend followers with different lookbacks and thresholds. Inspect or change its rows before **Launch fleet**. Select an active agent in the roster to see its confirmed nonce, pending action, BTC/USD available and locked balances, order count, and feed age; its parameters and target node can be updated live. **Stop and clear orders** or **Stop group** waits for in-flight transactions and cancels any remaining resting orders. Use a fresh account with no unrelated open orders per agent.

- **Binance mirror:** copies up to five bid/ask levels from Binance Spot's public `btcusdt@depth5@100ms` stream, mapping them to distinct whole-dollar local ticks; quantity is fixed at 1 BTC per level.
- **Avellaneda–Stoikov:** observes the same Binance feed. It estimates recent mid-price variance and quotes around a reservation price adjusted for BTC inventory. Tune risk aversion, arrival sensitivity, volatility window, inventory target/band, horizon, and refresh interval live. It waits for enough feed samples before quoting.
- **Random order:** randomly chooses buy or sell. Select IOC, FOK, or resting LO, plus target send rate; LO has an offset, cap, and cancellation TTL. Prices come from the local book.
- **Trend follower:** reads *committed local order-book changes* and submits a 1-BTC IOC in the observed direction once its lookback and tick threshold are met. No local book or insufficient history means no order.

The **Advanced transfer traffic** expander retains the separate multi-account transfer sender. Account ownership must not overlap an agent. A configured send rate is an attempted rate, not guaranteed commit TPS; each agent manages its account's nonce and retries a pending transaction at the same nonce until confirmed. The controller schedules policies in Rust (`../src/participants/strategies.rs`, `src-tauri/src/controller.rs`); TypeScript only configures and displays them. The shared Binance feed is handled in `src-tauri/src/binance_depth.rs`. Internet access is required for the two market-maker presets. BTCUSDT is treated as nominal BTC-USD, with no FX conversion.

Overview shows validators as selectable cards with a detailed inspector. **Query now** fetches a fresh status from one node, independent of the configured polling mode; the card displays when that node last answered. The inspector also offers a timed pause and a confirmed kill action. Market shows the BTC-USD order book and recent committed fills only when its tab is open. The trade list is reconstructed from committed blocks on restart. A stale Binance feed causes makers to cancel quotes. This remains a research simulator, not a production exchange or trading system.

The earlier fixed-seed synthetic agents and UI benchmark runner were removed. A separate, opt-in headless live-agent benchmark runner is documented in the [root README](../README.md); no benchmark job is run by the UI or tests. JSON examples remain in `../scenarios/`.

Verification without running a benchmark:

```bash
cargo test --manifest-path ../Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
npm run build
```
