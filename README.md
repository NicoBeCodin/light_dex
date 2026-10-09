# Light DEX

Light DEX is a local Rust blockchain and order-book simulator with a Tauri 2 + React control plane. I'm building it to learn more about decentralized exchanges and consensus protocols.

The root crate runs the nodes and implements transactions, consensus, and matching. `control-plane/` launches and monitors a local cluster; its TypeScript protocol types are generated from Rust with `ts-rs`.

## Run locally

You need Rust/Cargo, Node.js/npm, and the [Tauri 2 system prerequisites](https://v2.tauri.app/start/prerequisites/) for your OS.

```bash
cargo test
cd control-plane
npm ci
npm run tauri dev
```
