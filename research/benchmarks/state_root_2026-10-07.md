# Incremental state-root microbenchmark — 2026-10-07

Run: `cargo run --release --bin state_root_bench --quiet` on an Intel Core i5-8365U (8 logical CPUs). The benchmark runs in one process; it does not measure network, consensus, persistence, or complete block throughput. Values below are median times from one run, in microseconds unless marked `ns`.

| Accounts | Resting orders | Previous full-state SHA-256 scan | Cached Merkle-root read | Full Merkle-tree rebuild | Transfer + incremental root |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 100 | 0 | 40.3 µs | 21 ns | 568 µs | 14.2 µs |
| 100 | 1,000 | 192.5 µs | 20 ns | 8,477 µs | 19.0 µs |
| 1,000 | 1,000 | 526.9 µs | 22 ns | 21,756 µs | 22.9 µs |
| 5,000 | 5,000 | 3,134.1 µs | 20 ns | 111,111 µs | 30.0 µs |

The transfer measurement excludes fixture cloning but includes the transaction and root read; it is **not** a direct comparison with the old transfer path, because this executable no longer contains a no-tree transaction implementation. The previous full scan also hashes fewer fields than the new commitment (for example, it does not include market metadata or original order quantity). Consequently, this table shows the scaling of digest work, not an end-to-end block speedup. The cached-root read mainly measures call/timer overhead; updates pay for canonical encoding, changed-leaf hashes, and path copying during transaction execution.

Follow-up implementation: the exchange maps are now private, with read-only accessors and a genesis constructor that maintains the tree. Blocks now carry a state-root version; old blocks without the field deserialize as unversioned version 0 and retain their original block hash, whether they contain the former full digest or the transitional unversioned Merkle root. New blocks use version 1. Tests cover replay across the upgrade. The `full_tree_rebuild` row above was captured before the constructor/accessor follow-up; the benchmark executable now labels its corresponding diagnostic as `full_tree_verify`.

After the follow-up, the same command produced the following medians (one run; machine load was higher, so do not treat differences from the first table as a code regression):

| Accounts | Resting orders | Legacy full scan | Cached root read | Full tree verify | Transfer + root |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 100 | 0 | 50.4 µs | 29 ns | 861 µs | 20.8 µs |
| 100 | 1,000 | 274 µs | 28 ns | 12,500 µs | 25.8 µs |
| 1,000 | 1,000 | 764 µs | 38 ns | 29,807 µs | 30.5 µs |
| 5,000 | 5,000 | 5,970 µs | 47 ns | 184,030 µs | 46.7 µs |

The block-commit optimization is not captured by this exchange-only benchmark. Its correctness is covered by replay, prepared-result reuse, and persistence-failure tests; a separate block-level benchmark would be needed to quantify its speedup.
