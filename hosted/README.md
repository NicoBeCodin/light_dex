# Hosted research network

This is the restricted public frontend and gateway. The desktop/operator control
plane remains a separate application. No operator command is routed here.

The hosted configuration uses four fixed HotStuff2 validators, two on each host,
with a three-vote quorum and leader rotation every four blocks.
New hosted sessions select BTC, ETH and SOL spot/perpetual markets (IDs
0, 1, 2, 3, 8 and 9). The selection is saved in the session manifest and bound
by its genesis state root. The full research catalog remains in
`config/markets.toml`; older manifests without a selection retain that catalog.
Hosted validators use full proposals with certificate-backed handoff. Compact
proposals remain opt-in through `deploy.py --compact-proposals`: the current
compact-body recovery path had worse tail latency in the hosted load tests. All votes have
equal weight. A process can fail without stopping commitment; losing either
physical host removes quorum. This is a research demonstration with test assets.

## Components

- `public/`: trading page, browser wallet, scoped market/account subscriptions,
  network monitor and explorer. Ed25519 keys remain in IndexedDB; backups are
  exported explicitly by the visitor.
- `../protocol-codec/`: the canonical Rust signing encoder, compiled to WebAssembly.
  Native verification and browser signing use the same encoder.
- `../src/hosted/`: loopback-only gateway, SQLite history/quotas, and supervised
  maker, taker and liquidation participants. HTTP clients cannot start agents.
- `deploy.py`: installation through two operator-configured SSH aliases. Private
  addresses, session keys and generated configuration live only in `.local/`.

Public routes are limited to metadata, network status, market/account reads,
history, signed submissions and receipt lookup. Read streams share a short cache
rather than multiplying upstream subscriptions for every visitor. Caddy terminates
HTTPS in front of the loopback gateway. Validator transport and read endpoints
stay on the private host network.

The anchored market picker has search, Spot/Perps/Favorites filters and keyboard
navigation. Price snapshots load when it opens and are cached for 15 seconds;
the selected market remains the only market stream. UI assets use a content hash
and are uploaded before switching the entry HTML, so cached pages keep a
consistent set of scripts and styles.
The selected market loads an HTTP snapshot immediately, then streams changes.
If the stream stops updating, a five-second snapshot fallback keeps the book
visible. Reconnecting does not clear order inputs. Market reads start before
wallet storage or signing initialization, so a wallet error does not hide books.

Positions can be closed with a reduce-only IOC and a selected slippage bound;
unfilled quantity remains open. The leverage control sizes a new order from a
USD margin amount, within the market limit. It does not change existing positions
or the protocol's market-wide initial-margin requirement. Account risk shows
committed collateral, equity, unrealized PnL, initial and maintenance requirements,
and margin headroom (not a guaranteed withdrawal amount).

## Storage and recovery

Every hosted validator durably saves its consensus safety state before emitting
votes or decisions. Unchanged transaction notifications skip the snapshot write;
local vote/lock changes still persist even when they produce no send action. A separate process lease prevents concurrent use of the same
validator identity. Recovery checks the chain identity and preserves votes and
locks; deleting the safety file is not a supported restart procedure.

Certified blocks are fsynced to the journal before their state is published.
Every 1,000 committed blocks, a compressed exchange checkpoint is written and
synced. Two snapshots and the previous snapshot's replay suffix are retained.
The live chain keeps at most 1,024 recent blocks after checkpointing. Catch-up
can transfer a certified checkpoint when a peer falls behind retained history.
Gateway SQLite retains seven days of blocks, receipts and trades. This is
bounded operational history, not a complete archival node.

The journal refuses writes above its configured 256 MiB budget. The gateway stops
new submissions below a 2 GiB free-space reserve and the private fleet pauses
below 5 GiB. These limits protect the host; they do not erase account state.
Account-state growth remains bounded by the session faucet's claim allowance.
Snapshots reject decoded payloads above 64 MiB and require operator attention if
that limit is reached. Do not describe this as unlimited or archival storage.

Protocol 18 removes dependence on an unbounded historical transaction digest set.
Successful replay protection uses the account nonce window. Same-block duplicate
transactions remain invalid. Existing protocol-17 sessions are not upgraded by
this installer; it initializes a separate hosted session once and never resets it.

## Operator setup

Set up the `tokyo-ls` and `ideapad-home` aliases in your own SSH configuration.
Both machines need an authenticated private network, user systemd with lingering,
and compatible Linux binaries. Only the public ingress host needs Caddy and
ports 80/443. Keep personal endpoints out of tracked configuration.

The installer requires Python 3.11 or newer.

From the repository root:

```sh
cargo build --release --bin light_dex --bin light-dex-hosted
cargo build --release --manifest-path market-data-hub/Cargo.toml
rustup target add wasm32-unknown-unknown
cargo build --release --manifest-path protocol-codec/Cargo.toml --target wasm32-unknown-unknown
cp protocol-codec/target/wasm32-unknown-unknown/release/light_dex_protocol_codec.wasm hosted/public/codec.wasm
python3 hosted/deploy.py
```

The initial gateway is read-only. Inspect all four validators, verify feeds and
recovery, then use `--enable-writes --publish` to install the dedicated Caddy
virtual host. The installer preserves other Caddy sites and validates before
reloading. Its generated endpoint and private deployment details are in
`hosted/.local/deployment.json`; do not commit that directory.

The installer replaces binaries atomically and restarts the gateway to apply its
configuration. Existing validators require a deliberate rolling restart. Use
`python3 hosted/update.py` to update one validator at a time and wait for catch-up.

```sh
systemctl --user status 'light-dex-*'
journalctl --user -u light-dex-validator-0 --since '-10 minutes'
systemctl --user restart light-dex-validator-0
```

Use those commands on the appropriate host. The six-market fleet has 33 wallets:
three makers and two takers per market, plus one keeper per perpetual market.
`extra_agent_pairs_per_market` adds maker/taker pairs (default one, maximum two)
without replacing existing keys. Quote widths vary so price priority is not
permanently assigned to one maker. The fleet has a maximum of 20
submission attempts per second, cancels stale oracle quotes, and persists its
pending signed intentions so restarts retry the same transaction and nonce.

## Measurements

The UI separates successful order transactions, execution failures and total
successful transactions. The gateway clock starts just before private validator admission, after signature,
quota and submission-storage checks, and ends
when a private commit stream observes the durable commit. Browser latency also
includes the visitor's connection to Tokyo. The p95 panel shows its sample count
and a five-minute window; it is not a benchmark claim.

Run the explicit process regression with:

```sh
cargo test --test persistent_validator -- --ignored --nocapture
```

It covers staggered startup, individual and complete restarts, pruning, certified
snapshot catch-up, and fallback after corrupting the newest snapshot. Unit tests
also cover interrupted safety writes, identity conflicts and disk budgets.

## Backup and observation

`python3 hosted/setup-backups.py` installs six-hour backup timers. Each host
replays a copy of its certified state before encrypting it with a separate GPG
passphrase and transferring it to the other host. The receiver SSH key permits
only a bounded file upload. Two incoming backups are retained. Decryption
passphrases also remain in the operator's ignored `.local/` directory.

Backups exclude validator signing keys and voting safety state. Restoring an old
voting snapshot under the same identity can forget later votes and is unsafe.
Ordinary restarts use the current safety file; backup data can help reconstruct
certified exchange state, but is not an automatic identity-rollback mechanism.

`verify-deployment.py` deliberately stops a validator, pauses the remote host's
two processes, replays every journal and restarts the complete cluster. Run it
only during an announced maintenance period.

`soak.py` observes the deployed services for 24 hours. Its report stays
`observing` until the period ends, then reports `passed` or `needs_review`.
It does not silently restart unhealthy validators or claim stability early.

A bounded operator probe runs on the ingress host:

```sh
light-dex-hosted gateway.json bench 50 20
```

The private probe accepts 1–200 requested TPS for 5–60 seconds. It stops on low
memory, disk reserve or excessive backlog. Inspect issued and admitted counts,
execution outcomes, sample coverage and rejection reasons. A configured rate is
not proof the driver delivered that rate, and a short probe is not a sustainable
capacity measurement. The public gateway separately caps submissions at 50/s and rejects new admissions
above `max_public_backlog` (128 by default); clients retry the same signed body.
Persisted public submissions are retried by the server after outages, without
requiring the visitor to keep the page open.

The generated hostname contains the ingress host's public IP. If that public IP
changes, update the hostname, TLS configuration and website link.
