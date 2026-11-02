import { useEffect, useState } from "react"
import type { OrderBookSnapshot } from "@/bindings/OrderBookSnapshot"
import type { PerpPositionSnapshot } from "@/bindings/PerpPositionSnapshot"
import type { PerpRiskSnapshot } from "@/bindings/PerpRiskSnapshot"
import type { Trade } from "@/bindings/Trade"
import { formatAtoms } from "@/units"
import { Copy } from "lucide-react"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"

export type LiquidationData = {
  height: number
  market_id: number
  candidates: { risk: PerpRiskSnapshot; position: PerpPositionSnapshot;
    estimated_liquidation_price_atoms?: number; mark_price_atoms?: number; distance_bps?: number }[]
}

function AddressCopy({ address }: { address: string }) {
  return <Button size="xs" variant="ghost" title={`Copy ${address}`} aria-label={`Copy address ${address}`}
    onClick={() => void navigator.clipboard.writeText(address)}><Copy className="size-3" />{address.slice(0, 6)}…{address.slice(-4)}</Button>
}

export function MarketActivity({ book, trades, liquidations, isPerp, priceDecimals, quantityDecimals, onTab }:
  { book?: OrderBookSnapshot; trades: Trade[]; liquidations: LiquidationData | null; isPerp: boolean;
    priceDecimals: number; quantityDecimals: number; onTab: (tab: string) => void }) {
  const [tab, setTab] = useState("book")
  const setActive = (value: string) => { setTab(value); onTab(value) }
  useEffect(() => { if (!isPerp && tab === "liquidations") setActive("book") }, [isPerp, tab])
  const asks = book?.asks ?? []
  const bids = book?.bids ?? []
  const askDepth = asks.reduce<{ price: number; quantity: number; total: number }[]>((rows, level) => {
    rows.push({ ...level, total: (rows[rows.length - 1]?.total ?? 0) + level.quantity })
    return rows
  }, [])
  const bidDepth = bids.reduce<{ price: number; quantity: number; total: number }[]>((rows, level) => {
    rows.push({ ...level, total: (rows[rows.length - 1]?.total ?? 0) + level.quantity })
    return rows
  }, [])
  const depthScale = Math.max(1, askDepth[askDepth.length - 1]?.total ?? 0, bidDepth[bidDepth.length - 1]?.total ?? 0)
  const depthRow = (level: { price: number; quantity: number; total: number }, side: "ask" | "bid") =>
    <div key={`${side}${level.price}`} className="relative grid grid-cols-[1fr_1fr_1fr] items-center py-1 font-mono text-xs tabular-nums">
      <div aria-hidden="true" className={`pointer-events-none absolute inset-y-0 right-0 ${side === "ask" ? "bg-rose-500/15" : "bg-emerald-500/15"}`}
        style={{ width: `${level.total / depthScale * 100}%` }} />
      <span className={`relative z-10 ${side === "ask" ? "text-rose-400" : "text-emerald-400"}`}>{formatAtoms(level.price, priceDecimals)}</span>
      <span className="relative z-10 text-right">{formatAtoms(level.quantity, quantityDecimals)}</span>
      <span className="relative z-10 text-right text-muted-foreground">{formatAtoms(level.total, quantityDecimals)}</span>
    </div>
  return <section className="min-w-0 rounded border border-border bg-card p-3">
    <Tabs value={tab} onValueChange={setActive}>
      <TabsList className="mb-2 h-auto flex-wrap">
        <TabsTrigger value="book">Order book</TabsTrigger>
        <TabsTrigger value="trades">Trades</TabsTrigger>
        {isPerp && <TabsTrigger value="liquidations">Liquidations</TabsTrigger>}
      </TabsList>
      <TabsContent value="book">
        <div className="grid grid-cols-[1fr_1fr_1fr] border-b border-border pb-1 text-[11px] text-muted-foreground"><span>Price</span><span className="text-right">Size</span><span className="text-right">Total</span></div>
        <div className="max-h-48 overflow-auto">{[...askDepth].reverse().map(level => depthRow(level, "ask"))}</div>
        <div className="my-1 flex justify-between border-y border-border py-2 font-mono text-xs tabular-nums"><span className="text-emerald-400">{bids[0] ? formatAtoms(bids[0].price, priceDecimals) : "—"}</span><span className="text-muted-foreground">Spread {bids[0] && asks[0] ? formatAtoms(asks[0].price - bids[0].price, priceDecimals) : "—"}</span><span className="text-rose-400">{asks[0] ? formatAtoms(asks[0].price, priceDecimals) : "—"}</span></div>
        <div className="max-h-48 overflow-auto">{bidDepth.map(level => depthRow(level, "bid"))}</div>
      </TabsContent>
      <TabsContent value="trades" className="max-h-[29rem] overflow-auto">
        <table className="w-full min-w-[42rem] text-left text-xs">
          <thead className="sticky top-0 bg-card text-muted-foreground"><tr><th className="py-2">Block</th><th>Amount</th><th>Execution price</th><th>Direction</th><th>Taker address</th><th>Maker address</th></tr></thead>
          <tbody>{trades.map(trade => <tr key={`${trade.block_sequence}-${trade.transaction_index}-${trade.fill_index}`} className="border-t border-border/50 align-top">
            <td className="py-2">{trade.block_sequence}</td>
            <td className="font-mono">{formatAtoms(trade.quantity, quantityDecimals)}</td>
            <td className={`font-mono ${trade.aggressor_side === "Buy" ? "text-emerald-400" : "text-rose-400"}`}>{formatAtoms(trade.price, priceDecimals)}</td>
            <td className={trade.aggressor_side === "Buy" ? "text-emerald-400" : "text-rose-400"}>{trade.aggressor_side}{trade.liquidation && <Badge className="ml-1 px-1 py-0 text-[9px]" variant="destructive">LIQ</Badge>}</td>
            <td><AddressCopy address={trade.taker_account} /></td>
            <td><AddressCopy address={trade.maker_account} /></td>
          </tr>)}</tbody>
        </table>
        {!trades.length && <p className="py-5 text-sm text-muted-foreground">No committed executions yet.</p>}
      </TabsContent>
      {isPerp && <TabsContent value="liquidations" className="max-h-[29rem] overflow-auto">
        <p className="mb-3 text-xs text-muted-foreground">Nearest accounts within 10% of the committed mark. Trigger prices are estimates with other markets held fixed; liquidation eligibility is rechecked on-chain.</p>
        <div className="mb-3 flex h-24 items-end gap-1 border-b border-border pb-1" aria-label="Distance to liquidation for nearest accounts">
          {(liquidations?.candidates ?? []).slice(0, 20).map(({ risk, distance_bps }) =>
            <div key={risk.account_id} title={`${risk.account_id}: ${distance_bps === undefined ? "eligible" : `${(distance_bps / 100).toFixed(2)}% from trigger`}`}
              className={`min-w-2 flex-1 ${risk.liquidatable ? "bg-rose-400" : "bg-amber-400/80"}`}
              style={{ height: `${Math.max(6, 100 - Math.min(100, (distance_bps ?? 0) / 10))}%` }} />)}
        </div>
        {(liquidations?.candidates ?? []).map(({ risk, position, estimated_liquidation_price_atoms, distance_bps }) => <div key={risk.account_id} className="border-t border-border py-2 text-xs">
          <div className="flex items-center justify-between gap-2"><AddressCopy address={risk.account_id} />{risk.liquidatable && <Badge variant="destructive">Eligible</Badge>}</div>
          <div className="mt-1 grid grid-cols-2 gap-2 font-mono"><span>Size {formatAtoms(position.size_atoms, quantityDecimals)}</span><span>Trigger ≈ {estimated_liquidation_price_atoms === undefined ? "—" : formatAtoms(estimated_liquidation_price_atoms, priceDecimals)}</span><span>Distance {distance_bps === undefined ? "—" : `${(distance_bps / 100).toFixed(2)}%`}</span><span>Equity {formatAtoms(risk.equity_usd_atoms, 6)} USD</span></div>
        </div>)}
        {liquidations && !liquidations.candidates.length && <p className="py-5 text-sm text-muted-foreground">No accounts near liquidation.</p>}
        {!liquidations && <p className="py-5 text-sm text-muted-foreground">Connecting to the liquidation feed…</p>}
      </TabsContent>}
    </Tabs>
  </section>
}
