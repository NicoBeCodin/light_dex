// Browser transport. The local Rust backend owns keys and proxies narrow
// read-only subscriptions to the selected validator.
let session: Promise<string> | undefined
async function token(): Promise<string> {
  session ??= fetch("/api/session", { credentials: "same-origin" })
    .then(async response => {
      if (!response.ok) throw new Error("Browser session unavailable")
      return (await response.json() as { token: string }).token
    })
    .catch(error => { session = undefined; throw error })
  return session
}
async function authorizedFetch(path: string, init: RequestInit = {}): Promise<Response> {
  for (let attempt = 0; attempt < 2; attempt++) {
    const headers = new Headers(init.headers)
    headers.set("x-light-dex-token", await token())
    const response = await fetch(path, { ...init, headers, credentials: "same-origin" })
    if (response.status !== 403 || attempt === 1) return response
    // A restarted localhost backend has a new browser token. Recover in the
    // same tab instead of leaving every later command unauthorized.
    session = undefined
  }
  throw new Error("Browser session unavailable")
}
async function invoke<T>(command: string, args: Record<string, unknown> = {}): Promise<T> {
  const response = await authorizedFetch("/api/invoke", {
    method: "POST", headers: { "content-type": "application/json" },
    body: JSON.stringify({ command, args }),
  })
  const value = await response.json()
  if (!response.ok) throw new Error(value.error ?? `Request failed: ${response.status}`)
  return value as T
}
export const nodeApi = {
  async get<T>(nodeId: number, scope: "markets" | "accounts" | "liquidations" | "explorer" | "blocks" | "transactions" | "hashes", key: string | number): Promise<T> {
    const response = await authorizedFetch(`/api/nodes/${nodeId}/${scope}/${key}`)
    if (!response.ok) throw new Error(`Node query failed: ${response.status}`)
    return response.json() as Promise<T>
  },
  async subscribe(nodeId: number, scope: "markets" | "accounts" | "liquidations", key: string | number): Promise<WebSocket> {
    const scheme = location.protocol === "https:" ? "wss:" : "ws:"
    session = undefined
    return new WebSocket(`${scheme}//${location.host}/api/nodes/${nodeId}/${scope}/${key}/stream?token=${encodeURIComponent(await token())}`)
  },
}
import type { ControlSnapshot } from "@/bindings/ControlSnapshot"
import type { LaunchConfig } from "@/bindings/LaunchConfig"
import type { RemoteChoice } from "@/bindings/RemoteChoice"
import type { LoadConfig } from "@/bindings/LoadConfig"
import type { MarketMakerConfig } from "@/bindings/MarketMakerConfig"
import type { TargetNode } from "@/bindings/TargetNode"
import type { Transaction } from "@/bindings/Transaction"
import type { AgentConfig } from "@/bindings/AgentConfig"
import type { WalletAccount } from "@/bindings/WalletAccount"
import type { NodeRuntimeSnapshot } from "@/bindings/NodeRuntimeSnapshot"

export const api = {
  defaultConfig: () => invoke<LaunchConfig>("default_config"),
  remoteProfiles: () => invoke<RemoteChoice[]>("remote_profiles"),
  launch: (config: LaunchConfig) => invoke<void>("launch", { config }),
  stopAll: () => invoke<void>("stop_all"),
  snapshot: () => invoke<ControlSnapshot>("get_snapshot"),
  refreshNodeStatus: (nodeId: number) => invoke<NodeRuntimeSnapshot>("refresh_node_status", { nodeId }),
  submitTransaction: (transaction: Transaction, target: TargetNode) =>
    invoke<void>("submit_transaction", { transaction, target }),
  createAddress: () => invoke<WalletAccount>("create_address"),
  claimNewAccounts: (count: number) => invoke<WalletAccount[]>("claim_new_accounts", { count }),
  airdropNewAddress: (fromLabel: number, asset: number, amount: number, target: TargetNode) =>
    invoke<WalletAccount>("airdrop_new_address", { fromLabel, asset, amount, target }),
  fundAddress: (fromLabel: number, to: string, asset: number, amount: number, target: TargetNode) =>
    invoke<void>("fund_address", { fromLabel, to, asset, amount, target }),
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
  hundredParticipantFleet: () => invoke<AgentConfig[]>("hundred_participant_fleet"),
  perpParticipantFleet: () => invoke<AgentConfig[]>("perp_participant_fleet"),
}
