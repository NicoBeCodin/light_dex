# Pilot provenance

- Date: 2 October 2026; host CPU: Intel Core i5-8365U @ 1.60 GHz.
- Git HEAD: `3c6acc764c5e7ee3dc5877a6692c1762bd1d68ae`.
- Tracked worktree diff SHA-256 at the pilot and final load runs: `04db521730fa13a3739891f6a45bd50b3716ef5236a999c4a260ed0ea53a5dd5`.
- Release node binary SHA-256: `7d5d44260342e4211831d1be3d5c23200a01ae427fea5f1c81d687581ea0b887`.
- The worktree included untracked source files, so the diff hash alone does **not** reconstruct the executed tree. The binary hash is the authoritative identity of the tested node executable.
- Local raw run folders: `benchmark_results/runs/pilot-1790970277678` (26-trial pilot) and `benchmark_results/runs/pilot-1790971776123` (final 87-account load sweep). Those folders are intentionally ignored by Git and contain chain files and per-node 500-ms samples.
- There was no matched market-data replay; every trial used the Binance depth feed as observed at run time.

The tracked summaries and SVGs are curated for the article. To reproduce measurements rather than merely redraw them, run the release benchmark again under the same host conditions and record a new provenance entry.

## Bounded-pipeline follow-up, 3 October 2026

- Local raw folders: `benchmark_results/runs/pilot-1791052846207` (window-four transfers) and `benchmark_results/runs/pilot-1791053021554` (window-one mixed-order recheck).
- Both used the same release node binary SHA-256 listed above.
- Benchmark runner binary SHA-256: `d3e6d744d074133b5f091eb698ace64fd4c76c5c5f269762876f49d558dc6ae7`.
- Tracked worktree diff SHA-256 recorded at both runs: `76bdd86558a116c053d27c004558d3b8be0b874cb5dcb1e5f674771f5e4a04d2`. The working tree also contained untracked benchmark source, so use the runner binary hash to identify the executed harness.
- CPU: the same Intel Core i5-8365U laptop; live Binance data was not replayed or matched.
