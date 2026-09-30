import { useEffect, useMemo, useState, type ReactNode } from "react"
import { Activity, Boxes, Clock3, Gauge, Pause, Play, Send, Skull, Square } from "lucide-react"
import { api } from "@/api"
import { OrderBookView } from "@/OrderBookView"
import { ParticipantsTab } from "@/ParticipantsTab"
import type { ControlSnapshot } from "@/bindings/ControlSnapshot"
import type { LaunchConfig } from "@/bindings/LaunchConfig"
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
import { Separator } from "@/components/ui/separator"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"

type TxTab = "transfer" | "place" | "cancel"
type NodeOption = { node_id: number; state: string }

const targetFromString = (value: string): TargetNode => value === "random" ? { kind: "random" } : value === "primary" ? { kind: "primary" } : { kind: "node", node_id: Number(value.slice(5)) }

function NumberField({ label, value, onChange }: { label: string; value: number; onChange: (value: number) => void }) {
  return <div className="space-y-2"><Label>{label}</Label><Input type="number" value={value} onChange={(event) => onChange(Number(event.target.value))} /></div>
}
function MetricCard({ title, value, icon }: { title: string; value: string; icon?: ReactNode }) {
  return <Card><CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2"><CardTitle className="text-sm font-medium text-muted-foreground">{title}</CardTitle>{icon}</CardHeader><CardContent><div className="text-2xl font-semibold tracking-tight">{value}</div></CardContent></Card>
}
function TargetSelect({ value, onChange, nodes }: { value: string; onChange: (value: string) => void; nodes: NodeOption[] }) {
  return <Select value={value} onValueChange={(next) => next && onChange(next)}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="random">Random node</SelectItem><SelectItem value="primary">Current primary</SelectItem>{nodes.map((node) => <SelectItem key={node.node_id} value={`node:${node.node_id}`}>Node {node.node_id}</SelectItem>)}</SelectContent></Select>
}

function App() {
  const [config, setConfig] = useState<LaunchConfig | null>(null)
  const [snapshot, setSnapshot] = useState<ControlSnapshot | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [txTab, setTxTab] = useState<TxTab>("transfer")
  const [txTarget, setTxTarget] = useState("random")
  const [from, setFrom] = useState(0); const [to, setTo] = useState(1); const [asset, setAsset] = useState(0); const [amount, setAmount] = useState(100); const [transferNonce, setTransferNonce] = useState(0)
  const [accountId, setAccountId] = useState(0); const [marketId, setMarketId] = useState(0); const [side, setSide] = useState<Side>("Buy"); const [orderType, setOrderType] = useState<OrderType>("LO"); const [price, setPrice] = useState(100_000); const [quantity, setQuantity] = useState(1); const [orderNonce, setOrderNonce] = useState(0)
  const [cancelAccount, setCancelAccount] = useState(0); const [cancelMarket, setCancelMarket] = useState(0); const [orderId, setOrderId] = useState(0); const [cancelNonce, setCancelNonce] = useState(0)
  const [faultTarget, setFaultTarget] = useState("primary"); const [pauseMs, setPauseMs] = useState(5000)

  useEffect(() => { api.defaultConfig().then(setConfig).catch((err) => setError(String(err))); const refresh = () => api.snapshot().then(setSnapshot).catch(() => undefined); refresh(); const timer = window.setInterval(refresh, 500); return () => window.clearInterval(timer) }, [])
  const nodes = snapshot?.nodes ?? []
  const availableNodes = useMemo(() => nodes.filter((node) => node.state !== "dead"), [nodes])
  const command = async (operation: () => Promise<void>) => { try { setError(null); await operation() } catch (err) { setError(String(err)) } }
  const buildTransaction = (): Transaction => {
    switch (txTab) {
      case "transfer": return { Transfer: { from, to, asset, amount, nonce: transferNonce } }
      case "place": return { PlaceOrder: { account_id: accountId, market_id: marketId, order_type: orderType, side, price, quantity, nonce: orderNonce } }
      case "cancel": return { CancelOrder: { account_id: cancelAccount, market_id: cancelMarket, order_id: orderId, nonce: cancelNonce } }
    }
  }
  const metrics = snapshot?.metrics
  const selectConfigNumber = (key: "node_count" | "block_time_ms" | "timeout_ms" | "poll_ms") => (value: number) => config && setConfig({ ...config, [key]: value })
  return <div className="min-h-screen bg-muted/30"><main className="mx-auto max-w-[1600px] space-y-6 p-6">
    <header className="flex items-center justify-between"><div><h1 className="text-3xl font-semibold tracking-tight">Light DEX</h1><p className="text-sm text-muted-foreground">Blockchain control plane and benchmark dashboard</p></div><Badge variant={snapshot?.running ? "default" : "secondary"}>{snapshot?.stopping ? "Stopping" : snapshot?.launching ? "Launching" : snapshot?.running ? "Running" : "Stopped"}</Badge></header>
    {error && <Card className="border-destructive"><CardContent className="pt-6 text-sm text-destructive">{error}</CardContent></Card>}
    {config && <Card><CardHeader><CardTitle>Simulation</CardTitle></CardHeader><CardContent className="space-y-4"><div className="grid gap-4 md:grid-cols-2"><div className="space-y-2"><Label>Project directory</Label><Input value={config.project_dir} onChange={(event) => setConfig({ ...config, project_dir: event.target.value })} /></div><div className="space-y-2"><Label>Node binary</Label><Input value={config.binary_path} onChange={(event) => setConfig({ ...config, binary_path: event.target.value })} /></div></div><div className="grid gap-4 md:grid-cols-2"><div className="space-y-2"><Label>Node data directory</Label><Input value={config.data_dir} onChange={(event) => setConfig({ ...config, data_dir: event.target.value })} /><p className="text-xs text-muted-foreground">Fresh launches create a new run folder here. To recover, enter the exact run folder shown below.</p></div><div className="space-y-2"><Label className="flex items-center gap-2"><input type="checkbox" checked={config.recover_existing} onChange={(event) => setConfig({ ...config, recover_existing: event.target.checked })} />Recover existing run</Label><p className="break-all text-xs text-muted-foreground">Active run: {snapshot?.active_data_dir ?? "none"}</p></div></div><div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4"><NumberField label="Nodes" value={config.node_count} onChange={selectConfigNumber("node_count")} /><NumberField label="Block time (ms)" value={config.block_time_ms} onChange={selectConfigNumber("block_time_ms")} /><NumberField label="PBFT timeout (ms)" value={config.timeout_ms} onChange={selectConfigNumber("timeout_ms")} /><NumberField label="Poll interval (ms)" value={config.poll_ms} onChange={selectConfigNumber("poll_ms")} /></div><div className="flex gap-2"><Button onClick={() => command(() => api.launch(config))} disabled={snapshot?.running || snapshot?.launching || snapshot?.stopping}><Play className="mr-2 h-4 w-4" />Launch</Button><Button variant="destructive" onClick={() => command(api.stopAll)} disabled={snapshot?.stopping || (!snapshot?.running && !snapshot?.launching)}><Square className="mr-2 h-4 w-4" />Stop all</Button></div></CardContent></Card>}
    <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-6"><MetricCard title="Alive" value={String(metrics?.alive_nodes ?? 0)} icon={<Activity className="h-4 w-4 text-muted-foreground" />} /><MetricCard title="Submitted" value={String(metrics?.submitted ?? 0)} icon={<Send className="h-4 w-4 text-muted-foreground" />} /><MetricCard title="Submit TPS" value={(metrics?.submit_tps ?? 0).toFixed(1)} /><MetricCard title="Commit TPS" value={(metrics?.commit_tps ?? 0).toFixed(1)} icon={<Gauge className="h-4 w-4 text-muted-foreground" />} /><MetricCard title="Blocks / sec" value={(metrics?.blocks_per_second ?? 0).toFixed(2)} icon={<Boxes className="h-4 w-4 text-muted-foreground" />} /><MetricCard title="Commit latency" value={`${(metrics?.commit_latency_ms ?? 0).toFixed(1)} ms`} icon={<Clock3 className="h-4 w-4 text-muted-foreground" />} /></div>
    <Tabs defaultValue="control" className="space-y-5">
      <TabsList><TabsTrigger value="control">Control plane</TabsTrigger><TabsTrigger value="participants">Participants</TabsTrigger></TabsList>
      <TabsContent value="control" className="space-y-6">
    <div className="grid gap-6 lg:grid-cols-[2fr_1fr]"><Card><CardHeader><CardTitle>Transactions</CardTitle></CardHeader><CardContent><Tabs value={txTab} onValueChange={(value) => setTxTab(value as TxTab)}><TabsList><TabsTrigger value="transfer">Transfer</TabsTrigger><TabsTrigger value="place">Place order</TabsTrigger><TabsTrigger value="cancel">Cancel order</TabsTrigger></TabsList><TabsContent value="transfer" className="pt-4"><div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-5"><NumberField label="From" value={from} onChange={setFrom} /><NumberField label="To" value={to} onChange={setTo} /><NumberField label="Asset" value={asset} onChange={setAsset} /><NumberField label="Amount" value={amount} onChange={setAmount} /><NumberField label="Nonce" value={transferNonce} onChange={setTransferNonce} /></div></TabsContent><TabsContent value="place" className="pt-4"><div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4"><NumberField label="Account" value={accountId} onChange={setAccountId} /><NumberField label="Market" value={marketId} onChange={setMarketId} /><div className="space-y-2"><Label>Side</Label><Select value={side} onValueChange={(value) => setSide(value as Side)}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="Buy">Buy</SelectItem><SelectItem value="Sell">Sell</SelectItem></SelectContent></Select></div><div className="space-y-2"><Label>Order type</Label><Select value={orderType} onValueChange={(value) => setOrderType(value as OrderType)}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="LO">Limit</SelectItem><SelectItem value="IOC">IOC</SelectItem><SelectItem value="FOK">FOK</SelectItem></SelectContent></Select></div><NumberField label="Price" value={price} onChange={setPrice} /><NumberField label="Quantity" value={quantity} onChange={setQuantity} /><NumberField label="Nonce" value={orderNonce} onChange={setOrderNonce} /></div></TabsContent><TabsContent value="cancel" className="pt-4"><div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4"><NumberField label="Account" value={cancelAccount} onChange={setCancelAccount} /><NumberField label="Market" value={cancelMarket} onChange={setCancelMarket} /><NumberField label="Order ID" value={orderId} onChange={setOrderId} /><NumberField label="Nonce" value={cancelNonce} onChange={setCancelNonce} /></div></TabsContent></Tabs><Separator className="my-5" /><div className="flex items-end gap-3"><div className="w-56 space-y-2"><Label>Submit to</Label><TargetSelect value={txTarget} onChange={setTxTarget} nodes={availableNodes} /></div><Button onClick={() => command(() => api.submitTransaction(buildTransaction(), targetFromString(txTarget)))} disabled={!snapshot?.running}><Send className="mr-2 h-4 w-4" />Submit</Button></div></CardContent></Card>
    <Card><CardHeader><CardTitle>Fault injection</CardTitle></CardHeader><CardContent className="space-y-4"><div className="space-y-2"><Label>Target</Label><TargetSelect value={faultTarget} onChange={setFaultTarget} nodes={availableNodes} /></div><NumberField label="Pause duration (ms)" value={pauseMs} onChange={setPauseMs} /><div className="grid grid-cols-2 gap-2"><Button variant="secondary" onClick={() => command(() => api.pauseNode(targetFromString(faultTarget), pauseMs))}><Pause className="mr-2 h-4 w-4" />Pause</Button><Button variant="destructive" onClick={() => command(() => api.killNode(targetFromString(faultTarget)))}><Skull className="mr-2 h-4 w-4" />Kill</Button></div></CardContent></Card></div>
    <OrderBookView snapshot={snapshot} />
    <Card><CardHeader><div className="flex flex-wrap items-center justify-between gap-2"><CardTitle>Nodes</CardTitle><div className="flex gap-2"><Badge variant="outline">Height {metrics?.height_min ?? "-"}/{metrics?.height_max ?? "-"}</Badge><Badge variant="outline">View {metrics?.views.join(", ") || "-"}</Badge><Badge variant="outline">Primary {metrics?.primaries.join(", ") || "-"}</Badge><Badge variant={metrics?.chain_consistency === "consistent" ? "default" : "destructive"}>{metrics?.chain_consistency ?? "unknown"}</Badge></div></div></CardHeader><CardContent><div className="overflow-x-auto"><Table><TableHeader><TableRow>{["Node", "PID", "State", "Peers", "View", "Primary", "Height", "Mempool", "Committed", "TPS", "Blocks/s"].map((title) => <TableHead key={title}>{title}</TableHead>)}</TableRow></TableHeader><TableBody>{nodes.map((node) => { const status = node.status; return <TableRow key={node.node_id}><TableCell className="font-medium">{node.node_id}</TableCell><TableCell>{node.pid}</TableCell><TableCell><Badge variant={node.state === "dead" ? "destructive" : node.state === "ok" ? "default" : "secondary"}>{node.state}</Badge></TableCell><TableCell>{status ? `${status.peer_count}/${status.expected_peers}` : "-"}</TableCell><TableCell>{status?.current_view ?? "-"}</TableCell><TableCell>{status?.current_primary ?? "-"}</TableCell><TableCell>{status?.chain_height ?? "-"}</TableCell><TableCell>{status?.mempool_size ?? "-"}</TableCell><TableCell>{status?.transactions_committed ?? "-"}</TableCell><TableCell>{node.tx_rate.toFixed(1)}</TableCell><TableCell>{node.block_rate.toFixed(2)}</TableCell></TableRow> })}</TableBody></Table></div></CardContent></Card>
    <Card><CardHeader><CardTitle>Controller / node logs</CardTitle></CardHeader><CardContent><ScrollArea className="h-72 rounded-md border bg-zinc-950 p-4"><div className="space-y-1 font-mono text-xs text-zinc-200">{(snapshot?.logs ?? []).map((line, index) => <div key={index}>{line}</div>)}</div></ScrollArea></CardContent></Card>
      </TabsContent>
      <TabsContent value="participants"><ParticipantsTab snapshot={snapshot} command={command} /></TabsContent>
    </Tabs>
  </main></div>
}
export default App
