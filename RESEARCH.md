# Research and measurements

I use Light DEX to explore how consensus, networking and exchange execution affect a trading blockchain. The benchmarks are experiments on specific versions and machines, not a claim that the project can sustain these numbers in production.

## Cross-region latency

I ran signed HotStuff2 with four validators: two on a home laptop and two on a Tokyo VPS. The driver offered 200 BTC-USD spot-order transactions per second from the home laptop. A 3-of-4 quorum required at least one vote from the other region. Leader rotation was set to four blocks; each measurement lasted 70 seconds after a 12-second warm-up.

| Configuration | Committed TPS | Submission-to-commit p95 |
| --- | ---: | ---: |
| Baseline, first run | 192.5 | 3.29 s |
| Baseline, repeat | 197.9 | 1.03 s |
| Compact proposals and certificate-backed handoff | 198.0 | 0.82 s |

The p95 clock stops when the submitting node commits; it does not wait for all replicas to persist. The two baseline results show substantial network and catch-up variability, so this small sequential sample does not establish a stable speedup. Signed journal replay confirmed a common committed history in the final run, with a one-block difference in shutdown height.

## Reproduce a local workload

```bash
cargo build --release --bin light_dex
cargo build --release --manifest-path control-plane/src-tauri/Cargo.toml --bin light-dex-bench
./control-plane/src-tauri/target/release/light-dex-bench capacity \
  --algorithm hotstuff2 --nodes 4 --participants 0 --driver-accounts 96 \
  --workload orderflow --offered-tps 200 --warmup-sec 12 --measure-sec 70
```

The command starts a local cluster and writes its results to the ignored `benchmark_results/` directory. It will not reproduce the cross-region numbers above without a configured remote placement. Experimental handoff and compact proposals are enabled with `LIGHT_DEX_FAST_CERT_HANDOFF=1` and `LIGHT_DEX_COMPACT_PROPOSALS=1`, respectively.

The results are useful for investigating bottlenecks, but short runs cannot establish a durable throughput ceiling or Byzantine safety. The next experiments I want to run are longer alternating baseline and variant trials, explicit packet-loss tests, and fault tests around leader rotation.
