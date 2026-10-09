import { useState } from "react"
import { api } from "@/api"
import type { AgentConfig } from "@/bindings/AgentConfig"
import type { AgentStrategy } from "@/bindings/AgentStrategy"
import type { ControlSnapshot } from "@/bindings/ControlSnapshot"
import type { LoadConfig } from "@/bindings/LoadConfig"
import type { OrderType } from "@/bindings/OrderType"
import type { TargetNode } from "@/bindings/TargetNode"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"

type Kind = AgentStrategy["kind"]

const names: Record<Kind, string> = {
  binance_mirror: "Binance mirror",
  avellaneda_stoikov: "Avellaneda–Stoikov",
  random_order: "Random order",
  trend_follower: "Trend follower",
}

function defaultStrategy(kind: Kind): AgentStrategy {
  switch (kind) {
    case "binance_mirror": return { kind, levels: 3, refresh_ms: 1000 }
    case "avellaneda_stoikov": return { kind, gamma: 0.001, k: 0.02, horizon_seconds: 30,
      volatility_window_seconds: 60, target_inventory_btc: 1000, max_inventory_deviation_btc: 5, refresh_ms: 1000 }
    case "random_order": return { kind, order_type: "IOC", tps: 1, passive_offset_ticks: 10,
      max_resting_orders: 2, resting_ttl_ms: 5000 }
    case "trend_follower": return { kind, lookback_updates: 8, threshold_ticks: 3, tps: 0.5 }
  }
}

function newConfig(agentId: number, accountId: number, kind: Kind = "random_order"): AgentConfig {
  return { agent_id: agentId, account_id: accountId, market_id: 0, target: { kind: "random" }, strategy: defaultStrategy(kind) }
}

function Numeric({ label, value, set, step = 1, disabled = false }: { label: string; value: number; set: (value: number) => void; step?: number; disabled?: boolean }) {
  return <div className="space-y-1"><Label>{label}</Label><Input type="number" step={step} value={value} disabled={disabled} onChange={event => set(Number(event.target.value))} /></div>
}

function Editor({ config, onChange, nodes, lockIdentity }: {
  config: AgentConfig; onChange: (config: AgentConfig) => void; nodes: ControlSnapshot["nodes"]; lockIdentity: boolean
}) {
  const strategy = config.strategy
  const change = (strategy: AgentStrategy) => onChange({ ...config, strategy })
  const targetValue = config.target.kind === "node" ? `node:${config.target.node_id}` : config.target.kind
  return <div className="space-y-4">
    <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
      <Numeric label="Agent ID" value={config.agent_id} disabled={lockIdentity} set={value => onChange({ ...config, agent_id: value })} />
      {lockIdentity ? <Numeric label="Wallet #" value={config.account_id} disabled set={value => onChange({ ...config, account_id: value })} />
        : <div className="space-y-1"><Label>Wallet</Label><Input value="New address on activation" disabled /></div>}
      <div className="space-y-1"><Label>Preset</Label><Select value={strategy.kind} disabled={lockIdentity} onValueChange={value => value && change(defaultStrategy(value as Kind))}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent>{(Object.keys(names) as Kind[]).map(kind => <SelectItem key={kind} value={kind}>{names[kind]}</SelectItem>)}</SelectContent></Select></div>
      <div className="space-y-1"><Label>Submit to</Label><Select value={targetValue} onValueChange={value => value && onChange({ ...config, target: value === "random" || value === "primary" ? { kind: value } as TargetNode : { kind: "node", node_id: Number(value.slice(5)) } })}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="random">Random node</SelectItem><SelectItem value="primary">Primary</SelectItem>{nodes.filter(node => node.state !== "dead").map(node => <SelectItem key={node.node_id} value={`node:${node.node_id}`}>Node {node.node_id}</SelectItem>)}</SelectContent></Select></div>
    </div>
    <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
      {strategy.kind === "binance_mirror" && <>
        <Numeric label="Levels per side" value={strategy.levels} set={levels => change({ ...strategy, levels })} />
        <Numeric label="Refresh (ms)" value={strategy.refresh_ms} set={refresh_ms => change({ ...strategy, refresh_ms })} />
      </>}
      {strategy.kind === "avellaneda_stoikov" && <>
        <Numeric label="Risk aversion γ (1/USD)" value={strategy.gamma} step={0.0001} set={gamma => change({ ...strategy, gamma })} />
        <Numeric label="Arrival sensitivity k (1/USD)" value={strategy.k} step={0.001} set={k => change({ ...strategy, k })} />
        <Numeric label="Risk horizon (s)" value={strategy.horizon_seconds} set={horizon_seconds => change({ ...strategy, horizon_seconds })} />
        <Numeric label="Volatility window (s)" value={strategy.volatility_window_seconds} set={volatility_window_seconds => change({ ...strategy, volatility_window_seconds })} />
        <Numeric label="Target BTC inventory" value={strategy.target_inventory_btc} set={target_inventory_btc => change({ ...strategy, target_inventory_btc })} />
        <Numeric label="Max inventory deviation (BTC)" value={strategy.max_inventory_deviation_btc} set={max_inventory_deviation_btc => change({ ...strategy, max_inventory_deviation_btc })} />
        <Numeric label="Refresh (ms)" value={strategy.refresh_ms} set={refresh_ms => change({ ...strategy, refresh_ms })} />
      </>}
      {strategy.kind === "random_order" && <>
        <div className="space-y-1"><Label>Order type</Label><Select value={strategy.order_type} onValueChange={value => value && change({ ...strategy, order_type: value as OrderType })}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="IOC">IOC</SelectItem><SelectItem value="FOK">FOK</SelectItem><SelectItem value="LO">LO</SelectItem></SelectContent></Select></div>
        <Numeric label="Target TPS" value={strategy.tps} step={0.1} set={tps => change({ ...strategy, tps })} />
        {strategy.order_type === "LO" && <>
          <Numeric label="Passive offset (ticks)" value={strategy.passive_offset_ticks} set={passive_offset_ticks => change({ ...strategy, passive_offset_ticks })} />
          <Numeric label="Max resting orders" value={strategy.max_resting_orders} set={max_resting_orders => change({ ...strategy, max_resting_orders })} />
          <Numeric label="Resting TTL (ms)" value={strategy.resting_ttl_ms} set={resting_ttl_ms => change({ ...strategy, resting_ttl_ms })} />
        </>}
      </>}
      {strategy.kind === "trend_follower" && <>
        <Numeric label="Committed book updates" value={strategy.lookback_updates} set={lookback_updates => change({ ...strategy, lookback_updates })} />
        <Numeric label="Trend threshold (ticks)" value={strategy.threshold_ticks} set={threshold_ticks => change({ ...strategy, threshold_ticks })} />
        <Numeric label="Target TPS" value={strategy.tps} step={0.1} set={tps => change({ ...strategy, tps })} />
      </>}
    </div>
    <p className="text-xs text-muted-foreground">Makers observe Binance. Trend agents observe committed local-book updates. Random orders price against the local book. All orders are 1 BTC; submission rate is not guaranteed commit TPS.</p>
  </div>
}

export function ParticipantsTab({ snapshot, command }: { snapshot: ControlSnapshot | null; command: (operation: () => Promise<void>) => Promise<void> }) {
  const [draft, setDraft] = useState<AgentConfig>(newConfig(1, 0))
  const [fleet, setFleet] = useState<AgentConfig[]>([])
  const [preparedWallets, setPreparedWallets] = useState<{ label: number; address: string }[]>([])
  const [launchingFleet, setLaunchingFleet] = useState(false)
  const [editingFleetIndex, setEditingFleetIndex] = useState<number | null>(null)
  const [selectedId, setSelectedId] = useState<number | null>(null)
  const [transferTps, setTransferTps] = useState(5)
  const [transferFirst, setTransferFirst] = useState(0)
  const [transferCount, setTransferCount] = useState(10)
  const agents = snapshot?.agents ?? []
  const selected = agents.find(agent => agent.agent_id === selectedId)
  const groups = [...new Set(agents.flatMap(agent => agent.group_id === null ? [] : [agent.group_id]))]
  const nextId = () => Math.max(0, ...agents.map(agent => agent.agent_id), ...fleet.map(agent => agent.agent_id), draft.agent_id) + 1
  const addRow = () => { setPreparedWallets([]); setFleet(rows => [...rows, newConfig(nextId(), 0)]) }
  const loadMeta = () => command(async () => {
    const template = await api.defaultParticipantFleet()
    const startId = Math.max(0, ...agents.map(agent => agent.agent_id)) + 1
    setFleet(template.map((config, index) => ({ ...config, agent_id: startId + index, account_id: 0 })))
    setPreparedWallets([])
    setEditingFleetIndex(null)
  })
  const editActive = (id: number) => {
    const agent = agents.find(item => item.agent_id === id)
    if (!agent) return
    setSelectedId(id)
    setEditingFleetIndex(null)
    setDraft({ agent_id: id, account_id: agent.account_id, market_id: 0, target: agent.target, strategy: agent.strategy })
  }
  const saveDraft = () => {
    if (editingFleetIndex !== null) {
      setFleet(rows => rows.map((row, index) => index === editingFleetIndex ? draft : row))
      setEditingFleetIndex(null)
      return Promise.resolve()
    }
    if (selected) return api.startAgent(draft)
    return api.claimNewAccounts(1).then(([wallet]) => api.startAgent({ ...draft, account_id: wallet.label }))
  }
  const launchFleet = async () => {
    if (launchingFleet || fleet.length === 0) return
    setLaunchingFleet(true)
    try {
      const wallets = preparedWallets.length === fleet.length ? preparedWallets : await api.claimNewAccounts(fleet.length)
      setPreparedWallets(wallets)
      await api.startAgents(fleet.map((config, index) => ({ ...config, account_id: wallets[index].label })))
      setFleet([])
      setPreparedWallets([])
    } finally { setLaunchingFleet(false) }
  }
  const startTransfers = () => {
    const config: LoadConfig = { preset: { kind: "random_transfers", first_account: transferFirst,
      account_count: transferCount, asset: 0, amount: 1 }, target: { kind: "random" }, tps: transferTps }
    return api.startLoad(config)
  }

  return <div className="space-y-5">
    <div className="flex flex-wrap items-end justify-between gap-3"><div><p className="data-label text-primary">Agents</p><h2 className="mt-1 text-2xl font-semibold tracking-tight">Market participants</h2><p className="mt-1 text-sm text-muted-foreground">Configure and monitor agent strategies.</p></div><Badge variant="outline">{agents.length} active / stopping</Badge></div>
    <Card className="dashboard-card"><CardHeader><div className="flex items-center justify-between"><CardTitle>Participant fleet</CardTitle><Badge variant="secondary">{fleet.length} staged</Badge></div></CardHeader><CardContent className="space-y-4">
      <p className="text-sm text-muted-foreground">Each agent signs a one-time on-chain claim from a new address. Trading begins only after every claim commits.</p>
      <div className="flex flex-wrap gap-2"><Button variant="outline" disabled={launchingFleet} onClick={addRow}>Add agent</Button><Button variant="outline" disabled={launchingFleet} onClick={loadMeta}>Load 13-agent mix</Button><Button disabled={!snapshot?.running || fleet.length === 0 || launchingFleet} onClick={() => void command(launchFleet)}>{launchingFleet ? "Claiming accounts…" : `Claim & launch fleet (${fleet.length})`}</Button></div>
      {snapshot?.claim_progress && <p role="status" className="text-xs text-muted-foreground">Claims: {snapshot.claim_progress.confirmed}/{snapshot.claim_progress.requested} committed{snapshot.claim_progress.error ? ` · ${snapshot.claim_progress.error}` : ""}</p>}
      {fleet.length > 0 && <div className="overflow-x-auto"><Table><TableHeader><TableRow><TableHead>ID</TableHead><TableHead>Account</TableHead><TableHead>Preset</TableHead><TableHead>Target</TableHead><TableHead /></TableRow></TableHeader><TableBody>{fleet.map((config, index) => <TableRow key={index}><TableCell>{config.agent_id}</TableCell><TableCell>{preparedWallets[index] ? `#${preparedWallets[index].label} · claimed` : "New address on launch"}</TableCell><TableCell>{names[config.strategy.kind]}</TableCell><TableCell>{config.target.kind}</TableCell><TableCell className="space-x-2 text-right"><Button size="sm" variant="outline" disabled={launchingFleet} onClick={() => { setDraft(config); setEditingFleetIndex(index); setSelectedId(null) }}>Edit</Button><Button size="sm" variant="ghost" disabled={launchingFleet} onClick={() => { setPreparedWallets([]); setFleet(rows => rows.filter((_, item) => item !== index)) }}>Remove</Button></TableCell></TableRow>)}</TableBody></Table></div>}
      {groups.map(groupId => <div key={groupId} className="flex items-center gap-3 text-sm"><Badge variant="outline">Group {groupId}</Badge><span>{agents.filter(agent => agent.group_id === groupId).length} agents</span><Button size="sm" variant="outline" onClick={() => command(() => api.stopAgents(agents.filter(agent => agent.group_id === groupId).map(agent => agent.agent_id)))}>Stop group</Button></div>)}
    </CardContent></Card>

    <div className="grid gap-5 xl:grid-cols-[minmax(0,1fr)_minmax(0,1.1fr)]">
      <Card className="dashboard-card"><CardHeader><CardTitle>Active agents</CardTitle><p className="text-xs text-muted-foreground">Select an agent to see its account, nonce, orders, and editable strategy.</p></CardHeader><CardContent><div className="grid gap-2 sm:grid-cols-2 xl:grid-cols-1 2xl:grid-cols-2">{agents.map(agent => <button type="button" key={agent.agent_id} onClick={() => editActive(agent.agent_id)} className={`min-w-0 rounded-lg border p-3 text-left transition-colors hover:border-primary/50 focus-visible:outline-2 focus-visible:outline-primary ${selectedId === agent.agent_id ? "border-primary/70 bg-primary/5" : "border-border bg-background/50"}`}><div className="flex items-start justify-between gap-2"><div><p className="font-semibold">Agent {agent.agent_id}</p><p className="text-xs text-muted-foreground">{names[agent.strategy.kind]}</p></div><Badge variant={agent.state === "active" ? "default" : "secondary"}>{agent.state}</Badge></div><div className="mt-3 grid grid-cols-3 gap-2 border-t border-border pt-2"><div><p className="data-label">Wallet</p><p className="mt-1 text-sm">#{agent.account_id}</p></div><div><p className="data-label">Nonce</p><p className="mt-1 text-sm">{agent.confirmed_nonce ?? "—"}</p></div><div><p className="data-label">Orders</p><p className="mt-1 text-sm">{agent.own_open_orders}</p></div></div><p className="mt-2 text-xs text-muted-foreground">BTC total {agent.balance ? agent.balance.btc_available + agent.balance.btc_locked : "—"}</p></button>)}{agents.length === 0 && <p className="col-span-full rounded-lg border border-dashed border-border p-8 text-center text-sm text-muted-foreground">No agents active. Add one or load the 13-agent mix.</p>}</div></CardContent></Card>
      <Card className="dashboard-card"><CardHeader><CardTitle>{editingFleetIndex !== null ? `Edit fleet row ${editingFleetIndex + 1}` : selected ? `Agent ${selected.agent_id}` : "New agent"}</CardTitle></CardHeader><CardContent className="space-y-4">
        {selected && <div className="grid gap-2 text-sm sm:grid-cols-2"><span>Confirmed nonce: {selected.confirmed_nonce ?? "—"}</span><span>Pending: {selected.pending_action ?? (selected.pending ? "transaction" : "none")}</span><span>BTC available / locked: {selected.balance ? `${selected.balance.btc_available} / ${selected.balance.btc_locked}` : "—"}</span><span>USD available / locked: {selected.balance ? `${selected.balance.usd_available.toLocaleString()} / ${selected.balance.usd_locked.toLocaleString()}` : "—"}</span><span>Open orders: {selected.own_open_orders}</span><span>Binance feed age: {selected.feed_age_ms === null ? "not used / waiting" : `${selected.feed_age_ms} ms`}</span>{selected.last_error && <span role="alert" className="text-destructive sm:col-span-2">{selected.last_error}</span>}</div>}
        <Editor config={draft} onChange={setDraft} nodes={snapshot?.nodes ?? []} lockIdentity={!!selected} />
        <div className="flex flex-wrap gap-2"><Button disabled={!snapshot?.running || launchingFleet} onClick={() => void command(saveDraft)}>{editingFleetIndex !== null ? "Save fleet row" : selected ? "Update live" : "Claim & activate agent"}</Button>{selected && <Button variant="outline" onClick={() => command(() => api.stopAgent(selected.agent_id))}>Stop and clear orders</Button>}<Button variant="ghost" onClick={() => { setSelectedId(null); setEditingFleetIndex(null); setDraft(newConfig(nextId(), 0)) }}>New agent</Button></div>
      </CardContent></Card>
    </div>

    <details className="dashboard-card rounded-lg border border-border p-4"><summary className="cursor-pointer text-sm font-medium">Advanced transfer traffic</summary><div className="space-y-3 pt-4"><p className="text-xs text-muted-foreground">The existing transfer sender is separate from the market participants. Use accounts that are not assigned to agents.</p><div className="grid gap-3 sm:grid-cols-3"><Numeric label="First account" value={transferFirst} set={setTransferFirst} /><Numeric label="Account count" value={transferCount} set={setTransferCount} /><Numeric label="Target TPS" value={transferTps} set={setTransferTps} /></div><div className="flex gap-2"><Button disabled={!snapshot?.running} onClick={() => command(startTransfers)}>Start / update</Button><Button variant="outline" disabled={!snapshot?.load.running} onClick={() => command(api.stopLoad)}>Stop</Button></div></div></details>
  </div>
}
