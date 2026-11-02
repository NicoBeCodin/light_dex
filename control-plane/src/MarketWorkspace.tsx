import { useCallback, useEffect, useMemo, useState } from "react"
import { Activity, CandlestickChart, CircleDot, Send } from "lucide-react"
import { api, nodeApi } from "@/api"
import { MarketActivity, type LiquidationData } from "@/MarketActivity"
import { formatAtoms, parseAtoms } from "@/units"
import { marketName } from "@/marketLabels"
import type { ControlSnapshot } from "@/bindings/ControlSnapshot"
import type { MarketConfig } from "@/bindings/MarketConfig"
import type { OrderBookSnapshot } from "@/bindings/OrderBookSnapshot"
import type { Trade } from "@/bindings/Trade"
import type { PerpPositionSnapshot } from "@/bindings/PerpPositionSnapshot"
import type { PerpRiskSnapshot } from "@/bindings/PerpRiskSnapshot"
import type { OpenOrderSnapshot } from "@/bindings/OpenOrderSnapshot"
import type { AgentBalance } from "@/bindings/AgentBalance"
import type { OrderType } from "@/bindings/OrderType"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"

type MarketData = { node_id: number; height: number; digest: string; market_id: number; config: MarketConfig;
  book: OrderBookSnapshot; recent_trades: Trade[]; committed_mark_e8: number | null }
type AccountData = { node_id: number; height: number; digest: string; address: string; nonce: number;
  balances: AgentBalance["assets"]; positions: PerpPositionSnapshot[]; risk: PerpRiskSnapshot[];
  open_orders: OpenOrderSnapshot[]; recent_transaction_outcomes: { digest: string; success: boolean; error: string | null }[] }

function useNodeStream<T>(nodeId: number | null, scope: "markets" | "accounts" | "liquidations", key: string | number | null, onFailure?: (nodeId: number) => void) {
  const [data, setData] = useState<T | null>(null)
  const [connected, setConnected] = useState(false)
  useEffect(() => {
    setData(null)
    setConnected(false)
    if (nodeId === null || key === null) return
    let disposed = false
    let socket: WebSocket | undefined
    let retry: number | undefined
    let failures = 0
    const connect = async () => {
      try {
        const next = await nodeApi.subscribe(nodeId, scope, key)
        if (disposed) { next.close(); return }
        socket = next
        next.onopen = () => { failures = 0; setConnected(true) }
        next.onmessage = event => {
          try { setData(JSON.parse(event.data) as T) } catch { /* ignore malformed update */ }
        }
        next.onclose = () => {
          setConnected(false)
          if (!disposed && ++failures >= 2) onFailure?.(nodeId)
          if (!disposed) retry = window.setTimeout(connect, 1000)
        }
        next.onerror = () => next.close()
      } catch {
        if (!disposed && ++failures >= 2) onFailure?.(nodeId)
        if (!disposed) retry = window.setTimeout(connect, 1000)
      }
    }
    void connect()
    return () => { disposed = true; window.clearTimeout(retry); socket?.close() }
  }, [nodeId, scope, key, onFailure])
  return { data, connected }
}

type ChartPoint = { sequence: number; price: number; volume: number }
function chartFromTrades(trades: Trade[]): ChartPoint[] {
  const points: ChartPoint[] = []
  for (const trade of [...trades].reverse()) {
    const last = points[points.length - 1]
    if (last?.sequence === trade.block_sequence) {
      last.price = trade.price
      last.volume += trade.quantity
    } else {
      points.push({ sequence: trade.block_sequence, price: trade.price, volume: trade.quantity })
    }
  }
  return points
}
function PriceTrace({ points, decimals }: { points: ChartPoint[]; decimals: number }) {
  if (points.length === 0) return <div className="flex h-64 items-center justify-center rounded border border-dashed border-border text-sm text-muted-foreground">No committed executions in recent trade history.</div>
  const prices = points.map(point => point.price)
  const low = Math.min(...prices)
  const high = Math.max(...prices)
  const span = Math.max(1, high - low)
  const first = points[0].sequence
  const blockSpan = Math.max(1, points[points.length - 1].sequence - first)
  const x = (sequence: number) => Math.max(0, Math.min(100, (sequence - first) / blockSpan * 100))
  const path = points.map((point, index) => `${index === 0 ? "M" : "L"}${x(point.sequence)},${85 - (point.price - low) * 70 / span}`).join(" ")
    + (points.length === 1 ? ` L1,${85 - (points[0].price - low) * 70 / span}` : "")
  const maxVolume = Math.max(1, ...points.map(point => point.volume))
  return <div className="relative h-80 overflow-hidden rounded border border-border bg-[#090e12] xl:h-[27rem]">
    <div className="absolute left-3 top-3 z-10 text-xs text-muted-foreground">Committed execution price · recent trades</div>
    <div className="absolute left-3 bottom-2 z-10 text-xs text-muted-foreground">Block {first}</div>
    <div className="absolute right-3 bottom-2 z-10 text-xs text-muted-foreground">Block {points[points.length - 1].sequence}</div>
    <div className="absolute right-3 top-3 z-10 text-xs text-muted-foreground">{formatAtoms(high, decimals)} / {formatAtoms(low, decimals)}</div>
    <svg viewBox="0 0 100 100" preserveAspectRatio="none" className="h-full w-full" role="img" aria-label="Recent executed price trace">
      <defs><linearGradient id="trace-fill" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stopColor="#5bd6bc" stopOpacity="0.22" /><stop offset="1" stopColor="#5bd6bc" stopOpacity="0" /></linearGradient></defs>
      <path d={`${path} L${x(points[points.length - 1].sequence)},90 L0,90 Z`} fill="url(#trace-fill)" />
      {points.map(point => <rect key={point.sequence} x={x(point.sequence)} y={94 - point.volume / maxVolume * 10} width="0.35" height={point.volume / maxVolume * 10} fill="#387d70" />)}
      <path d={path} fill="none" stroke="#5bd6bc" strokeWidth="0.65" vectorEffect="non-scaling-stroke" />
    </svg>
  </div>
}

export function MarketWorkspace({ snapshot, onError, walletLabel, onWalletChange }: { snapshot: ControlSnapshot | null; onError: (error: string) => void; walletLabel: number | null; onWalletChange: (label: number) => void }) {
  const [marketId, setMarketId] = useState(1)
  const [nodePreference, setNodePreference] = useState<number | null>(null)
  const [failedNodes, setFailedNodes] = useState<number[]>([])
  const [orderType, setOrderType] = useState<OrderType>("LO")
  const [side, setSide] = useState<"Buy" | "Sell">("Buy")
  const [price, setPrice] = useState("")
  const [quantity, setQuantity] = useState("0.001")
  const [reduceOnly, setReduceOnly] = useState(false)
  const [amend, setAmend] = useState<{ id: number; remaining: string } | null>(null)
  const [activityTab, setActivityTab] = useState("book")
  const candidates = snapshot?.nodes.filter(node => node.state !== "dead" && node.read_api_address) ?? []
  const selectedNode = candidates.find(node => node.node_id === nodePreference && !failedNodes.includes(node.node_id))
    ?? [...candidates].sort((a, b) => a.node_id - b.node_id).find(node => !failedNodes.includes(node.node_id))
    ?? candidates[0]
  const nodeId = selectedNode?.node_id ?? null
  const failed = useCallback((id: number) => setFailedNodes(previous => previous.includes(id) ? previous : [...previous, id]), [])
  const wallets = snapshot?.wallet ?? []
  const wallet = wallets.find(entry => entry.label === walletLabel) ?? wallets[0]
  const address = wallet?.address ?? null
  const marketStream = useNodeStream<MarketData>(nodeId, "markets", marketId, failed)
  const accountStream = useNodeStream<AccountData>(nodeId, "accounts", address)
  const market = marketStream.data
  const chartPoints = useMemo(() => chartFromTrades(market?.market_id === marketId ? market.recent_trades : []), [market, marketId])
  const account = accountStream.data
  const catalog = selectedNode?.status?.markets ?? snapshot?.nodes.find(node => node.status)?.status?.markets ?? []
  const book = market?.book
  const marketConfig = market?.config ?? catalog.find(entry => entry.id === marketId)?.config
  const liquidationStream = useNodeStream<LiquidationData>(nodeId, "liquidations",
    activityTab === "liquidations" && marketConfig?.market_type.kind === "perp" ? marketId : null)
  const decimals = marketConfig?.price.decimals ?? 0
  const quantityDecimals = marketConfig?.quantity.decimals ?? 0
  const bestBid = book?.bids[0]?.price
  const bestAsk = book?.asks[0]?.price
  const agent = snapshot?.agents.find(entry => entry.account_id === wallet?.label)
  const selectedRisk = account?.risk.find(entry => entry.market_id === marketId || entry.market_id === null)
  const ownOrders = useMemo(() => account?.open_orders.filter(order => order.market_id === marketId) ?? [], [account, marketId])
  const canTrade = !!snapshot?.running && !!wallet && !!marketConfig && !!account

  async function submit() {
    if (!wallet || !marketConfig || !account) return
    try {
      const orderPrice = parseAtoms(price || (side === "Buy" ? formatAtoms(bestAsk ?? 0, decimals) : formatAtoms(bestBid ?? 0, decimals)), decimals, marketConfig.price.step)
      const orderQuantity = parseAtoms(quantity, quantityDecimals, marketConfig.quantity.step)
      const payload = { account_id: wallet.address, market_id: marketId, order_type: orderType, side,
        price: orderPrice, quantity: orderQuantity, nonce: account.nonce }
      const perp = marketConfig.market_type.kind === "perp"
      await api.submitTransaction(perp ? { PlacePerpOrder: { ...payload, reduce_only: reduceOnly } }
        : { PlaceOrder: payload }, { kind: "random" })
    } catch (error) { onError(String(error)) }
  }
  async function cancel(orderId: number) {
    if (!wallet || !account || nodeId === null) return
    try {
      await api.submitTransaction({ CancelOrder: { account_id: wallet.address, market_id: marketId, order_id: orderId, nonce: account.nonce } },
        { kind: "random" })
    } catch (error) { onError(String(error)) }
  }
  async function reduceOrder() {
    if (!amend || !wallet || !account || !marketConfig) return
    try {
      const remaining = parseAtoms(amend.remaining, quantityDecimals, marketConfig.quantity.step)
      const order = ownOrders.find(order => order.order_id === amend.id)
      if (!order || remaining <= 0 || remaining >= order.remaining_quantity) throw new Error("Enter a positive remaining size smaller than the current order")
      await api.submitTransaction({ ReduceOrder: { account_id: wallet.address, market_id: marketId,
        order_id: amend.id, remaining_quantity: remaining, nonce: account.nonce } }, { kind: "random" })
      setAmend(null)
    } catch (error) { onError(String(error)) }
  }
  const name = marketConfig ? marketName({ id: marketId, config: marketConfig }) : `Market ${marketId}`
  const formatPrice = (value: number | undefined) => value === undefined ? "—" : formatAtoms(value, decimals)
  return <div className="space-y-3">
    <div className="flex flex-wrap items-center justify-between gap-3 border-b border-border pb-3">
      <div className="flex items-center gap-3"><div className="min-w-52"><Select value={String(marketId)} onValueChange={value => value && setMarketId(Number(value))}><SelectTrigger className="h-10 min-w-52 border-border bg-card px-3 font-semibold text-foreground"><SelectValue>{name}</SelectValue></SelectTrigger><SelectContent>{catalog.map(entry => <SelectItem key={entry.id} value={String(entry.id)}>{marketName(entry)}</SelectItem>)}</SelectContent></Select></div><Badge variant="outline">{marketConfig?.market_type.kind ?? "—"}</Badge><span className="text-xs text-muted-foreground">Committed market data</span></div>
      <div className="flex items-center gap-2 text-xs"><CircleDot className={`size-3 ${marketStream.connected ? "text-emerald-400" : "text-muted-foreground"}`} /><span>{marketStream.connected ? "Live" : "Connecting"}</span><Select value={String(nodeId ?? "")} onValueChange={value => value && (setFailedNodes(previous => previous.filter(id => id !== Number(value))), setNodePreference(Number(value)))}><SelectTrigger className="h-8 w-28"><SelectValue placeholder="Node">{nodeId === null ? "Node" : `Node ${nodeId}`}</SelectValue></SelectTrigger><SelectContent>{candidates.map(node => <SelectItem key={node.node_id} value={String(node.node_id)}>Node {node.node_id}</SelectItem>)}</SelectContent></Select><span className="text-muted-foreground">height {market?.height ?? "—"}</span></div>
    </div>
    <div className="grid grid-cols-2 gap-3 border-b border-border pb-3 text-sm sm:grid-cols-4 xl:grid-cols-6">
      <div><p className="data-label">Last trade</p><p className={`mt-1 font-mono ${market?.recent_trades[0]?.aggressor_side === "Buy" ? "text-emerald-400" : "text-rose-400"}`}>{formatPrice(market?.recent_trades[0]?.price)}</p></div>
      <div><p className="data-label">Mark</p><p className="mt-1 font-mono">{market?.committed_mark_e8 == null ? "—" : formatAtoms(Math.round(market.committed_mark_e8 / 10 ** (8 - decimals)), decimals)}</p></div>
      <div><p className="data-label">Best bid</p><p className="mt-1 font-mono text-emerald-400">{formatPrice(bestBid)}</p></div>
      <div><p className="data-label">Best ask</p><p className="mt-1 font-mono text-rose-400">{formatPrice(bestAsk)}</p></div>
      <div><p className="data-label">Spread</p><p className="mt-1 font-mono">{bestBid === undefined || bestAsk === undefined ? "—" : formatAtoms(bestAsk - bestBid, decimals)}</p></div>
      <div><p className="data-label">Trades shown</p><p className="mt-1 font-mono">{market?.recent_trades.length ?? 0}</p></div>
    </div>
    <div className="grid gap-3 xl:grid-cols-[minmax(0,1fr)_minmax(22rem,0.9fr)_19rem]">
      <section className="min-w-0 space-y-3 rounded border border-border bg-card p-3"><div className="flex items-center gap-2 text-sm font-medium"><CandlestickChart className="size-4 text-primary" /> Market activity</div><PriceTrace points={chartPoints} decimals={decimals} /><p className="text-xs text-muted-foreground">Recent fills by block. Bars show executed volume.</p></section>
      <MarketActivity book={book} trades={market?.recent_trades ?? []} liquidations={liquidationStream.data}
        isPerp={marketConfig?.market_type.kind === "perp"} priceDecimals={decimals}
        quantityDecimals={quantityDecimals} onTab={setActivityTab} />
      <section className="space-y-3 rounded border border-border bg-card p-3"><div className="flex items-center justify-between"><h3 className="text-sm font-medium">Trade as participant</h3><Activity className="size-4 text-primary" /></div><Select value={String(wallet?.label ?? "")} onValueChange={value => value && onWalletChange(Number(value))}><SelectTrigger><SelectValue placeholder="Select wallet">{wallet ? `Wallet #${wallet.label}${agent ? ` · Agent ${agent.agent_id}` : ""}` : "Select wallet"}</SelectValue></SelectTrigger><SelectContent>{wallets.map(entry => <SelectItem key={entry.label} value={String(entry.label)}>#{entry.label} · {snapshot?.agents.find(agent => agent.account_id === entry.label)?.strategy.kind.replace(/_/g, " ") ?? "manual"}</SelectItem>)}</SelectContent></Select><p className="truncate text-xs text-muted-foreground" title={wallet?.address}>{wallet ? `${wallet.address.slice(0, 14)}…` : "No claimed wallet"}{agent ? ` · Agent ${agent.agent_id} active` : ""}</p><div className="grid grid-cols-2 gap-2"><Button size="sm" variant={side === "Buy" ? "default" : "outline"} onClick={() => setSide("Buy")}>Buy</Button><Button size="sm" variant={side === "Sell" ? "destructive" : "outline"} onClick={() => setSide("Sell")}>Sell</Button></div><Select value={orderType} onValueChange={value => value && setOrderType(value as OrderType)}><SelectTrigger><SelectValue>{orderType === "LO" ? "Limit" : orderType === "PostOnly" ? "Post only" : orderType === "IOC" ? "Marketable IOC" : "Fill or kill"}</SelectValue></SelectTrigger><SelectContent><SelectItem value="LO">Limit</SelectItem><SelectItem value="PostOnly">Post only</SelectItem><SelectItem value="IOC">Marketable IOC</SelectItem><SelectItem value="FOK">Fill or kill</SelectItem></SelectContent></Select><label className="block text-xs text-muted-foreground">Limit price<Input className="mt-1" inputMode="decimal" placeholder={formatPrice(side === "Buy" ? bestAsk : bestBid)} value={price} onChange={event => setPrice(event.target.value)} /></label><label className="block text-xs text-muted-foreground">Quantity<Input className="mt-1" inputMode="decimal" value={quantity} onChange={event => setQuantity(event.target.value)} /></label>{marketConfig?.market_type.kind === "perp" && <label className="flex items-center gap-2 text-xs"><input type="checkbox" checked={reduceOnly} onChange={event => setReduceOnly(event.target.checked)} /> Reduce only</label>}<div className="border-t border-border pt-2 text-xs text-muted-foreground">Nonce {account?.nonce ?? "—"} · {selectedRisk?.mode ?? "—"} margin<br />Equity {selectedRisk ? formatAtoms(selectedRisk.equity_usd_atoms, 6) : "—"} USD</div><Button className="w-full" disabled={!canTrade} onClick={() => void submit()}><Send className="mr-2 size-4" />Submit {side.toLowerCase()}</Button></section>
    </div>
    <Tabs defaultValue="positions" className="rounded border border-border bg-card p-3"><TabsList><TabsTrigger value="positions">Positions</TabsTrigger><TabsTrigger value="orders">Open orders ({ownOrders.length})</TabsTrigger><TabsTrigger value="balances">Balances</TabsTrigger></TabsList>
      <TabsContent value="positions"><div className="max-h-60 overflow-auto"><table className="w-full text-left text-xs"><thead><tr className="border-b border-border text-muted-foreground"><th className="py-2">Market</th><th>Size</th><th>Entry</th><th>Realized PnL</th><th>Margin</th></tr></thead><tbody>{account?.positions.map(position => { const config = catalog.find(entry => entry.id === position.market_id); return <tr key={position.market_id} className="border-b border-border/50"><td className="py-2">{config ? marketName(config) : position.market_id}</td><td>{formatAtoms(position.size_atoms, config?.config.quantity.decimals ?? 0)}</td><td>{formatAtoms(position.entry_price, config?.config.price.decimals ?? 0)}</td><td>{formatAtoms(position.realized_pnl_usd_atoms, 6)}</td><td>{account.risk.find(risk => risk.market_id === position.market_id)?.mode ?? account.risk.find(risk => risk.market_id === null)?.mode ?? "—"}</td></tr> })}</tbody></table>{!account?.positions.length && <p className="py-5 text-sm text-muted-foreground">No open positions for this address.</p>}</div></TabsContent>
      <TabsContent value="orders"><div className="max-h-60 overflow-auto"><table className="w-full text-left text-xs"><thead><tr className="border-b border-border text-muted-foreground"><th className="py-2">ID</th><th>Side</th><th>Price</th><th>Remaining</th><th /></tr></thead><tbody>{ownOrders.map(order => <tr key={order.order_id} className="border-b border-border/50"><td className="py-2">{order.order_id}</td><td>{order.side}</td><td>{formatAtoms(order.price, decimals)}</td><td>{formatAtoms(order.remaining_quantity, quantityDecimals)}</td><td className="text-right"><Button size="sm" variant="ghost" disabled={!canTrade || order.remaining_quantity <= (marketConfig?.quantity.step ?? 1)} onClick={() => setAmend({ id: order.order_id, remaining: formatAtoms(order.remaining_quantity, quantityDecimals) })}>Reduce</Button><Button size="sm" variant="ghost" onClick={() => void cancel(order.order_id)}>Cancel</Button></td></tr>)}</tbody></table>{!ownOrders.length && <p className="py-5 text-sm text-muted-foreground">No open orders in {name}.</p>}</div>{amend && <div className="mt-3 flex flex-wrap items-end gap-2 border-t pt-3"><label className="text-xs text-muted-foreground">Order {amend.id} · new remaining size<Input aria-label="New remaining order size" inputMode="decimal" value={amend.remaining} onChange={event => setAmend({ ...amend, remaining: event.target.value })} /></label><Button size="sm" disabled={!canTrade} onClick={() => void reduceOrder()}>Apply reduction</Button><Button size="sm" variant="ghost" onClick={() => setAmend(null)}>Dismiss</Button><p className="w-full text-xs text-muted-foreground">Keeps FIFO priority. A fill before inclusion can make this request fail.</p></div>}</TabsContent>
      <TabsContent value="balances"><div className="grid gap-2 py-3 sm:grid-cols-3 lg:grid-cols-5">{account?.balances.map(balance => <div key={balance.asset_id} className="rounded border border-border p-3"><p className="data-label">Asset {balance.asset_id}</p><p className="mt-2 font-mono text-sm">{balance.available.toLocaleString()} available</p><p className="text-xs text-muted-foreground">{balance.locked.toLocaleString()} locked · raw atoms</p></div>)}{!account?.balances.length && <p className="text-sm text-muted-foreground">No balance data.</p>}</div></TabsContent>
    </Tabs>
  </div>
}
