import { useMemo, useState } from "react"
import { Pause, Play, Send, Skull, Square, Wallet } from "lucide-react"
import { api } from "@/api"
import type { ControlSnapshot } from "@/bindings/ControlSnapshot"
import type { ConsensusAlgorithm } from "@/bindings/ConsensusAlgorithm"
import type { LaunchConfig } from "@/bindings/LaunchConfig"
import type { PollMode } from "@/bindings/PollMode"
import type { OrderType } from "@/bindings/OrderType"
import type { Side } from "@/bindings/Side"
import type { TargetNode } from "@/bindings/TargetNode"
import type { Transaction } from "@/bindings/Transaction"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { ScrollArea } from "@/components/ui/scroll-area"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"

type TxTab = "transfer" | "place" | "cancel"
type Props = {
  snapshot: ControlSnapshot | null
  config: LaunchConfig | null
  setConfig: (config: LaunchConfig) => void
  command: (operation: () => Promise<void>) => Promise<void>
  onLaunch: () => Promise<void>
  onStop: () => Promise<void>
}

function NumberField({ label, value, onChange }: { label: string; value: number; onChange: (value: number) => void }) {
  return <div className="space-y-1.5"><Label>{label}</Label><Input type="number" value={value} onChange={event => onChange(Number(event.target.value))} /></div>
}

function targetFromString(value: string): TargetNode {
  return value === "random" || value === "primary" ? { kind: value } : { kind: "node", node_id: Number(value.slice(5)) }
}

function TargetSelect({ value, onChange, snapshot }: { value: string; onChange: (value: string) => void; snapshot: ControlSnapshot | null }) {
  return <Select value={value} onValueChange={next => next && onChange(next)}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="random">Random node</SelectItem><SelectItem value="primary">Current primary</SelectItem>{snapshot?.nodes.filter(node => node.state !== "dead").map(node => <SelectItem key={node.node_id} value={`node:${node.node_id}`}>Node {node.node_id}</SelectItem>)}</SelectContent></Select>
}

export function OperationsTab({ snapshot, config, setConfig, command, onLaunch, onStop }: Props) {
  const [txTab, setTxTab] = useState<TxTab>("transfer")
  const [txTarget, setTxTarget] = useState("random")
  const [from, setFrom] = useState(0), [toAddress, setToAddress] = useState(""), [asset, setAsset] = useState(0), [amount, setAmount] = useState(100), [transferNonce, setTransferNonce] = useState(0)
  const [accountId, setAccountId] = useState(0), [marketId, setMarketId] = useState(0), [side, setSide] = useState<Side>("Buy"), [orderType, setOrderType] = useState<OrderType>("LO"), [price, setPrice] = useState(100_000), [quantity, setQuantity] = useState(1), [orderNonce, setOrderNonce] = useState(0)
  const [cancelAccount, setCancelAccount] = useState(0), [cancelMarket, setCancelMarket] = useState(0), [orderId, setOrderId] = useState(0), [cancelNonce, setCancelNonce] = useState(0)
  const [faultTarget, setFaultTarget] = useState("primary"), [pauseMs, setPauseMs] = useState(5000)
  const [airdropFrom, setAirdropFrom] = useState(0), [airdropAmount, setAirdropAmount] = useState(100), [airdropAsset, setAirdropAsset] = useState(0)
  const [lastCreated, setLastCreated] = useState<string | null>(null)
  const [walletFilter, setWalletFilter] = useState("")
  const [visibleLogs, setVisibleLogs] = useState(200)
  const bestStatus = snapshot?.nodes.filter(node => node.state === "ok" && node.status).sort((a, b) => (b.status?.chain_height ?? 0) - (a.status?.chain_height ?? 0))[0]?.status
  const balances = useMemo(() => new Map(bestStatus?.agent_balances.map(balance => [balance.account_id, balance]) ?? []), [bestStatus])
  const wallets = snapshot?.wallet.filter(wallet => `#${wallet.label} ${wallet.address}`.toLowerCase().includes(walletFilter.toLowerCase())) ?? []
  const logs = snapshot?.logs ?? []
  const shownLogs = logs.slice(-visibleLogs)

  const setConfigNumber = (key: "node_count" | "block_time_ms" | "timeout_ms" | "poll_ms") => (value: number) => config && setConfig({ ...config, [key]: value })
  const addressOf = (label: number) => {
    const address = snapshot?.wallet.find(wallet => wallet.label === label)?.address
    if (!address) throw new Error(`Wallet #${label} does not exist`)
    return address
  }
  const buildTransaction = (): Transaction => {
    switch (txTab) {
      case "transfer": return { Transfer: { from: addressOf(from), to: toAddress.trim(), asset, amount, nonce: transferNonce } }
      case "place": return { PlaceOrder: { account_id: addressOf(accountId), market_id: marketId, order_type: orderType, side, price, quantity, nonce: orderNonce } }
      case "cancel": return { CancelOrder: { account_id: addressOf(cancelAccount), market_id: cancelMarket, order_id: orderId, nonce: cancelNonce } }
    }
  }

  return <div className="space-y-5">
    <div><p className="data-label text-primary">Configuration</p><h2 className="mt-1 text-2xl font-semibold tracking-tight">Operations</h2><p className="mt-1 text-sm text-muted-foreground">Cluster settings, wallets, transactions, and logs.</p></div>
    {config && <Card className="dashboard-card"><CardHeader><div className="flex flex-wrap items-center justify-between gap-3"><CardTitle>Simulation</CardTitle><Badge variant={snapshot?.running ? "default" : "secondary"}>{snapshot?.stopping ? "Stopping" : snapshot?.launching ? "Launching" : snapshot?.running ? "Running" : "Stopped"}</Badge></div></CardHeader><CardContent className="space-y-5">
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4"><NumberField label="Nodes" value={config.node_count} onChange={setConfigNumber("node_count")} /><NumberField label="Block time (ms)" value={config.block_time_ms} onChange={setConfigNumber("block_time_ms")} /><NumberField label="Consensus timeout (ms)" value={config.timeout_ms} onChange={setConfigNumber("timeout_ms")} /><NumberField label="Poll interval (ms)" value={config.poll_ms} onChange={setConfigNumber("poll_ms")} /></div>
      <div className="grid gap-4 sm:grid-cols-2"><div className="space-y-1.5"><Label>Consensus algorithm</Label><Select value={config.consensus} onValueChange={value => value && setConfig({ ...config, consensus: value as ConsensusAlgorithm, recover_existing: false })}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="pbft">PBFT</SelectItem><SelectItem value="hotstuff">HotStuff</SelectItem></SelectContent></Select></div><div className="space-y-1.5"><Label>Status polling</Label><Select value={config.poll_mode} onValueChange={value => value && setConfig({ ...config, poll_mode: value as PollMode })}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="round_robin">One node per interval</SelectItem><SelectItem value="all">All nodes per interval</SelectItem></SelectContent></Select><p className="text-xs text-muted-foreground">Round robin refreshes each node about every {(config.poll_ms * config.node_count / 1000).toFixed(1)} s.</p></div></div>
      <details className="rounded-lg border border-border bg-background/40 p-3"><summary className="cursor-pointer text-sm font-medium">Paths and session security</summary><div className="mt-4 grid gap-4 md:grid-cols-2"><div className="space-y-1.5"><Label>Project directory</Label><Input value={config.project_dir} onChange={event => setConfig({ ...config, project_dir: event.target.value })} /></div><div className="space-y-1.5"><Label>Node binary</Label><Input value={config.binary_path} onChange={event => setConfig({ ...config, binary_path: event.target.value })} /><p className="text-xs text-muted-foreground">Release builds are recommended for larger HotStuff clusters.</p></div><div className="space-y-1.5"><Label>Node data directory</Label><Input value={config.data_dir} onChange={event => setConfig({ ...config, data_dir: event.target.value })} /></div><div className="space-y-1.5 text-xs text-muted-foreground"><p>Chain ID: <span className="break-all text-foreground">{bestStatus?.chain_id || "—"}</span></p><p>Ed25519 signing: {bestStatus?.signing_enabled ? "enabled" : "waiting"}</p><p>Rejected authentication: {snapshot?.nodes.reduce((sum, node) => sum + (node.status?.authentication_rejections ?? 0), 0) ?? 0}</p><p>Session: <span className="break-all">{snapshot?.active_data_dir ?? "none"}</span></p><p>Signed sessions cannot restart validators until vote and lock state is durable.</p></div></div></details>
      <div className="flex gap-2"><Button onClick={() => void onLaunch()} disabled={snapshot?.running || snapshot?.launching || snapshot?.stopping}><Play />Launch cluster</Button><Button variant="destructive" onClick={() => void onStop()} disabled={snapshot?.stopping || (!snapshot?.running && !snapshot?.launching)}><Square />Stop all</Button></div>
    </CardContent></Card>}

    <div className="grid gap-5 xl:grid-cols-[1fr_1.15fr]"><Card className="dashboard-card"><CardHeader><CardTitle>Wallet addresses</CardTitle></CardHeader><CardContent className="space-y-4"><p className="text-xs text-muted-foreground">Wallet numbers are local labels. Nodes see only addresses; signing keys stay with the controller.</p><div className="grid gap-3 sm:grid-cols-3"><NumberField label="Fund from #" value={airdropFrom} onChange={setAirdropFrom} /><div className="space-y-1.5"><Label>Asset</Label><Select value={String(airdropAsset)} onValueChange={value => setAirdropAsset(Number(value))}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="0">USD</SelectItem><SelectItem value="1">BTC</SelectItem></SelectContent></Select></div><NumberField label="Amount" value={airdropAmount} onChange={setAirdropAmount} /></div><div className="flex flex-wrap gap-2"><Button size="sm" variant="secondary" disabled={!snapshot?.running} onClick={() => void command(async () => { const wallet = await api.createAddress(); setLastCreated(wallet.address); setToAddress(wallet.address) })}><Wallet />Create address</Button><Button size="sm" disabled={!snapshot?.running} onClick={() => void command(async () => { const wallet = await api.airdropNewAddress(airdropFrom, airdropAsset, airdropAmount, { kind: "primary" }); setLastCreated(wallet.address); setToAddress(wallet.address) })}>Airdrop new address</Button></div>{lastCreated && <p className="break-all text-xs text-primary">Latest: {lastCreated}</p>}<Input aria-label="Search wallet" placeholder="Search wallet number or address" value={walletFilter} onChange={event => setWalletFilter(event.target.value)} /><ScrollArea className="h-64 rounded-lg border border-border bg-background/50"><div className="divide-y divide-border/70">{wallets.map(wallet => { const balance = balances.get(wallet.address); return <div key={wallet.label} className="flex items-center gap-3 px-3 py-2 text-xs"><span className="w-10 shrink-0 font-semibold text-primary">#{wallet.label}</span><code className="min-w-0 flex-1 truncate" title={wallet.address}>{wallet.address}</code><span className="shrink-0 text-muted-foreground">{balance ? `${balance.usd_available} USD · ${balance.btc_available} BTC` : "Unfunded"}</span><Button size="xs" variant="ghost" onClick={() => setToAddress(wallet.address)}>Use</Button></div> })}{wallets.length === 0 && <p className="p-4 text-sm text-muted-foreground">No matching wallets.</p>}</div></ScrollArea></CardContent></Card>
    <Card className="dashboard-card"><CardHeader><CardTitle>Manual transaction</CardTitle></CardHeader><CardContent className="space-y-4"><Tabs value={txTab} onValueChange={value => setTxTab(value as TxTab)}><TabsList><TabsTrigger value="transfer">Transfer</TabsTrigger><TabsTrigger value="place">Place order</TabsTrigger><TabsTrigger value="cancel">Cancel order</TabsTrigger></TabsList><TabsContent value="transfer" className="pt-4"><div className="grid gap-3 sm:grid-cols-2"><NumberField label="From wallet #" value={from} onChange={setFrom} /><div className="space-y-1.5"><Label>To address</Label><Input value={toAddress} onChange={event => setToAddress(event.target.value.trim())} placeholder="64-character address" /></div><NumberField label="Asset" value={asset} onChange={setAsset} /><NumberField label="Amount" value={amount} onChange={setAmount} /><NumberField label="Nonce" value={transferNonce} onChange={setTransferNonce} /></div></TabsContent><TabsContent value="place" className="pt-4"><div className="grid gap-3 sm:grid-cols-2"><NumberField label="Wallet #" value={accountId} onChange={setAccountId} /><NumberField label="Market" value={marketId} onChange={setMarketId} /><div className="space-y-1.5"><Label>Side</Label><Select value={side} onValueChange={value => setSide(value as Side)}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="Buy">Buy</SelectItem><SelectItem value="Sell">Sell</SelectItem></SelectContent></Select></div><div className="space-y-1.5"><Label>Order type</Label><Select value={orderType} onValueChange={value => setOrderType(value as OrderType)}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="LO">Limit</SelectItem><SelectItem value="IOC">IOC</SelectItem><SelectItem value="FOK">FOK</SelectItem></SelectContent></Select></div><NumberField label="Price" value={price} onChange={setPrice} /><NumberField label="Quantity" value={quantity} onChange={setQuantity} /><NumberField label="Nonce" value={orderNonce} onChange={setOrderNonce} /></div></TabsContent><TabsContent value="cancel" className="pt-4"><div className="grid gap-3 sm:grid-cols-2"><NumberField label="Wallet #" value={cancelAccount} onChange={setCancelAccount} /><NumberField label="Market" value={cancelMarket} onChange={setCancelMarket} /><NumberField label="Order ID" value={orderId} onChange={setOrderId} /><NumberField label="Nonce" value={cancelNonce} onChange={setCancelNonce} /></div></TabsContent></Tabs><div className="flex items-end gap-3 border-t pt-4"><div className="w-44 space-y-1.5"><Label>Submit to</Label><TargetSelect value={txTarget} onChange={setTxTarget} snapshot={snapshot} /></div><Button disabled={!snapshot?.running} onClick={() => void command(() => api.submitTransaction(buildTransaction(), targetFromString(txTarget)))}><Send />Submit</Button></div></CardContent></Card></div>

    <div className="grid gap-5 xl:grid-cols-[330px_minmax(0,1fr)]"><Card className="dashboard-card"><CardHeader><CardTitle>Fault injection</CardTitle></CardHeader><CardContent className="space-y-3"><div className="space-y-1.5"><Label>Target</Label><TargetSelect value={faultTarget} onChange={setFaultTarget} snapshot={snapshot} /></div><NumberField label="Pause duration (ms)" value={pauseMs} onChange={setPauseMs} /><div className="flex gap-2"><Button variant="secondary" disabled={!snapshot?.running || pauseMs < 1} onClick={() => void command(() => api.pauseNode(targetFromString(faultTarget), pauseMs))}><Pause />Pause</Button><Button variant="destructive" disabled={!snapshot?.running} onClick={() => { if (window.confirm(`Kill ${faultTarget === "primary" ? "the current primary" : faultTarget === "random" ? "a random node" : faultTarget.replace(":", " ")}?`)) void command(() => api.killNode(targetFromString(faultTarget))) }}><Skull />Kill</Button></div><p className="text-xs text-muted-foreground">For a specific node, use its card on Overview for status and targeted actions.</p></CardContent></Card>
    <Card className="dashboard-card"><CardHeader><div className="flex items-center justify-between"><CardTitle>Controller / node logs</CardTitle><Badge variant="outline">{logs.length} buffered</Badge></div></CardHeader><CardContent><ScrollArea className="h-72 rounded-lg border border-border bg-[#070b0f] p-3"><div className="space-y-1 font-mono text-[11px] leading-relaxed text-slate-300">{shownLogs.map((line, index) => <div key={`${logs.length - shownLogs.length + index}-${line.slice(0, 20)}`}>{line}</div>)}{shownLogs.length === 0 && <p className="text-muted-foreground">No logs yet.</p>}</div></ScrollArea>{visibleLogs < logs.length && <Button className="mt-3" size="sm" variant="ghost" onClick={() => setVisibleLogs(count => count + 200)}>Show older logs</Button>}</CardContent></Card></div>
  </div>
}
