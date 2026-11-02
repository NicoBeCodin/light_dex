import { useState } from "react"
import { api } from "@/api"
import { formatAtoms, parseAtoms } from "@/units"
import { marketName } from "@/marketLabels"
import type { ControlSnapshot } from "@/bindings/ControlSnapshot"
import type { MarginMode } from "@/bindings/MarginMode"
import type { Transaction } from "@/bindings/Transaction"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"

type Props = { snapshot: ControlSnapshot | null; command: (operation: () => Promise<void>) => Promise<void> }

export function PerpControls({ snapshot, command }: Props) {
  const [accountLabel, setAccountLabel] = useState(0)
  const [targetLabel, setTargetLabel] = useState(1)
  const [marketId, setMarketId] = useState(1)
  const [amount, setAmount] = useState("1000")
  const [quantity, setQuantity] = useState("0.0001")
  const [mode, setMode] = useState<MarginMode>("Cross")
  const status = snapshot?.nodes.filter(node => node.state === "ok" && node.status?.account_nonces.length)
    .sort((left, right) => (right.status?.chain_height ?? 0) - (left.status?.chain_height ?? 0))[0]?.status
  const markets = status?.markets.filter(market => market.config.market_type.kind === "perp") ?? []
  const market = markets.find(entry => entry.id === marketId)
  const wallet = snapshot?.wallet.find(entry => entry.label === accountLabel)
  const target = snapshot?.wallet.find(entry => entry.label === targetLabel)
  const nonce = status?.account_nonces.find(entry => entry.account_id === wallet?.address)?.nonce
  const send = (build: () => Transaction) => void command(() => api.submitTransaction(build(), { kind: "random" }))
  const account = () => {
    if (!wallet || nonce === undefined) throw new Error("Choose a claimed wallet and wait for a node status")
    return wallet.address
  }
  const collateral = () => parseAtoms(amount, 6, 1)

  return <Card className="dashboard-card"><CardHeader><CardTitle>Perpetual margin</CardTitle><p className="text-xs text-muted-foreground">Wallet USD and perp collateral are separate. Cross mode shares one USD pool across all perp markets; mode changes require flat positions, no open perp orders, and withdrawn collateral.</p></CardHeader><CardContent className="space-y-4">
    <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
      <div className="space-y-1"><Label>Wallet #</Label><Input type="number" value={accountLabel} onChange={event => setAccountLabel(Number(event.target.value))} /></div>
      <div className="space-y-1"><Label>Perp market</Label><Select value={String(marketId)} onValueChange={value => value && setMarketId(Number(value))}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent>{markets.map(entry => <SelectItem key={entry.id} value={String(entry.id)}>{marketName(entry)}</SelectItem>)}</SelectContent></Select></div>
      <div className="space-y-1"><Label>Collateral (USD)</Label><Input inputMode="decimal" value={amount} onChange={event => setAmount(event.target.value)} /></div>
      <div className="space-y-1"><Label>Next confirmed nonce</Label><Input value={nonce ?? "—"} disabled /></div>
    </div>
    <div className="flex flex-wrap gap-2"><Button size="sm" disabled={!snapshot?.running || nonce === undefined} onClick={() => send(() => ({ DepositPerpCollateral: { account_id: account(), market_id: marketId, amount: collateral(), nonce: nonce! } }))}>Deposit collateral</Button><Button size="sm" variant="secondary" disabled={!snapshot?.running || nonce === undefined} onClick={() => send(() => ({ WithdrawPerpCollateral: { account_id: account(), market_id: marketId, amount: collateral(), nonce: nonce! } }))}>Withdraw collateral</Button><Select value={mode} onValueChange={value => value && setMode(value as MarginMode)}><SelectTrigger className="w-36"><SelectValue /></SelectTrigger><SelectContent><SelectItem value="Cross">Cross</SelectItem><SelectItem value="Isolated">Isolated</SelectItem></SelectContent></Select><Button size="sm" variant="outline" disabled={!snapshot?.running || nonce === undefined} onClick={() => send(() => ({ SetMarginMode: { account_id: account(), mode, nonce: nonce! } }))}>Set margin mode</Button></div>
    <div className="grid gap-3 border-t pt-4 sm:grid-cols-3"><div className="space-y-1"><Label>Liquidation target wallet #</Label><Input type="number" value={targetLabel} onChange={event => setTargetLabel(Number(event.target.value))} /></div><div className="space-y-1"><Label>Close quantity ({market ? marketName(market) : "base"})</Label><Input inputMode="decimal" value={quantity} onChange={event => setQuantity(event.target.value)} /></div><div className="flex items-end"><Button size="sm" variant="destructive" disabled={!snapshot?.running || nonce === undefined || !market || !target} onClick={() => send(() => ({ LiquidatePerp: { liquidator: account(), target: target!.address, market_id: marketId, quantity: parseAtoms(quantity, market!.config.quantity.decimals, market!.config.quantity.step), nonce: nonce! } }))}>Liquidate eligible position</Button></div></div>
    <p className="text-xs text-muted-foreground">Liquidation is permissionless, partial (at most half the target position), and executes at the committed oracle mark. The liquidator must have enough margin for the transferred position.</p>
    <div className="flex flex-wrap gap-2 text-xs"><Badge variant="outline">Insurance {formatAtoms(status?.insurance_usd_atoms ?? 0, 6)} USD</Badge><Badge variant={status?.bad_debt_usd_atoms ? "destructive" : "outline"}>Bad debt {formatAtoms(status?.bad_debt_usd_atoms ?? 0, 6)} USD</Badge>{status?.funding.map(entry => <Badge key={entry.market_id} variant="secondary">{markets.find(market => market.id === entry.market_id) ? marketName(markets.find(market => market.id === entry.market_id)!) : `Market ${entry.market_id}`} funding {entry.last_rate_bps} bp · {entry.updates} updates</Badge>)}</div>
    <div className="max-h-44 overflow-auto rounded border border-border"><table className="w-full text-left text-xs"><thead><tr className="border-b"><th className="p-2">Account</th><th className="p-2">Mode / market</th><th className="p-2">Collateral</th><th className="p-2">Equity</th><th className="p-2">Initial</th><th className="p-2">Maintenance</th></tr></thead><tbody>{status?.perp_risk.map(risk => <tr key={`${risk.account_id}-${risk.market_id ?? "cross"}`} className={`border-b ${risk.liquidatable ? "text-destructive" : ""}`}><td className="p-2">#{snapshot?.wallet.find(entry => entry.address === risk.account_id)?.label ?? risk.account_id.slice(0, 8)}</td><td className="p-2">{risk.mode}{risk.market_id === null ? " pool" : ` / ${risk.market_id}`}</td><td className="p-2">{formatAtoms(risk.collateral_usd_atoms, 6)}</td><td className="p-2">{formatAtoms(risk.equity_usd_atoms, 6)}</td><td className="p-2">{formatAtoms(risk.initial_required_usd_atoms, 6)}</td><td className="p-2">{formatAtoms(risk.maintenance_required_usd_atoms, 6)}</td></tr>)}</tbody></table></div>
    {status && status.perp_positions.length > 0 && <div className="max-h-44 overflow-auto rounded border border-border"><table className="w-full text-left text-xs"><thead><tr className="border-b"><th className="p-2">Account</th><th className="p-2">Position market</th><th className="p-2">Signed size</th><th className="p-2">Entry</th><th className="p-2">Realized PnL (USD)</th></tr></thead><tbody>{status.perp_positions.map(position => { const config = markets.find(market => market.id === position.market_id); return <tr key={`${position.account_id}-${position.market_id}`} className="border-b"><td className="p-2">#{snapshot?.wallet.find(entry => entry.address === position.account_id)?.label ?? position.account_id.slice(0, 8)}</td><td className="p-2">{config ? marketName(config) : position.market_id}</td><td className="p-2">{formatAtoms(position.size_atoms, config?.config.quantity.decimals ?? 0)}</td><td className="p-2">{formatAtoms(position.entry_price, config?.config.price.decimals ?? 0)}</td><td className="p-2">{formatAtoms(position.realized_pnl_usd_atoms, 6)}</td></tr> })}</tbody></table></div>}
  </CardContent></Card>
}
