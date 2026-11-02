import { useState } from "react"
import { marketName } from "@/marketLabels"

import type { ControlSnapshot } from "@/bindings/ControlSnapshot"
import type { BookLevel } from "@/bindings/BookLevel"
import type { Trade } from "@/bindings/Trade"
import { Badge } from "@/components/ui/badge"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"

function formatAtoms(value: number, decimals: number): string {
  if (!Number.isSafeInteger(value) || decimals < 0 || decimals > 18) return String(value)
  const digits = BigInt(Math.abs(value)).toString().padStart(decimals + 1, "0")
  const whole = decimals === 0 ? digits : digits.slice(0, -decimals)
  const fraction = decimals === 0 ? "" : `.${digits.slice(-decimals)}`
  return `${value < 0 ? "-" : ""}${whole.replace(/\B(?=(\d{3})+(?!\d))/g, ",")}${fraction}`
}

function BookSide({ levels, side, maxQuantity, priceDecimals, quantityDecimals }: {
  levels: BookLevel[]
  side: "bid" | "ask"
  maxQuantity: number
  priceDecimals: number
  quantityDecimals: number
}) {
  return (
    <div className="min-w-0 space-y-1">
      <div className="grid grid-cols-3 gap-2 border-b pb-2 text-xs text-muted-foreground">
        <span>Price</span><span className="text-right">Size</span><span className="text-right">Orders</span>
      </div>
      {levels.length === 0 && <p className="py-4 text-sm text-muted-foreground">No {side}s</p>}
      {levels.map((level) => (
        <div key={level.price} className="relative grid grid-cols-3 gap-2 rounded px-1 py-1 font-mono text-sm tabular-nums">
          <div
            aria-hidden="true"
            className={`absolute inset-y-0 right-0 rounded ${side === "bid" ? "bg-emerald-500/10" : "bg-rose-500/10"}`}
            style={{ width: `${Math.max(0, Math.min(100, level.quantity / maxQuantity * 100))}%` }}
          />
          <span className={`relative ${side === "bid" ? "text-emerald-700 dark:text-emerald-400" : "text-rose-700 dark:text-rose-400"}`}>{formatAtoms(level.price, priceDecimals)}</span>
          <span className="relative text-right">{formatAtoms(level.quantity, quantityDecimals)}</span>
          <span className="relative text-right">{level.order_count}</span>
        </div>
      ))}
    </div>
  )
}

export function OrderBookView({ snapshot }: { snapshot: ControlSnapshot | null }) {
  const [marketId, setMarketId] = useState(0)
  const source = snapshot?.nodes
    .filter((node) => node.state === "ok" && node.status?.order_books.some((book) => book.market_id === marketId))
    .sort((left, right) => (right.status?.chain_height ?? 0) - (left.status?.chain_height ?? 0))[0]
  const book = source?.status?.order_books.find((entry) => entry.market_id === marketId)
  const market = source?.status?.markets.find((entry) => entry.id === marketId)
  const priceDecimals = market?.config.price.decimals ?? 0
  const quantityDecimals = market?.config.quantity.decimals ?? 0
  const trades: Trade[] = source?.status?.recent_trades.filter((trade) => trade.market_id === marketId).slice(0, 100) ?? []
  const markets = [...new Set((source?.status?.order_books ?? []).map((entry) => entry.market_id))]
  const bestBid = book?.bids[0]?.price
  const bestAsk = book?.asks[0]?.price
  const maxQuantity = Math.max(1, ...(book?.bids ?? []).map((level) => level.quantity), ...(book?.asks ?? []).map((level) => level.quantity))
  const walletLabels = new Map(snapshot?.wallet.map(entry => [entry.address, entry.label]))
  const accountLabel = (address: string) => walletLabels.has(address) ? `#${walletLabels.get(address)}` : `${address.slice(0, 10)}…`

  return (
    <Card className="dashboard-card border-border">
      <CardHeader className="flex flex-wrap items-center justify-between gap-3 border-b border-border/70">
        <div className="space-y-1">
          <p className="data-label text-primary">Committed market data</p>
          <CardTitle className="text-2xl">{market ? marketName(market) : `Market ${marketId}`}</CardTitle>
          <p className="text-xs text-muted-foreground">Order book and recent executions from one responsive validator.</p>
        </div>
        <div className="w-44"><Select value={String(marketId)} onValueChange={(value) => value && setMarketId(Number(value))}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent>{(markets.length ? markets : [0]).map((id) => <SelectItem key={id} value={String(id)}>{source?.status?.markets.find(entry => entry.id === id) ? marketName(source.status.markets.find(entry => entry.id === id)!) : `Market ${id}`}</SelectItem>)}</SelectContent></Select></div>
      </CardHeader>
        <CardContent className="space-y-5 pt-5">
          {!book ? <p className="text-sm text-muted-foreground">No responsive node is reporting this market yet.</p> : <>
            <div className="flex flex-wrap items-center gap-2 text-sm">
              <Badge variant="secondary">Node {source?.node_id} · height {source?.status?.chain_height}</Badge>
              <Badge variant={snapshot?.metrics.chain_consistency === "consistent" ? "outline" : "destructive"}>{snapshot?.metrics.chain_consistency ?? "unknown"} replicas</Badge>
              <span className="text-muted-foreground">Spread: {bestBid === undefined || bestAsk === undefined ? "—" : formatAtoms(bestAsk - bestBid, priceDecimals)}</span>
            </div>
            <div className="grid gap-3 sm:grid-cols-3"><div className="rounded-lg border border-border bg-background/60 p-4"><p className="data-label">Best bid</p><p className="mt-2 font-mono text-xl text-emerald-400">{bestBid === undefined ? "—" : formatAtoms(bestBid, priceDecimals)}</p></div><div className="rounded-lg border border-border bg-background/60 p-4"><p className="data-label">Spread</p><p className="mt-2 font-mono text-xl">{bestBid === undefined || bestAsk === undefined ? "—" : formatAtoms(bestAsk - bestBid, priceDecimals)}</p></div><div className="rounded-lg border border-border bg-background/60 p-4"><p className="data-label">Best ask</p><p className="mt-2 font-mono text-xl text-rose-400">{bestAsk === undefined ? "—" : formatAtoms(bestAsk, priceDecimals)}</p></div></div>
            <div className="grid gap-4 md:grid-cols-2">
              <section aria-label="Market bids" className="min-w-0 rounded-lg border border-border bg-background/50 p-4"><h3 className="mb-3 text-sm font-semibold">Bids <span className="font-normal text-muted-foreground">· highest first</span></h3><BookSide levels={book.bids} side="bid" maxQuantity={maxQuantity} priceDecimals={priceDecimals} quantityDecimals={quantityDecimals} /></section>
              <section aria-label="Market asks" className="min-w-0 rounded-lg border border-border bg-background/50 p-4"><h3 className="mb-3 text-sm font-semibold">Asks <span className="font-normal text-muted-foreground">· lowest first</span></h3><BookSide levels={book.asks} side="ask" maxQuantity={maxQuantity} priceDecimals={priceDecimals} quantityDecimals={quantityDecimals} /></section>
            </div>
            <section className="space-y-2 rounded-lg border border-border bg-background/50 p-4"><h3 className="text-sm font-semibold">Recent trades</h3><p className="text-xs text-muted-foreground">Newest first. Coordinates identify a fill in the committed chain, not wall-clock time.</p><div className="max-h-72 overflow-auto"><Table><TableHeader><TableRow><TableHead>Block · Tx · Fill</TableHead><TableHead>Price</TableHead><TableHead>Size</TableHead><TableHead>Aggressor</TableHead><TableHead>Maker</TableHead><TableHead>Taker</TableHead></TableRow></TableHeader><TableBody>{trades.map((trade) => <TableRow key={`${trade.block_sequence}-${trade.transaction_index}-${trade.fill_index}`}><TableCell>{trade.block_sequence} · {trade.transaction_index} · {trade.fill_index}</TableCell><TableCell>{formatAtoms(trade.price, priceDecimals)}</TableCell><TableCell>{formatAtoms(trade.quantity, quantityDecimals)}</TableCell><TableCell>{trade.aggressor_side}</TableCell><TableCell title={trade.maker_account}>{accountLabel(trade.maker_account)}</TableCell><TableCell title={trade.taker_account}>{accountLabel(trade.taker_account)}</TableCell></TableRow>)}</TableBody></Table></div>{trades.length === 0 && <p className="text-sm text-muted-foreground">No committed trades yet.</p>}</section>
          </>}
        </CardContent>
    </Card>
  )
}
