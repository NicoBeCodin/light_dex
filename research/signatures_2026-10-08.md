# Signed sessions and quorum evidence — 2026-10-08

Historical note: these measurements and archive-compatibility statements describe the first signed format. Protocol v2 now uses address-based accounts, a different genesis root, and new transaction/block domains. Pre-v2 archives are intentionally not replayed by the migrated node; rerun the microbenchmark for current timings.

Light DEX now generates a new public session manifest, independent validator keys, and independent account keys for every cluster launch. Its manifest defines the consensus algorithm, chain ID, validator/account public keys, and initial state root. Private keys live in the session's `keys/` directory. The control plane signs account transactions in Rust; nodes verify them before mempool admission and again during block validation. The transaction payload remains the same Rust enum exported to TypeScript.

Consensus messages use Ed25519 signatures over versioned, chain-specific, deterministic bytes. Peer connections use signed fresh challenges. PBFT commit and view-change messages retain signed voter evidence; HotStuff QCs retain signed votes, and a new view carries signed high-QC reports. Both algorithms use `n − floor((n − 1)/3)` votes for a quorum. Commit certificates are stored beside blocks and are required when a lagging node applies a fetched block or an archived signed chain is replayed. The certificate is outside the block hash to avoid signing a hash that contains its own proof.

The session's `validator-N.started` marker is synced to disk before consensus begins. A signed validator refuses to restart with the same key because votes and locks are still only held in memory. The control plane rejects live recovery for PBFT and HotStuff. Old unsigned chains remain readable through `BlockchainState::replay_legacy`; signed chains use `BlockchainState::load_signed` with the original manifest. There is no automatic unsigned-to-signed migration.

## Focused signature cost

Command: `target/release/signature_microbench 1000` on this machine. Values are microseconds per operation from one run; key generation and disk access were outside the timed loops.

| Validators | Transaction sign | Transaction verify | Commit QC verify |
| ---: | ---: | ---: | ---: |
| 4 | 57.3 | 51.8 | 141.4 |
| 7 | 59.2 | 52.0 | 241.0 |
| 13 | 61.6 | 51.9 | 431.5 |
| 20 | 58.2 | 52.6 | 679.0 |

QC verification checks `n − f` individual Ed25519 signatures. These figures measure the cryptographic checks, not consensus throughput. Network scheduling, block execution, certificate construction, and disk sync can dominate a live run. This single run is a sizing observation, not a stable capacity claim.

## Scope and next safety steps

Signatures establish who authorized a transaction or vote and let a node validate quorum evidence from another peer. The current PBFT view change does not transfer prepared certificates, and neither algorithm persists its voting/locking safety state. A complete adversarial safety claim requires those changes and a protocol review. All local processes currently run as the same OS user, so file mode `0600` protects keys from other users and accidental commits, but does not isolate one compromised node process from another node's key files.

The canonical byte codec signs a sorted, typed encoding of Serde values; changes to signed fields, enum variants, or encoding tags require an explicit protocol version change. The signed block hash is `light-dex/block/v2`, while the exchange state root remains the existing Merkle version. Archived unsigned hashes retain their old format and cannot be appended to a signed live session.
