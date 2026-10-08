import { useEffect, useMemo, useRef, useState } from "react"
import { Activity, ArrowUpRight, Pause, RefreshCw, Skull, Wifi, WifiOff } from "lucide-react"
import type { ControlSnapshot } from "@/bindings/ControlSnapshot"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"

type Props = {
  snapshot: ControlSnapshot | null
  onQuery: (nodeId: number) => Promise<void>
  onPause: (nodeId: number, millis: number) => Promise<void>
  onKill: (nodeId: number) => Promise<void>
  onOpenMarket: () => void
  onOpenParticipants: () => void
}

function ageLabel(timestamp: number | null, now: number) {
  if (timestamp === null) return "Never queried"
  const seconds = Math.max(0, Math.floor((now - timestamp) / 1000))
  if (seconds < 2) return "Just updated"
  if (seconds < 60) return `${seconds}s ago`
  return `${Math.floor(seconds / 60)}m ${seconds % 60}s ago`
}

function healthColor(state: string) {
  if (state === "ok") return "bg-emerald-400"
  if (state === "dead") return "bg-rose-400"
  if (state === "paused") return "bg-amber-400"
  return "bg-slate-400"
}

function Detail({ label, value }: { label: string; value: string | number | undefined | null }) {
  return <div className="min-w-0 border-b border-border/60 py-2.5 last:border-0"><dt className="data-label">{label}</dt><dd className="data-value mt-1 truncate text-sm text-foreground" title={String(value ?? "—")}>{value ?? "—"}</dd></div>
}

export function NodeOverview({ snapshot, onQuery, onPause, onKill, onOpenMarket, onOpenParticipants }: Props) {
  const nodes = snapshot?.nodes ?? []
  const [selectedId, setSelectedId] = useState<number | null>(null)
  const [busyId, setBusyId] = useState<number | null>(null)
  const [pauseMs, setPauseMs] = useState(5000)
  const [actionError, setActionError] = useState<string | null>(null)
  const [now, setNow] = useState(Date.now())
  const confirmRef = useRef<HTMLDialogElement>(null)
  const selected = useMemo(() => nodes.find(node => node.node_id === selectedId) ?? nodes[0], [nodes, selectedId])

  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000)
    return () => window.clearInterval(timer)
  }, [])

  async function run(nodeId: number, operation: () => Promise<void>) {
    setActionError(null)
    setBusyId(nodeId)
    try { await operation() } catch (error) { setActionError(String(error)) }
    finally { setBusyId(null) }
  }

  const active = selected && selected.state !== "dead" && snapshot?.running
  const status = selected?.status
  const stale = selected?.last_status_at_ms === null || (selected?.last_status_at_ms !== undefined && now - selected.last_status_at_ms > 10_000)

  return <div className="space-y-5">
    <div className="flex flex-wrap items-end justify-between gap-4">
      <div><p className="data-label text-primary">Nodes</p><h2 className="mt-1 text-2xl font-semibold tracking-tight">Validators</h2><p className="mt-1 text-sm text-muted-foreground">Select a node for status and controls.</p></div>
      <div className="flex flex-wrap gap-2"><Badge variant="outline">{snapshot?.metrics.responsive_nodes ?? 0}/{nodes.length} responsive</Badge><Badge variant="outline">Height {snapshot?.metrics.height_min ?? "—"}–{snapshot?.metrics.height_max ?? "—"}</Badge><Badge variant={snapshot?.metrics.chain_consistency === "split" ? "destructive" : "secondary"}>{snapshot?.metrics.chain_consistency ?? "unknown"} tips</Badge></div>
    </div>
    <div className="grid gap-5 xl:grid-cols-[minmax(0,1fr)_340px]">
      <div className="grid content-start gap-3 sm:grid-cols-2 2xl:grid-cols-3">
        {nodes.map(node => {
          const isSelected = selected?.node_id === node.node_id
          const isPrimary = node.status?.current_primary === node.node_id
          return <div key={node.node_id} className={`dashboard-card min-w-0 rounded-xl border transition-colors ${isSelected ? "border-primary/70 ring-1 ring-primary/30" : "border-border hover:border-primary/40"}`}>
            <button type="button" aria-label={`Inspect node ${node.node_id}`} aria-pressed={isSelected} onClick={() => setSelectedId(node.node_id)} className="w-full rounded-t-xl p-4 text-left focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-primary">
              <div className="flex items-start justify-between gap-2"><div className="flex min-w-0 items-center gap-2"><span className={`size-2.5 shrink-0 rounded-full ${healthColor(node.state)}`} /><span className="text-base font-semibold">Node {String(node.node_id).padStart(2, "0")}</span></div><div className="flex gap-1">{isPrimary && <Badge>Primary</Badge>}<ArrowUpRight className="size-4 text-muted-foreground" /></div></div>
              <div className="mt-1 flex items-center gap-2 text-xs text-muted-foreground"><span className="capitalize">{node.state}</span><span>·</span><span>{ageLabel(node.last_status_at_ms, now)}</span></div>
              <div className="mt-4 grid grid-cols-3 gap-2 border-t border-border/70 pt-3"><div><p className="data-label">Height</p><p className="data-value mt-1 text-lg font-medium">{node.status?.chain_height ?? "—"}</p></div><div><p className="data-label">View</p><p className="data-value mt-1 text-lg font-medium">{node.status?.current_view ?? "—"}</p></div><div><p className="data-label">Mempool</p><p className="data-value mt-1 text-lg font-medium">{node.status?.mempool_size ?? "—"}</p></div></div>
              <div className="mt-3 flex items-center justify-between text-xs text-muted-foreground"><span>Peers {node.status ? `${node.status.peer_count}/${node.status.expected_peers}` : "—"}</span><span>{node.tx_rate.toFixed(1)} tx/s</span></div>
            </button>
            <div className="flex items-center justify-between border-t border-border/70 px-4 py-2"><span className="text-[11px] text-muted-foreground">PID {node.pid}</span><Button size="xs" variant="ghost" disabled={!snapshot?.running || node.state === "dead" || busyId === node.node_id} onClick={() => run(node.node_id, () => onQuery(node.node_id))}><RefreshCw className={busyId === node.node_id ? "animate-spin" : ""} />Query now</Button></div>
          </div>
        })}
        {nodes.length === 0 && <Card className="col-span-full border-dashed"><CardContent className="py-12 text-center text-muted-foreground">Launch a cluster to see validator status here.</CardContent></Card>}
      </div>
      <Card className="dashboard-card h-fit border border-border xl:sticky xl:top-5">
        <CardHeader><div className="flex items-center justify-between"><div><p className="data-label">Selected validator</p><CardTitle className="mt-1 text-xl">{selected ? `Node ${selected.node_id}` : "No node selected"}</CardTitle></div>{selected && <span className={`size-3 rounded-full ${healthColor(selected.state)}`} />}</div></CardHeader>
        <CardContent className="space-y-4">
          {!selected ? <p className="text-sm text-muted-foreground">Node details will appear after launch.</p> : <>
            <div className="flex flex-wrap gap-2"><Badge variant={selected.state === "dead" ? "destructive" : "secondary"}>{selected.state}</Badge>{status?.current_primary === selected.node_id && <Badge>Current primary</Badge>}{stale && <Badge variant="outline">Stale status</Badge>}</div>
            <p className="text-xs text-muted-foreground">Last successful query: {ageLabel(selected.last_status_at_ms, now)}</p>
            <dl className="grid grid-cols-2 gap-x-4"><Detail label="Chain height" value={status?.chain_height} /><Detail label="Next sequence" value={status?.next_sequence} /><Detail label="View" value={status?.current_view} /><Detail label="Leader" value={status?.current_primary} /><Detail label="Peers" value={status ? `${status.peer_count}/${status.expected_peers}` : null} /><Detail label="Mempool" value={status?.mempool_size} /><Detail label="Committed tx" value={status?.transactions_committed} /><Detail label="Blocks / sec" value={selected.block_rate.toFixed(2)} /><Detail label="Latency" value={status ? `${status.average_commit_latency_ms.toFixed(1)} ms` : null} /><Detail label="Catch-up events" value={status?.catch_up_events} /></dl>
            <div className="rounded-lg border border-border/70 bg-background/60 p-3"><p className="data-label">Network address</p><p className="mt-1 break-all font-mono text-xs">{selected.address ?? "Not registered"}</p></div>
            <div className="flex flex-wrap gap-2"><Button size="sm" disabled={!active || busyId === selected.node_id} onClick={() => run(selected.node_id, () => onQuery(selected.node_id))}><RefreshCw />Query now</Button></div>
            <div className="space-y-2 border-t pt-4"><Label htmlFor="node-pause-duration">Pause duration (ms)</Label><div className="flex gap-2"><Input id="node-pause-duration" type="number" min={1} max={60000} value={pauseMs} onChange={event => setPauseMs(Number(event.target.value))} /><Button variant="secondary" disabled={!active || busyId === selected.node_id || !Number.isInteger(pauseMs) || pauseMs < 1 || pauseMs > 60000} onClick={() => run(selected.node_id, () => onPause(selected.node_id, pauseMs))}><Pause />Pause</Button></div><p className="text-xs text-muted-foreground">The node stops processing briefly, then resumes automatically.</p></div>
            <Button variant="destructive" className="w-full" disabled={!active || busyId === selected.node_id} onClick={() => confirmRef.current?.showModal()}><Skull />Kill node {selected.node_id}</Button>
            {status && <div className="border-t pt-4"><p className="data-label">Recent blocks</p><div className="mt-2 space-y-1">{status.recent_blocks.slice(0, 4).map(block => <div key={block.sequence} className="flex justify-between rounded bg-background/60 px-2 py-1 font-mono text-xs"><span>#{block.sequence}</span><span>{block.transaction_count} tx</span></div>)}{status.recent_blocks.length === 0 && <p className="text-xs text-muted-foreground">No blocks reported.</p>}</div></div>}
          </>}
          {actionError && <p role="alert" className="text-sm text-destructive">{actionError}</p>}
        </CardContent>
      </Card>
    </div>
    <div className="grid gap-3 md:grid-cols-2"><button type="button" onClick={onOpenMarket} className="dashboard-card flex items-center justify-between rounded-xl border border-border p-4 text-left transition-colors hover:border-primary/50 focus-visible:outline-2 focus-visible:outline-primary"><span><span className="data-label">Market data</span><span className="mt-1 block font-medium">See BTC-USD order book and trades</span></span><Activity className="text-primary" /></button><button type="button" onClick={onOpenParticipants} className="dashboard-card flex items-center justify-between rounded-xl border border-border p-4 text-left transition-colors hover:border-primary/50 focus-visible:outline-2 focus-visible:outline-primary"><span><span className="data-label">Simulation</span><span className="mt-1 block font-medium">Inspect {snapshot?.agents.length ?? 0} market participants</span></span><Wifi className="text-primary" /></button></div>
    <dialog ref={confirmRef} className="m-auto w-[min(92vw,420px)] rounded-xl border border-border bg-card p-0 text-foreground shadow-2xl backdrop:bg-black/75" onClick={event => { if (event.target === confirmRef.current) confirmRef.current.close() }}><div className="p-6"><div className="flex items-center gap-2 text-destructive"><WifiOff className="size-5" /><h3 className="text-lg font-semibold">Kill node {selected?.node_id}?</h3></div><p className="mt-3 text-sm text-muted-foreground">This terminates the validator process. The remaining nodes may need to elect another primary. The node cannot be restarted in this signed session.</p><div className="mt-6 flex justify-end gap-2"><Button variant="outline" onClick={() => confirmRef.current?.close()}>Cancel</Button><Button variant="destructive" onClick={() => { confirmRef.current?.close(); if (selected) void run(selected.node_id, () => onKill(selected.node_id)) }}>Kill node</Button></div></div></dialog>
  </div>
}
