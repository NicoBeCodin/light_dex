# Light DEX control plane

From this directory, run `npm ci` and `npm run serve`, then open `http://127.0.0.1:1421`. The control plane now runs in a local browser tab; the Rust backend retains wallet keys and node credentials. The top navigation has **Node monitoring**, **Node operations**, **Market**, **Market participants**, and **Explorer**. Launch a cluster in Node operations. The UI and Rust controller share protocol types generated with `ts-rs` (`npm run bindings`). Market-data feeds are observation inputs only; nothing sends orders to an external exchange. For frontend development, keep the Rust browser backend running and start `npm run dev` separately; Vite proxies `/api` to the backend.

Explorer reads the latest 20 blocks and 40 transactions every two seconds from one authenticated local validator. Click a row or search by block sequence, block hash, or full transaction digest for details. Its source label gives the exact node read API URL currently used, and it selects another local validator if that endpoint fails. An older transaction's execution result can become unavailable after the node's recent 512-outcome window expires; the transaction and block remain searchable in the active session. The remote validator's read API stays on its own localhost, so this page uses local validators as sources.

The Market workspace uses a single validator for its market-, account-, and liquidation-specific WebSocket feeds and switches to another if its market stream fails. Trading transactions default to a random responsive validator. Changing the market or wallet closes the old scoped stream. Trades show executed size, price, direction, a LIQ tag when relevant, and short maker/taker addresses with copy buttons. The rolling five-minute chart records time-spaced committed execution prices and per-second volume from when the page is opened; it does not backfill earlier activity. The Liquidations tab subscribes only while visible and displays accounts below maintenance or within 10% of their estimated trigger price. Estimates hold other market marks fixed and are not a promise of liquidatability; validators recheck on-chain. The node's authenticated read-only routes are proxied by the localhost backend, so keys and read credentials remain outside browser JavaScript. Legacy broad status polling remains for validator monitoring.

The Simulation card selects HotStuff (default) or PBFT before launch. Status polling rotates through one validator per 200 ms by default; use **Query now** for an immediate targeted status. Market data instead stays pinned to one validator with failover. Automatic leader rotation defaults to every 4 committed non-genesis blocks; blank disables it. Timeout- and fault-triggered view changes still work independently. The setting is fixed for the run and recorded in the signed session manifest. The compact **Airdrop settings** expander sets maximum claims and USD/BTC minted per claim. Every launch creates a protocol-v16 signed session with no funded accounts at genesis. The controller wallet keeps numeric labels and private keys; nodes only see public-key addresses. Signed-session validator restart still requires durable vote and lock state. Older signed-session files are not compatible and are not deleted automatically.

**Stop all** shuts down validators and releases their tunnels before enabling **Launch cluster** again in the same browser tab. The next launch creates a fresh signed chain in a new run directory. Remote journals are copied and old remote directories cleaned up in the background after the validators stop; if a copy fails, that old remote run directory is retained for recovery.

When HotStuff is selected, **Chained HotStuff** is enabled by default. Clear it before launch to compare with the original three-phase-per-block HotStuff. Chained mode extends certified speculative parents and finalizes an ancestor after three linked certificates; its initial blocks may include empty flush blocks. The signed session manifest records the choice, so changing it requires a fresh run. PBFT ignores the checkbox. The benchmark CLI uses chained mode by default and `--classic-hotstuff` for the original implementation. Optional `LIGHT_DEX_HANDOFF_TRACE=1` prints leader-handoff stages to node logs.

**TX batch window** in Node operations defaults to 50 ms for chained HotStuff. The leader coalesces new transactions for that interval before proposing, while the regular block timer remains a fallback. Set zero to disable this window. At 96 parent-eligible pending transactions, the leader proposes immediately; set `LIGHT_DEX_PROPOSAL_READY_TXS` to 32..512 to tune the trigger.

The **Perpetual margin** card now lives under Market participants alongside **Wallet addresses** and **Manual transaction**. It signs collateral deposits/withdrawals, isolated/cross mode changes, and eligible partial liquidations. New perp agents set Cross mode before depositing into a shared USD pool across all perp markets; spot balances remain separate. Switching an existing account from isolated requires flat positions, no resting perp orders, and withdrawn collateral. The card shows committed positions, equity, initial and maintenance requirements, insurance, bad debt, and funding state. Manual order entry supports any configured market.

HotStuff replicas can fetch missing committed blocks from peers after falling behind. Catch-up verifies continuity, block hashes, transaction signatures, state roots, and Ed25519 quorum certificates before applying blocks. PBFT and HotStuff certificates contain independently verifiable signatures. The current PBFT view-change logic still lacks prepared-certificate transfer, so signatures alone do not establish complete Byzantine safety.

## Participants

Fleet market makers now submit atomic `UpdateQuotes` transactions: one address,
one market, one nonce/signature, and up to 32 cancel/reduce/place instructions. Missing
cancel IDs are harmless; a live foreign order or a failed placement rolls back
the entire batch. Unchanged quotes retain FIFO priority. This applies to the
participant fleet; the legacy standalone maker API still submits individual
instructions. Perp accounts may have at most 512 resting orders across markets.

Routine monitoring requests only active participant/manual-wallet open orders,
while preserving the total exchange order count. Market/account streams share
one committed projection per resource; slow readers receive the latest snapshot
without blocking consensus. Fresh protocol-v16 sessions are required.

The **Participant fleet** card lets you add agents, select a market, edit each preset, and activate them together. **Load 13-agent mix** stages three market makers, five random-order agents, and five trend followers. **Load 100-agent mix** stages 24 market makers, 52 random-order agents, and 24 trend followers; its random agents favor resting liquidity and use fewer FOK orders. Neither button starts the agents: inspect or change rows before **Claim & launch fleet**. On launch, the controller creates one key/address per agent, signs one on-chain claim per address, and waits for claims to commit. The 100-agent mix needs 100 remaining faucet claims. Perp agents then set Cross mode, deposit USD collateral, and sign perp-order transactions. Binance mirror can quote BTC perp; Avellaneda–Stoikov and trend-history remain spot-only. Select an active agent to edit parameters or use **Log in as** to show that wallet's balances, positions, and orders in Market. **Stop and clear orders** cancels remaining quotes.

For sustained stress runs, new sessions now claim 1 billion USD and 10,000 BTC per account by default, with matching amounts of the other configured spot base assets. The Avellaneda–Stoikov preset starts with a 10,000 BTC inventory target. Ordinary perp agents deposit 10% of their available USD, capped at 100 million USD, into cross margin and add collateral from their wallet if equity falls close to required initial margin. The leveraged-taker preset keeps its configured smaller deposit so it can still be liquidated. These are simulator funding defaults, configurable under **Airdrop settings** before launch; an already running session keeps the amounts in its signed manifest. To use the larger grants, stop the cluster and launch a new session, then claim fresh participant addresses. New funding reduces balance-related stalls, but does not make orders or market liquidity unlimited.

**Wait for confirmation** is a per-agent setting that can also be changed live. It defaults to off: a taker can pipeline up to eight transactions (makers retain at most two quote batches) with consecutive nonces before the first commits. Turn it on to limit that agent to one pending transaction. Pending transactions are retried unchanged after three seconds; confirmed nonces or failure receipts clear them. Participants on different accounts run concurrently, but a strategy's target TPS is an offered-rate setting, not a guarantee of committed TPS. Stopping an agent cancels its resting orders but does **not** close its perp position or withdraw collateral.

The fleet planner uses eight bounded admission workers. Validator replies can take much longer across regions, so agents no longer wait on one another's network round trips. Random targets still include remote validators. The submit rate therefore measures accepted client submissions, while the commit rate also depends on cross-host consensus and the strategies' actual order decisions.

- **Binance mirror:** copies up to five bid/ask levels from Binance Spot's public `btcusdt@depth5@100ms` stream, mapping them to distinct whole-dollar local ticks; quantity is fixed at 1 BTC per level.
- **Avellaneda–Stoikov:** observes the same Binance feed. It estimates recent mid-price variance and quotes around a reservation price adjusted for BTC inventory. Tune risk aversion, arrival sensitivity, volatility window, inventory target/band, horizon, and refresh interval live. It waits for enough feed samples before quoting.
- **Random order:** randomly chooses buy or sell. Select IOC, FOK, or resting LO, plus target send rate; LO has an offset, cap, and cancellation TTL. Prices come from the local book, with the committed oracle mark as an LO fallback when the book is empty. This preset supports spot and perp markets.
- **Trend follower:** reads *committed local order-book changes* and submits a 1-BTC IOC in the observed direction once its lookback and tick threshold are met. No local book or insufficient history means no order.
- **Liquidation keeper:** selects a perp market, subscribes to that node's liquidatable-candidate stream, and signs partial liquidation transactions when committed margin falls below maintenance. The node rechecks risk and matching deterministically; the keeper only discovers opportunities. Its wallet needs USD collateral to backstop book residuals. This is a research prototype, not a production liquidation service.
- **Leveraged taker:** claims an address, switches to cross margin, deposits a configurable USD amount, then submits IOC orders toward a leverage target on one side. It waits for its preceding nonce and available book liquidity. A later adverse committed mark move may make its position liquidatable; the preset cannot guarantee that.

Before loading a fleet, select the markets in the **Markets for loaded mixes** row. The old 13/100-agent spot mixes require BTC spot for their spot-only makers and trend agents; random-order agents are distributed over all selected markets. The new 13-agent perp mix has eight random-order agents, four leveraged takers, and one all-market keeper. It requires at least one selected perp market; each participant is assigned to a compatible selected market, then follows claim → margin deposit → order submission. An all-market keeper uses one candidate stream and a shared cross-margin USD pool; a single-market keeper uses one market-specific stream.

A configured send rate is an attempted rate, not guaranteed commit TPS; each agent manages its account's nonce and retries pending transactions at the same nonce until confirmed. The controller schedules policies in Rust (`../src/participants/strategies.rs`, `src-tauri/src/controller/agents.rs`); TypeScript only configures and displays them. The shared Binance feed is handled in `src-tauri/src/binance_depth.rs`. Internet access is required for the two market-maker presets. BTCUSDT is treated as nominal BTC-USD, with no FX conversion.

Under heavy load, `FokUnfilled` means a fill-or-kill order could not fill at execution time; `OrderNotFound` can occur when a queued cancel reaches a book after that order has already filled or been cancelled. These are failed transactions, not divergent validator executions; the agent replans the same nonce. The 100-agent mix now uses fewer FOK orders and longer-lived resting orders, but thin books and racing cancels remain possible. A random transaction target uses responsive nodes and retries the same signed transaction once on a different node after a connection failure.

Status shows validators as selectable cards with a detailed inspector. **Query now** fetches a fresh status from one node, independent of the configured polling mode. The inspector also offers a timed pause and a confirmed kill action. Market shows the selected market's order book and recent committed fills only when its tab is open. The trade list is reconstructed from committed blocks on restart. A stale Binance feed causes BTC makers to cancel quotes. This remains a research simulator, not a production exchange or trading system.

The earlier fixed-seed synthetic agents and UI benchmark runner were removed. A separate, opt-in headless live-agent benchmark runner is documented in the [root README](../README.md); no benchmark job is run by the UI or tests. JSON examples remain in `../scenarios/`.

For a cluster with validators on multiple hosts, put your SSH host profiles in the ignored `control-plane/.local/remotes.json`, using [remotes.example.json](remotes.example.json) as a template. Node operations lets you choose a count for each configured host.

HotStuff2 broadcasts authenticated votes. Set `LIGHT_DEX_FAST_LEADER_COLLECTOR=1` before starting the control plane to compare leader-only collection. Certificate-backed four-block handoff and compact proposals are experimental: enable them with `LIGHT_DEX_FAST_CERT_HANDOFF=1` and `LIGHT_DEX_COMPACT_PROPOSALS=1`. The launcher forwards these settings to remote validators.

Verification without running a benchmark:

```bash
cargo test --manifest-path ../Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
npm run build
```

## Quote amendments and post-only orders (protocol 10)

Choose **Post only** in Market or Manual transaction to reject a limit order
that would trade immediately. Fleet and legacy makers use it automatically.
A rejected placement rolls back the entire quote batch, including cancellations;
the previous quotes stay live. Execution uses the current book, not the UI snapshot.

`ReduceOrder` and `UpdateQuotes.reduce` accept a strictly smaller, positive
`remaining_quantity` in market atoms, on the market lot grid. They preserve the
order ID and FIFO priority. A missing/filled order or a size that is no longer a
strict reduction fails atomically; cancellation alone remains idempotent.
Makers amend oversized quotes instead of cancelling/replacing them. At most two
signed quote generations remain pending; unsent intentions are regenerated from
the latest feed, while signed transactions retain their nonces and retry behavior.

Local admission is bounded by 32 MiB serialized transaction payload, 65,536
queued instruction-weight units, and 512 units per account, in addition to the
existing transaction/nonce limits. These are admission estimates, not exact CPU
costs or a total-process RSS cap. Existing deterministic execution limits remain.

`LIGHT_DEX_PREFETCH_DISTANCE=8` enables bounded cache hints in proposal/follower
execution (maximum 64); default 0 disables them. This is an experiment, not a
consensus rule, and is not enabled by default without measured benefit. Start a
fresh session for protocol 10. No older session files are deleted or migrated.
