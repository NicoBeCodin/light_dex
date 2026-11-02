# Light DEX

Light DEX combines a Rust validator, an order-book exchange, and a browser control plane for running experiments. I'm building it to learn more about decentralized exchanges and consensus protocols.

The question driving this project is how exchange execution, consensus, networking and validator hardware affect one another. The goal is to change those pieces, run a workload, and inspect both the results and the code behind them.

## Try it

Install Rust and Node.js, then run:

```bash
cargo test
cd control-plane
npm ci
npm run serve
```

Open **http://127.0.0.1:1421**, choose the consensus settings in Node operations, and launch the cluster. You can place orders, run market participants, inspect markets and nodes, inject faults, and explore committed blocks. The [control plane guide](control-plane/README.md) covers its controls and requirements.

For a cluster spanning computers, configure SSH hosts using [the example profile](control-plane/remotes.example.json). Keep your own host details in `control-plane/.local/remotes.json`, which Git ignores.

## What's inside

- **Validator:** signed transactions, state commitments, networking and journaled blocks.
- **Consensus:** PBFT and several HotStuff-style research implementations with configurable leader rotation.
- **Exchange:** spot order books and a prototype perpetual exchange with margin, funding and liquidation mechanics.
- **Control plane:** node monitoring, fault injection, market data, participants and a block explorer.
- **Experiments:** benchmarks and traces for execution, validator scaling and cross-region latency.

The validator code lives in `src/`, and the control plane lives in `control-plane/`. Market parameters are defined in [config/markets.toml](config/markets.toml).

## What the measurements show

These are examples from individual experiments, not a capacity guarantee. They use different workloads and configurations.

One cross-region experiment reached 198 committed TPS and 821 ms end-to-end p95 with four HotStuff2 validators, split between an IdeaPad and Tokyo, at 200 offered spot-order TPS. Other baseline runs varied substantially. See [Research and measurements](RESEARCH.md) for the setup, limitations and a command to run the benchmark.

## Current limits

This is a research simulator, not an exchange for real funds. Consensus variants and performance changes are experimental. Validator votes and locks are not durably journaled for safe live restart, mixed protocol versions are unsupported, and the project has not received an independent security audit.

Market-making agents can read public exchange data; they do not place orders on those external exchanges. Signed sessions use generated local keys and start with a configured research faucet. Local processes run under the same operating-system user, so they are not isolated from one another's session files.
