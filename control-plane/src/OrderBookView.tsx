import { useState } from "react"

import type { ControlSnapshot } from "@/bindings/ControlSnapshot"
import type { BookLevel } from "@/bindings/BookLevel"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"

const BTC_USD = 0

function BookSide({ levels, side, maxQuantity }: {
  levels: BookLevel[]
  side: "bid" | "ask"
  maxQuantity: number
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
          <span className={`relative ${side === "bid" ? "text-emerald-700 dark:text-emerald-400" : "text-rose-700 dark:text-rose-400"}`}>{level.price.toLocaleString()}</span>
          <span className="relative text-right">{level.quantity.toLocaleString()}</span>
          <span className="relative text-right">{level.order_count}</span>
        </div>
      ))}
    </div>
  )
}

export function OrderBookView({ snapshot }: { snapshot: ControlSnapshot | null }) {
  const [visible, setVisible] = useState(false)
  const source = snapshot?.nodes
    .filter((node) => node.state === "ok" && node.status?.order_books.some((book) => book.market_id === BTC_USD))
    .sort((left, right) => (right.status?.chain_height ?? 0) - (left.status?.chain_height ?? 0))[0]
  const book = source?.status?.order_books.find((entry) => entry.market_id === BTC_USD)
  const bestBid = book?.bids[0]?.price
  const bestAsk = book?.asks[0]?.price
  const maxQuantity = Math.max(1, ...(book?.bids ?? []).map((level) => level.quantity), ...(book?.asks ?? []).map((level) => level.quantity))

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between gap-3">
        <div className="space-y-1">
          <CardTitle>BTC-USD order book</CardTitle>
          <p className="text-xs text-muted-foreground">Committed price levels, shown from one responsive node.</p>
        </div>
        <Button variant="outline" aria-expanded={visible} aria-controls="btc-usd-order-book" onClick={() => setVisible(!visible)}>
          {visible ? "Hide BTC-USD order book" : "See BTC-USD order book"}
        </Button>
      </CardHeader>
      {visible && (
        <CardContent id="btc-usd-order-book" className="space-y-4">
          {!book ? <p className="text-sm text-muted-foreground">No responsive node is reporting this market yet.</p> : <>
            <div className="flex flex-wrap items-center gap-2 text-sm">
              <Badge variant="secondary">Node {source?.node_id} · height {source?.status?.chain_height}</Badge>
              <Badge variant={snapshot?.metrics.chain_consistency === "consistent" ? "outline" : "destructive"}>{snapshot?.metrics.chain_consistency ?? "unknown"} replicas</Badge>
              <span className="text-muted-foreground">Spread: {bestBid === undefined || bestAsk === undefined ? "—" : (bestAsk - bestBid).toLocaleString()}</span>
            </div>
            <div className="grid gap-6 md:grid-cols-2">
              <section aria-label="BTC-USD bids"><h3 className="mb-2 text-sm font-semibold">Bids · highest first</h3><BookSide levels={book.bids} side="bid" maxQuantity={maxQuantity} /></section>
              <section aria-label="BTC-USD asks"><h3 className="mb-2 text-sm font-semibold">Asks · lowest first</h3><BookSide levels={book.asks} side="ask" maxQuantity={maxQuantity} /></section>
            </div>
          </>}
        </CardContent>
      )}
    </Card>
  )
}
