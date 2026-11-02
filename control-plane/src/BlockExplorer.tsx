import { useCallback, useEffect, useMemo, useState } from "react"
import { ArrowUpRight, Blocks, Copy, Search, ShieldCheck } from "lucide-react"
import { nodeApi } from "@/api"
import type { ControlSnapshot } from "@/bindings/ControlSnapshot"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"

type Block = {
  sequence: number; digest: string; previous_hash: string; state_root: string
  transaction_count: number; oracle_update_count: number; algorithm: string | null
  view: number | null; leader: number | null; certificate_votes: number
}
type Tx = {
  digest: string; block_sequence: number; transaction_index: number; kind: string
  account: string; nonce: number; success: boolean | null; error: string | null
}
type Recent = {
  node_id: number; height: number; view: number; primary: number; chain_id: string | null
  blocks: Block[]; transactions: Tx[]
}
type BlockDetail = { block: Block; transactions: Tx[] }
type TxDetail = { block: Block; transaction: Tx; payload: unknown; signature: string | null }
type Detail = { kind: "block"; data: BlockDetail } | { kind: "transaction"; data: TxDetail }

const short = (value: string, size = 9) => value.length > size * 2 ? `${value.slice(0, size)}…${value.slice(-size)}` : value

function Hash({ value }: { value: string }) {
  return <span className="inline-flex items-center gap-1.5 font-mono text-xs" title={value}>{short(value)}
    <button type="button" aria-label={`Copy ${value}`} className="text-muted-foreground hover:text-primary" onClick={() => void navigator.clipboard.writeText(value)}><Copy className="size-3" /></button>
  </span>
}

function Outcome({ value }: { value: boolean | null }) {
  if (value === null) return <Badge variant="outline">Unavailable</Badge>
  return <Badge variant={value ? "default" : "destructive"}>{value ? "Executed" : "Failed"}</Badge>
}

export function BlockExplorer({ snapshot }: { snapshot: ControlSnapshot | null }) {
  const candidateKey = JSON.stringify((snapshot?.nodes ?? [])
    .filter(node => node.state !== "dead" && node.read_api_address)
    .map(node => ({ id: node.node_id, address: node.read_api_address! })))
  const candidates = useMemo(() => JSON.parse(candidateKey) as { id: number; address: string }[], [candidateKey])
  const [source, setSource] = useState<number | null>(null)
  const [recent, setRecent] = useState<Recent | null>(null)
  const [detail, setDetail] = useState<Detail | null>(null)
  const [search, setSearch] = useState("")
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  const query = useCallback(async <T,>(scope: "explorer" | "blocks" | "transactions" | "hashes", key: string | number): Promise<T> => {
    if (candidates.length === 0) throw new Error("No validator read API is available yet")
    const start = Math.max(0, candidates.findIndex(node => node.id === source))
    let lastError: unknown
    for (let offset = 0; offset < candidates.length; offset++) {
      const node = candidates[(start + offset) % candidates.length]
      try {
        const result = await nodeApi.get<T>(node.id, scope, key)
        setSource(node.id)
        return result
      } catch (cause) { lastError = cause }
    }
    throw lastError ?? new Error("No validator answered")
  }, [candidates, source])

  useEffect(() => {
    if (candidates.length === 0) { setRecent(null); setSource(null); return }
    let cancelled = false
    let timer: number | undefined
    const refresh = async () => {
      try {
        const value = await query<Recent>("explorer", "recent")
        if (!cancelled) { setRecent(value); setError(null) }
      } catch (cause) {
        if (!cancelled) setError(String(cause))
      } finally {
        if (!cancelled) timer = window.setTimeout(refresh, 2000)
      }
    }
    void refresh()
    return () => { cancelled = true; window.clearTimeout(timer) }
  }, [candidates.length, query])

  async function openBlock(sequence: number) {
    setLoading(true); setError(null)
    try { setDetail({ kind: "block", data: await query<BlockDetail>("blocks", sequence) }) }
    catch (cause) { setError(String(cause)) }
    finally { setLoading(false) }
  }
  async function openTransaction(digest: string) {
    setLoading(true); setError(null)
    try { setDetail({ kind: "transaction", data: await query<TxDetail>("transactions", digest) }) }
    catch (cause) { setError(String(cause)) }
    finally { setLoading(false) }
  }
  async function lookup() {
    const value = search.trim().replace(/^#/, "")
    if (/^\d+$/.test(value)) { await openBlock(Number(value)); return }
    if (!/^[0-9a-fA-F]{64}$/.test(value)) {
      setError("Enter a block sequence or a 64-character block or transaction digest")
      return
    }
    setLoading(true); setError(null)
    try {
      const result = await query<{ kind: "block"; detail: BlockDetail } | { kind: "transaction"; detail: TxDetail }>("hashes", value.toLowerCase())
      setDetail(result.kind === "block"
        ? { kind: "block", data: result.detail }
        : { kind: "transaction", data: result.detail })
    } catch (cause) { setError(String(cause)) }
    finally { setLoading(false) }
  }

  const current = candidates.find(node => node.id === source)
  return <div className="space-y-5">
    <div className="flex flex-wrap items-end justify-between gap-4">
      <div><p className="data-label text-primary">Chain</p><h2 className="mt-1 text-3xl font-semibold tracking-tight">Block explorer</h2>
        <p className="mt-1 text-sm text-muted-foreground">Committed blocks and transactions from a live validator.</p></div>
      <div className="rounded-lg border border-border bg-card px-4 py-2 text-xs">
        <div className="flex items-center gap-2"><span className={`size-2 rounded-full ${recent ? "bg-emerald-500" : "bg-muted-foreground"}`} />Source: {current ? `node ${current.id}` : "waiting for a node"}</div>
        <div className="mt-1 font-mono text-muted-foreground">{current ? `http://${current.address}` : "No read API connected"}</div>
      </div>
    </div>

    <div className="grid gap-3 sm:grid-cols-3">
      <div className="dashboard-card rounded-xl border border-border p-4"><p className="data-label">Height</p><p className="mt-2 text-2xl font-semibold tabular-nums">{recent?.height.toLocaleString() ?? "—"}</p></div>
      <div className="dashboard-card rounded-xl border border-border p-4"><p className="data-label">Current view</p><p className="mt-2 text-2xl font-semibold tabular-nums">{recent?.view.toLocaleString() ?? "—"}</p></div>
      <div className="dashboard-card rounded-xl border border-border p-4"><p className="data-label">Current leader</p><p className="mt-2 text-2xl font-semibold tabular-nums">{recent ? `Node ${recent.primary}` : "—"}</p></div>
    </div>

    <div className="flex gap-2"><div className="relative max-w-xl flex-1"><Search className="absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" /><Input aria-label="Search block or transaction" placeholder="Block sequence, block hash or transaction digest" value={search} onChange={event => { setSearch(event.target.value) }} onKeyDown={event => { if (event.key === "Enter") void lookup() }} className="pl-9" /></div><Button onClick={() => void lookup()} disabled={loading}>Search</Button></div>
    {error && <p role="alert" className="rounded-lg border border-destructive/40 bg-destructive/10 px-4 py-3 text-sm text-destructive">{error}</p>}

    {detail && <section className="rounded-xl border border-primary/30 bg-card p-5">
      <div className="mb-4 flex items-center justify-between"><h3 className="text-lg font-semibold">{detail.kind === "block" ? `Block ${detail.data.block.sequence}` : "Transaction"}</h3><Button variant="ghost" size="sm" onClick={() => setDetail(null)}>Close</Button></div>
      {detail.kind === "block" ? <>
        <BlockFacts block={detail.data.block} />
        <h4 className="mt-5 mb-2 text-sm font-medium">Transactions ({detail.data.transactions.length})</h4>
        <TransactionRows transactions={detail.data.transactions} onOpen={openTransaction} />
      </> : <>
        <div className="grid gap-3 text-sm sm:grid-cols-2"><Fact label="Digest"><Hash value={detail.data.transaction.digest} /></Fact><Fact label="Status"><Outcome value={detail.data.transaction.success} /></Fact><Fact label="Type">{detail.data.transaction.kind}</Fact><Fact label="Account"><Hash value={detail.data.transaction.account} /></Fact><Fact label="Nonce">{detail.data.transaction.nonce}</Fact><Fact label="Included in"><button type="button" className="text-primary hover:underline" onClick={() => void openBlock(detail.data.block.sequence)}>Block {detail.data.block.sequence}</button></Fact></div>
        {detail.data.transaction.error && <p className="mt-4 text-sm text-destructive">{detail.data.transaction.error}</p>}
        <h4 className="mt-5 mb-2 text-sm font-medium">Payload</h4><pre className="overflow-x-auto rounded-lg border border-border bg-background p-4 text-xs">{JSON.stringify(detail.data.payload, null, 2)}</pre>
        {detail.data.signature && <p className="mt-3 text-xs text-muted-foreground">Signature <Hash value={detail.data.signature} /></p>}
      </>}
    </section>}

    <div className="grid gap-5 xl:grid-cols-2">
      <section className="min-w-0 rounded-xl border border-border bg-card"><div className="flex items-center gap-2 border-b border-border px-5 py-4"><Blocks className="size-4 text-primary" /><h3 className="font-semibold">Recent blocks</h3><span className="ml-auto text-xs text-muted-foreground">Latest 20</span></div>
        <div className="max-h-[620px] overflow-auto divide-y divide-border">{(recent?.blocks ?? []).map(block => <button type="button" key={block.sequence} onClick={() => void openBlock(block.sequence)} className="flex w-full items-center gap-4 px-5 py-3 text-left hover:bg-muted/50"><span className="w-16 shrink-0 font-mono text-sm text-primary">#{block.sequence}</span><span className="min-w-0 flex-1"><span className="block font-mono text-xs text-muted-foreground" title={block.digest}>{short(block.digest)}</span><span className="mt-1 block text-xs text-muted-foreground">{block.algorithm ?? "Genesis"} · {block.leader === null ? "No leader" : `Leader ${block.leader}`} · {block.certificate_votes} votes</span></span><span className="text-xs tabular-nums">{block.transaction_count} tx</span><ArrowUpRight className="size-4 text-muted-foreground" /></button>)}
          {recent && recent.blocks.length === 0 && <p className="p-5 text-sm text-muted-foreground">Waiting for the first block.</p>}</div></section>
      <section className="min-w-0 rounded-xl border border-border bg-card"><div className="flex items-center gap-2 border-b border-border px-5 py-4"><ShieldCheck className="size-4 text-primary" /><h3 className="font-semibold">Recent transactions</h3><span className="ml-auto text-xs text-muted-foreground">Latest 40</span></div><div className="max-h-[620px] overflow-auto"><TransactionRows transactions={recent?.transactions ?? []} onOpen={openTransaction} />
        {recent && recent.transactions.length === 0 && <p className="p-5 text-sm text-muted-foreground">No transactions committed yet.</p>}</div></section>
    </div>
    <p className="text-xs text-muted-foreground">Older transaction outcomes may be unavailable after the node’s recent receipt window expires. Block contents remain searchable for this session.</p>
  </div>
}

function Fact({ label, children }: { label: string; children: React.ReactNode }) {
  return <div><p className="data-label">{label}</p><div className="mt-1 break-all">{children}</div></div>
}
function BlockFacts({ block }: { block: Block }) {
  return <div className="grid gap-3 text-sm sm:grid-cols-2 lg:grid-cols-3"><Fact label="Block hash"><Hash value={block.digest} /></Fact><Fact label="Previous hash"><Hash value={block.previous_hash} /></Fact><Fact label="State root"><Hash value={block.state_root} /></Fact><Fact label="Consensus">{block.algorithm ?? "Genesis"}</Fact><Fact label="View / leader">{block.view === null ? "—" : `${block.view} / node ${block.leader}`}</Fact><Fact label="Certificate">{block.certificate_votes} votes</Fact><Fact label="Oracle updates">{block.oracle_update_count}</Fact></div>
}
function TransactionRows({ transactions, onOpen }: { transactions: Tx[]; onOpen: (digest: string) => void }) {
  return <div className="divide-y divide-border">{transactions.map(tx => <button type="button" key={tx.digest} onClick={() => onOpen(tx.digest)} className="flex w-full items-center gap-3 px-5 py-3 text-left hover:bg-muted/50"><span className="min-w-0 flex-1"><span className="block truncate text-sm">{tx.kind} <span className="ml-1 font-mono text-xs text-muted-foreground">{short(tx.digest, 7)}</span></span><span className="mt-1 block text-xs text-muted-foreground">Block {tx.block_sequence} · nonce {tx.nonce}</span></span><Outcome value={tx.success} /><ArrowUpRight className="size-4 text-muted-foreground" /></button>)}</div>
}
