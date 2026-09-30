import { invoke } from "@tauri-apps/api/core"
import type { ControlSnapshot } from "@/bindings/ControlSnapshot"
import type { LaunchConfig } from "@/bindings/LaunchConfig"
import type { LoadConfig } from "@/bindings/LoadConfig"
import type { MarketMakerConfig } from "@/bindings/MarketMakerConfig"
import type { TargetNode } from "@/bindings/TargetNode"
import type { Transaction } from "@/bindings/Transaction"

export const api = {
  defaultConfig: () => invoke<LaunchConfig>("default_config"),
  launch: (config: LaunchConfig) => invoke<void>("launch", { config }),
  stopAll: () => invoke<void>("stop_all"),
  snapshot: () => invoke<ControlSnapshot>("get_snapshot"),
  submitTransaction: (transaction: Transaction, target: TargetNode) =>
    invoke<void>("submit_transaction", { transaction, target }),
  pauseNode: (target: TargetNode, millis: number) =>
    invoke<void>("pause_node", { target, millis }),
  killNode: (target: TargetNode) => invoke<void>("kill_node", { target }),
  startLoad: (config: LoadConfig) => invoke<void>("start_load", { config }),
  stopLoad: () => invoke<void>("stop_load"),
  startMarketMaker: (config: MarketMakerConfig) => invoke<void>("start_market_maker", { config }),
  stopMarketMaker: () => invoke<void>("stop_market_maker"),
}
