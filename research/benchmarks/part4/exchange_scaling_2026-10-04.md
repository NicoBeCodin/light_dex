# Exchange-state scaling, 4 October 2026

This benchmark isolates `ExchangeState` on one process. It does **not** run nodes, consensus, TCP, the controller, Binance participants, or disk persistence. Its purpose is to identify which exchange operations grow with account count, resting orders, queue length, and number of fills. The complete [CSV](data/exchange_scaling_2026-10-04.csv) is retained with this note.

## Reproduce

```bash
cargo build --release --bin exchange_scaling_bench
./target/release/exchange_scaling_bench --samples 500 --out benchmark_results/exchange_scaling.csv
```

The run used an Intel Core i5-8365U (4 cores / 8 threads), release mode, on the local laptop. The benchmark executable SHA-256 was `5131efad0a58aa7c3a10b44ef2d98aba458475570de8b998171a9bc088cfbcdc`; HEAD was `3c6acc7` with a dirty worktree. The benchmark, not the exchange implementation, was added for this run. Results were checked with a second run of the same executable; for example, the 100,000-account digest medians were 60.85 and 61.72 ms, the 10,000-order digest medians were 1.46 and 1.64 ms, and 500-fill IOC medians were 123.85 and 122.99 µs. These are repeat observations, not a statistical confidence interval.

Each isolated operation has a deterministic fixture, 10 warm-up iterations, then up to 500 measured samples (100 for 10,000-order fixtures, 60 for 100,000-account fixtures). Fixture cloning is **outside** the transaction timer. The `state_clone` row intentionally times a whole-state clone and its destruction. The stream rows time one growing state across 200,000 transfers or 20,000 passive limit orders, including transaction construction and nonce lookup; they report mean ns/op only. All fixtures are cache-warm and single-threaded. The actual chain has 100 funded genesis accounts; larger account fixtures add funded accounts in memory to test scaling, without changing genesis.

## Findings

| Workload | Median | p95 | Interpretation |
| --- | ---: | ---: | --- |
| Transfer, 100 accounts | 0.56 µs | 0.60 µs | Small touched-account transaction |
| Transfer, 10,000 accounts | 0.69 µs | 1.16 µs | Nearly flat while touched set stays at two |
| Transfer, 100,000 accounts | 4.52 µs | 5.10 µs | Cache/memory pressure is visible at this synthetic size |
| Digest, 100 / 10,000 / 100,000 accounts | 0.035 / 3.49 / 61.72 ms | 0.064 / 3.96 / 68.70 ms | Full traversal, sorting and SHA-256 scale with state |
| Clone, 100 / 10,000 / 100,000 accounts | 0.0044 / 0.541 / 18.61 ms | 0.0046 / 0.676 / 20.43 ms | Whole-state copy becomes costly at large account count |
| Passive LO, 10,000 resting orders at distinct prices | 1.49 µs | 2.35 µs | Individual order placement remains fairly cheap |
| Digest, 10,000 resting orders | 1.64 ms | 2.02 ms | Includes the 100 accounts as well as the book |
| Clone, 10,000 resting orders | 0.689 ms | 1.22 ms | Includes the 100 accounts as well as the book |
| Cancel back of 10,000-order FIFO at one price | 17.86 µs | 30.14 µs | Search/removal scales with queue position |
| IOC matching 500 one-unit maker orders | 122.99 µs | 154.96 µs | Includes preview, account settlement, receipt and book update |
| Order-book-only IOC matching the same 500 orders | 4.97 µs | 5.09 µs | Matching mutation alone is a small part of exchange cost |

The isolated stream averages were 0.386 µs/transfer (~2.6 million operations/s) and 0.552 µs/passive LO (~1.8 million operations/s). **These are not blockchain TPS or sustainable exchange capacity:** the loop omits block roots, full-state copies, validation, persistence, networking and consensus. Likewise, the per-operation medians must not be converted directly to cluster throughput.

### Likely bottlenecks, in priority order

1. **Whole-state work at block boundaries.** `BlockchainState::build_block` clones `ExchangeState` and computes `state_digest`; `validate_block` does both again, and `commit_block_with_receipts` calls validation. The benchmark directly measures these primitives, but not their frequency per replica or block. At 10,000 resting orders, one clone plus digest was roughly 2.3 ms in isolation; at 100,000 accounts it was roughly 80 ms. If state size grows, these costs can dominate even when each transaction executes in microseconds. Do not interpret this as an additive exact block time: cache effects and other block work differ.
2. **Multi-fill settlement and receipt construction.** At 500 fills, `ExchangeState::execute_with_receipt` took ~123 µs, while `OrderBook::apply_order` took ~5 µs and `preview_order` ~2.3 µs. The difference includes touched-account staging, per-fill balance updates, trade receipts and location-map maintenance. It is not solely an order-book data-structure bottleneck. IOC and FOK had similar costs when both succeeded.
3. **Long FIFO queues at one price.** Cancelling the back of a 10,000-order level took ~17.9 µs end-to-end, versus ~0.79 µs at the front. The direct book cancellation took ~8.9 µs at the back, so linear queue search/removal is a material component. This is a pathological concentration at one price, not a typical dispersed book.
4. **Ordinary transfers and passive placements are not the current hot path in isolation.** Touched-account staging keeps transfers approximately flat from 100 to 10,000 accounts. Passive LO placement at 10,000 distinct price levels stayed around 1.5 µs median. Their surrounding block and status-polling costs can still limit the live system.

## Next measurements before claiming a maximum

Profile a full node while the order book grows, separating time in `build_block`, `validate_block`, state digest, state clone, persistence, and status serialization. Then run sustained exchange-only mixed workloads with replenished maker balances and a controlled distribution of fills per taker, price-level widths, and cancellations. The isolated numbers here locate scaling risks; they do not establish a live-node limit, a maximum account count, or a safe production capacity.
