import { invoke } from "@tauri-apps/api/core"
import type { ControlSnapshot } from "@/bindings/ControlSnapshot"
import type { LaunchConfig } from "@/bindings/LaunchConfig"
import type { LoadConfig } from "@/bindings/LoadConfig"
import type { MarketMakerConfig } from "@/bindings/MarketMakerConfig"
import type { TargetNode } from "@/bindings/TargetNode"
import type { Transaction } from "@/bindings/Transaction"
import type { AgentConfig } from "@/bindings/AgentConfig"
import type { WalletAccount } from "@/bindings/WalletAccount"
import type { NodeRuntimeSnapshot } from "@/bindings/NodeRuntimeSnapshot"

export const api = {
  defaultConfig: () => invoke<LaunchConfig>("default_config"),
  launch: (config: LaunchConfig) => invoke<void>("launch", { config }),
  stopAll: () => invoke<void>("stop_all"),
  snapshot: () => invoke<ControlSnapshot>("get_snapshot"),
  refreshNodeStatus: (nodeId: number) => invoke<NodeRuntimeSnapshot>("refresh_node_status", { nodeId }),
  submitTransaction: (transaction: Transaction, target: TargetNode) =>
    invoke<void>("submit_transaction", { transaction, target }),
  createAddress: () => invoke<WalletAccount>("create_address"),
  airdropNewAddress: (fromLabel: number, asset: number, amount: number, target: TargetNode) =>
    invoke<WalletAccount>("airdrop_new_address", { fromLabel, asset, amount, target }),
  pauseNode: (target: TargetNode, millis: number) =>
    invoke<void>("pause_node", { target, millis }),
  killNode: (target: TargetNode) => invoke<void>("kill_node", { target }),
  startLoad: (config: LoadConfig) => invoke<void>("start_load", { config }),
  stopLoad: () => invoke<void>("stop_load"),
  startMarketMaker: (config: MarketMakerConfig) => invoke<void>("start_market_maker", { config }),
  stopMarketMaker: () => invoke<void>("stop_market_maker"),
  startAgent: (config: AgentConfig) => invoke<void>("start_agent", { config }),
  stopAgent: (agentId: number) => invoke<void>("stop_agent", { agentId }),
  startAgents: (configs: AgentConfig[]) => invoke<number>("start_agents", { configs }),
  stopAgents: (agentIds: number[]) => invoke<void>("stop_agents", { agentIds }),
  defaultParticipantFleet: () => invoke<AgentConfig[]>("default_participant_fleet"),
}
