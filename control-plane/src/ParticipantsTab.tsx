import { useState } from "react"
import { Play, Square } from "lucide-react"

import { api } from "@/api"
import type { ControlSnapshot } from "@/bindings/ControlSnapshot"
import type { LoadConfig } from "@/bindings/LoadConfig"
import type { MarketMakerConfig } from "@/bindings/MarketMakerConfig"
import type { Side } from "@/bindings/Side"
import type { TargetNode } from "@/bindings/TargetNode"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"

type NodeOption = { node_id: number; state: string }

function NumberField({ label, value, onChange, min }: { label: string; value: number; onChange: (value: number) => void; min?: number }) {
  return <div className="space-y-2"><Label>{label}</Label><Input type="number" min={min} value={value} onChange={(event) => onChange(Number(event.target.value))} /></div>
}

function TargetSelect({ value, onChange, nodes }: { value: string; onChange: (value: string) => void; nodes: NodeOption[] }) {
  return <Select value={value} onValueChange={(next) => next && onChange(next)}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="random">Random node</SelectItem><SelectItem value="primary">Current primary</SelectItem>{nodes.filter((node) => node.state !== "dead").map((node) => <SelectItem key={node.node_id} value={`node:${node.node_id}`}>Node {node.node_id}</SelectItem>)}</SelectContent></Select>
}

function targetFromString(value: string): TargetNode {
  return value === "random" ? { kind: "random" } : value === "primary" ? { kind: "primary" } : { kind: "node", node_id: Number(value.slice(5)) }
}

export function ParticipantsTab({ snapshot, command }: { snapshot: ControlSnapshot | null; command: (operation: () => Promise<void>) => Promise<void> }) {
  const [mode, setMode] = useState<"round_robin_transfers" | "random_transfers">("random_transfers")
  const [firstAccount, setFirstAccount] = useState(0)
  const [accountCount, setAccountCount] = useState(10)
  const [asset, setAsset] = useState(0)
  const [amount, setAmount] = useState(100)
  const [sendTps, setSendTps] = useState(5)
  const [senderTarget, setSenderTarget] = useState("random")
  const [takerAccount, setTakerAccount] = useState(80)
  const [takerSide, setTakerSide] = useState<Side>("Buy")
  const [takerTps, setTakerTps] = useState(1)
  const [takerTarget, setTakerTarget] = useState("primary")
  const [makerAccount, setMakerAccount] = useState(90)
  const [makerLevels, setMakerLevels] = useState(3)
  const [makerRefresh, setMakerRefresh] = useState(1000)
  const [makerTarget, setMakerTarget] = useState("primary")

  const nodes = snapshot?.nodes ?? []
  const running = snapshot?.running ?? false
  const maker = snapshot?.market_maker

  const startSender = () => {
    const config: LoadConfig = {
      preset: { kind: mode, first_account: firstAccount, account_count: accountCount, asset, amount },
      target: targetFromString(senderTarget), tps: sendTps,
    }
    return api.startLoad(config)
  }
  const startTaker = () => {
    const config: LoadConfig = {
      preset: { kind: "book_taker", account_id: takerAccount, market_id: 0, side: takerSide },
      target: targetFromString(takerTarget), tps: takerTps,
    }
    return api.startLoad(config)
  }
  const startMaker = () => {
    const config: MarketMakerConfig = {
      account_id: makerAccount, levels: makerLevels, refresh_ms: makerRefresh,
      target: targetFromString(makerTarget),
    }
    return api.startMarketMaker(config)
  }

  return <Tabs defaultValue="maker" className="space-y-4">
    <div className="flex flex-wrap items-center gap-3"><TabsList>
      <TabsTrigger value="maker">Market maker</TabsTrigger>
      <TabsTrigger value="random">Random transactions</TabsTrigger>
      <TabsTrigger value="taker">Taker</TabsTrigger>
    </TabsList><Badge variant="secondary">Sender: {snapshot?.load.kind ?? "stopped"}</Badge></div>

    <TabsContent value="maker">
      <Card>
        <CardHeader><div className="flex items-center justify-between"><CardTitle>Binance-mirroring market maker</CardTitle><Badge variant={maker?.running ? "default" : "secondary"}>{maker?.stopping ? "Clearing quotes" : maker?.running ? "Active" : "Inactive"}</Badge></div></CardHeader>
        <CardContent className="space-y-5">
          <p className="text-sm text-muted-foreground">Subscribes to Binance Spot BTCUSDT top-5 depth, then updates this simulator's BTC-USD quotes at your interval. Each resting level is exactly 1 BTC. Binance USDT is treated as nominal USD, and prices are mapped to whole-dollar chain ticks.</p>
          <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
            <NumberField label="Participant account" value={makerAccount} onChange={setMakerAccount} min={0} />
            <NumberField label="Levels per side (1–5)" value={makerLevels} onChange={setMakerLevels} min={1} />
            <NumberField label="Quote refresh (ms)" value={makerRefresh} onChange={setMakerRefresh} min={250} />
            <div className="space-y-2"><Label>Submit to</Label><TargetSelect value={makerTarget} onChange={setMakerTarget} nodes={nodes} /></div>
          </div>
          <div className="flex flex-wrap gap-2">
            <Button onClick={() => command(startMaker)} disabled={!running || maker?.stopping}><Play className="mr-2 h-4 w-4" />{maker?.running ? "Update maker" : "Activate maker"}</Button>
            <Button variant="outline" onClick={() => command(api.stopMarketMaker)} disabled={!maker?.running}><Square className="mr-2 h-4 w-4" />Deactivate and cancel quotes</Button>
          </div>
          <div className="grid gap-3 text-sm sm:grid-cols-2 lg:grid-cols-4">
            <div>Feed age: <span className="font-mono">{maker?.feed_age_ms === null || maker?.feed_age_ms === undefined ? "waiting" : `${maker.feed_age_ms} ms`}</span></div>
            <div>Binance update: <span className="font-mono">{maker?.last_update_id ?? "—"}</span></div>
            <div>Own open orders: <span className="font-mono">{maker?.own_open_orders ?? 0}</span></div>
            <div>Pending nonce: <span className="font-mono">{maker?.pending ? "yes" : "no"}</span></div>
          </div>
          <div className="grid gap-3 text-sm sm:grid-cols-2">
            <div>Desired bids: <span className="font-mono">{maker?.desired_bids.join(", ") || "—"}</span></div>
            <div>Desired asks: <span className="font-mono">{maker?.desired_asks.join(", ") || "—"}</span></div>
          </div>
          {maker?.last_error && <p role="alert" className="text-sm text-destructive">{maker.last_error}</p>}
          <p className="text-xs text-muted-foreground">The maker uses its own account and confirms each nonce before its next cancel/place action. Use a different account for the taker or transfer sender. Deactivation cancels its resting quotes; if a transaction fails, inspect the remaining orders before restarting.</p>
        </CardContent>
      </Card>
    </TabsContent>

    <TabsContent value="random">
      <Card><CardHeader><CardTitle>Transfer sender</CardTitle></CardHeader><CardContent className="space-y-4">
        <p className="text-sm text-muted-foreground">Send from multiple accounts at a target rate. Different accounts can have transactions in the same block; the target TPS is not a guaranteed commit rate.</p>
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          <div className="space-y-2"><Label>Behavior</Label><Select value={mode} onValueChange={(value) => value && setMode(value as typeof mode)}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="random_transfers">Random account pairs</SelectItem><SelectItem value="round_robin_transfers">Accounts in rotation</SelectItem></SelectContent></Select></div>
          <NumberField label="First account" value={firstAccount} onChange={setFirstAccount} min={0} />
          <NumberField label="Account count" value={accountCount} onChange={setAccountCount} min={2} />
          <NumberField label="Target TPS" value={sendTps} onChange={setSendTps} min={1} />
          <NumberField label="Asset ID" value={asset} onChange={setAsset} min={0} />
          <NumberField label="Amount" value={amount} onChange={setAmount} min={1} />
          <div className="space-y-2"><Label>Send to</Label><TargetSelect value={senderTarget} onChange={setSenderTarget} nodes={nodes} /></div>
        </div>
        <div className="flex gap-2"><Button onClick={() => command(startSender)} disabled={!running}><Play className="mr-2 h-4 w-4" />Start / update sender</Button><Button variant="outline" onClick={() => command(api.stopLoad)} disabled={!snapshot?.load.running || snapshot.load.kind === "book taker"}>Stop sender</Button></div>
        <p className="text-xs text-muted-foreground">One nonce is pending per account; use multiple accounts to batch. Pending accounts: {snapshot?.load.pending_accounts ?? 0}. Failed accounts: {snapshot?.load.failed_accounts.join(", ") || "none"}.</p>
      </CardContent></Card>
    </TabsContent>

    <TabsContent value="taker">
      <Card><CardHeader><CardTitle>Book taker</CardTitle></CardHeader><CardContent className="space-y-4">
        <p className="text-sm text-muted-foreground">Repeatedly submits a 1 BTC immediate-or-cancel order at the best visible opposite price. It waits for its account nonce before sending the next order.</p>
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          <NumberField label="Taker account" value={takerAccount} onChange={setTakerAccount} min={0} />
          <div className="space-y-2"><Label>Side</Label><Select value={takerSide} onValueChange={(value) => value && setTakerSide(value as Side)}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="Buy">Buy</SelectItem><SelectItem value="Sell">Sell</SelectItem></SelectContent></Select></div>
          <NumberField label="Target TPS" value={takerTps} onChange={setTakerTps} min={1} />
          <div className="space-y-2"><Label>Send to</Label><TargetSelect value={takerTarget} onChange={setTakerTarget} nodes={nodes} /></div>
        </div>
        <div className="flex gap-2"><Button onClick={() => command(startTaker)} disabled={!running}><Play className="mr-2 h-4 w-4" />Start / update taker</Button><Button variant="outline" onClick={() => command(api.stopLoad)} disabled={snapshot?.load.kind !== "book taker"}>Stop taker</Button></div>
        <p className="text-xs text-muted-foreground">The transfer sender and taker share one load-generator slot; starting either replaces the other. Keep the taker on a different account from the maker.</p>
      </CardContent></Card>
    </TabsContent>
  </Tabs>
}
