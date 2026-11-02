import { useMemo, useState } from "react"
import { Send, Wallet } from "lucide-react"
import { api } from "@/api"
import { assetName } from "@/marketLabels"
import { formatAtoms, parseAtoms } from "@/units"
import type { ControlSnapshot } from "@/bindings/ControlSnapshot"
import type { OrderType } from "@/bindings/OrderType"
import type { Side } from "@/bindings/Side"
import type { TargetNode } from "@/bindings/TargetNode"
import type { Transaction } from "@/bindings/Transaction"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { ScrollArea } from "@/components/ui/scroll-area"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"

type TxTab = "transfer" | "place" | "cancel"
function NumberField({ label, value, onChange }: { label: string; value: number; onChange: (value: number) => void }) {
  return <div className="space-y-1.5"><Label>{label}</Label><Input type="number" value={value} onChange={event => onChange(Number(event.target.value))} /></div>
}
function targetFromString(value: string): TargetNode {
  return value === "random" || value === "primary" ? { kind: value } : { kind: "node", node_id: Number(value.slice(5)) }
}
function TargetSelect({ value, onChange, snapshot }: { value: string; onChange: (value: string) => void; snapshot: ControlSnapshot | null }) {
  return <Select value={value} onValueChange={next => next && onChange(next)}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="random">Random node</SelectItem><SelectItem value="primary">Current primary</SelectItem>{snapshot?.nodes.filter(node => node.state !== "dead").map(node => <SelectItem key={node.node_id} value={`node:${node.node_id}`}>Node {node.node_id}</SelectItem>)}</SelectContent></Select>
}

export function ParticipantWallets({ snapshot, command }: { snapshot: ControlSnapshot | null; command: (operation: () => Promise<void>) => Promise<void> }) {
  const [txTab, setTxTab] = useState<TxTab>("transfer")
  const [txTarget, setTxTarget] = useState("random")
  const [from, setFrom] = useState(0), [toAddress, setToAddress] = useState(""), [asset, setAsset] = useState(0), [amount, setAmount] = useState(100), [transferNonce, setTransferNonce] = useState(1)
  const [accountId, setAccountId] = useState(0), [marketId, setMarketId] = useState(0), [side, setSide] = useState<Side>("Buy"), [orderType, setOrderType] = useState<OrderType>("LO"), [price, setPrice] = useState("100000.0"), [quantity, setQuantity] = useState("0.0001"), [orderNonce, setOrderNonce] = useState(1)
  const [reduceOnly, setReduceOnly] = useState(false)
  const [cancelAccount, setCancelAccount] = useState(0), [cancelMarket, setCancelMarket] = useState(0), [orderId, setOrderId] = useState(0), [cancelNonce, setCancelNonce] = useState(1)
  const [lastCreated, setLastCreated] = useState<string | null>(null)
  const [walletFilter, setWalletFilter] = useState("")
  const bestStatus = snapshot?.nodes.filter(node => node.state === "ok" && node.status?.account_nonces.length).sort((a, b) => (b.status?.chain_height ?? 0) - (a.status?.chain_height ?? 0))[0]?.status
  const selectedMarket = bestStatus?.markets.find(market => market.id === marketId)?.config
  const balances = useMemo(() => new Map(bestStatus?.agent_balances.map(balance => [balance.account_id, balance]) ?? []), [bestStatus])
  const wallets = snapshot?.wallet.filter(wallet => `#${wallet.label} ${wallet.address}`.toLowerCase().includes(walletFilter.toLowerCase())) ?? []
  const addressOf = (label: number) => {
    const address = snapshot?.wallet.find(wallet => wallet.label === label)?.address
    if (!address) throw new Error(`Wallet #${label} does not exist`)
    return address
  }
  const buildTransaction = (): Transaction => {
    switch (txTab) {
      case "transfer": return { Transfer: { from: addressOf(from), to: toAddress.trim(), asset, amount, nonce: transferNonce } }
      case "place": {
        if (!selectedMarket) throw new Error("Wait for market configuration from a node")
        const order = { account_id: addressOf(accountId), market_id: marketId, order_type: orderType, side,
          price: parseAtoms(price, selectedMarket.price.decimals, selectedMarket.price.step),
          quantity: parseAtoms(quantity, selectedMarket.quantity.decimals, selectedMarket.quantity.step), nonce: orderNonce }
        return selectedMarket.market_type.kind === "perp"
          ? { PlacePerpOrder: { ...order, reduce_only: reduceOnly } }
          : { PlaceOrder: order }
      }
      case "cancel": return { CancelOrder: { account_id: addressOf(cancelAccount), market_id: cancelMarket, order_id: orderId, nonce: cancelNonce } }
    }
  }
  return <>
    <div className="grid gap-5 xl:grid-cols-[1fr_1.15fr]"><Card className="dashboard-card"><CardHeader><CardTitle>Wallet addresses</CardTitle></CardHeader><CardContent className="space-y-4"><p className="text-xs text-muted-foreground">New identities are unfunded until a signed claim commits. Private keys stay in this controller session.</p><div className="flex flex-wrap gap-2"><Button size="sm" variant="secondary" disabled={!snapshot?.running} onClick={() => void command(async () => { const wallet = await api.createAddress(); setLastCreated(wallet.address); setToAddress(wallet.address) })}><Wallet />Create address</Button><Button size="sm" disabled={!snapshot?.running || snapshot.claim_progress?.active} onClick={() => void command(async () => { const [wallet] = await api.claimNewAccounts(1); setLastCreated(wallet.address); setToAddress(wallet.address) })}>Claim new address</Button></div>{lastCreated && <p className="break-all text-xs text-primary">Latest: {lastCreated}</p>}<Input aria-label="Search wallet" placeholder="Search wallet number or address" value={walletFilter} onChange={event => setWalletFilter(event.target.value)} /><ScrollArea className="h-64 rounded-lg border border-border bg-background/50"><div className="divide-y divide-border/70">{wallets.map(wallet => { const balance = balances.get(wallet.address); return <div key={wallet.label} className="flex items-center gap-3 px-3 py-2 text-xs"><span className="w-10 shrink-0 font-semibold text-primary">#{wallet.label}</span><code className="min-w-0 flex-1 truncate" title={wallet.address}>{wallet.address}</code><span className="shrink-0 text-muted-foreground">{balance ? `${formatAtoms(balance.usd_available, 6)} USD · ${formatAtoms(balance.btc_available, 4)} BTC` : "Unclaimed"}</span><Button size="xs" variant="ghost" onClick={() => setToAddress(wallet.address)}>Use</Button></div> })}{wallets.length === 0 && <p className="p-4 text-sm text-muted-foreground">No wallets yet.</p>}</div></ScrollArea></CardContent></Card>
    <Card className="dashboard-card"><CardHeader><CardTitle>Manual transaction</CardTitle></CardHeader><CardContent className="space-y-4"><Tabs value={txTab} onValueChange={value => setTxTab(value as TxTab)}><TabsList><TabsTrigger value="transfer">Transfer</TabsTrigger><TabsTrigger value="place">Place order</TabsTrigger><TabsTrigger value="cancel">Cancel order</TabsTrigger></TabsList><TabsContent value="transfer" className="pt-4"><div className="grid gap-3 sm:grid-cols-2"><NumberField label="From wallet #" value={from} onChange={setFrom} /><div className="space-y-1.5"><Label>To address</Label><Input value={toAddress} onChange={event => setToAddress(event.target.value.trim())} placeholder="64-character address" /></div><NumberField label="Asset" value={asset} onChange={setAsset} /><NumberField label="Amount (atoms)" value={amount} onChange={setAmount} /><NumberField label="Nonce" value={transferNonce} onChange={setTransferNonce} /></div></TabsContent><TabsContent value="place" className="pt-4"><div className="grid gap-3 sm:grid-cols-2"><NumberField label="Wallet #" value={accountId} onChange={setAccountId} /><NumberField label="Market" value={marketId} onChange={setMarketId} /><div className="space-y-1.5"><Label>Side</Label><Select value={side} onValueChange={value => setSide(value as Side)}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="Buy">Buy</SelectItem><SelectItem value="Sell">Sell</SelectItem></SelectContent></Select></div><div className="space-y-1.5"><Label>Order type</Label><Select value={orderType} onValueChange={value => setOrderType(value as OrderType)}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="LO">Limit</SelectItem><SelectItem value="PostOnly">Post only</SelectItem><SelectItem value="IOC">IOC</SelectItem><SelectItem value="FOK">FOK</SelectItem></SelectContent></Select></div><div className="space-y-1.5"><Label>Price (USD)</Label><Input type="text" inputMode="decimal" value={price} onChange={event => setPrice(event.target.value)} /></div><div className="space-y-1.5"><Label>Quantity (BTC)</Label><Input type="text" inputMode="decimal" value={quantity} onChange={event => setQuantity(event.target.value)} /></div><NumberField label="Nonce" value={orderNonce} onChange={setOrderNonce} /></div>{selectedMarket?.market_type.kind === "perp" && <label className="mt-3 flex items-center gap-2 text-xs"><input type="checkbox" checked={reduceOnly} onChange={event => setReduceOnly(event.target.checked)} />Reduce only (IOC/FOK)</label>}<p className="mt-2 text-xs text-muted-foreground">{selectedMarket ? `Tick $${formatAtoms(selectedMarket.price.step, selectedMarket.price.decimals)}; minimum lot ${formatAtoms(selectedMarket.quantity.step, selectedMarket.quantity.decimals)} ${assetName(selectedMarket.base_asset)}.` : "Waiting for market configuration."}</p></TabsContent><TabsContent value="cancel" className="pt-4"><div className="grid gap-3 sm:grid-cols-2"><NumberField label="Wallet #" value={cancelAccount} onChange={setCancelAccount} /><NumberField label="Market" value={cancelMarket} onChange={setCancelMarket} /><NumberField label="Order ID" value={orderId} onChange={setOrderId} /><NumberField label="Nonce" value={cancelNonce} onChange={setCancelNonce} /></div></TabsContent></Tabs><div className="flex items-end gap-3 border-t pt-4"><div className="w-44 space-y-1.5"><Label>Submit to</Label><TargetSelect value={txTarget} onChange={setTxTarget} snapshot={snapshot} /></div><Button disabled={!snapshot?.running} onClick={() => void command(() => api.submitTransaction(buildTransaction(), targetFromString(txTarget)))}><Send />Submit</Button></div></CardContent></Card></div>
  </>
}
