import { useEffect, useState } from "react"
import { Pause, Play, Skull, Square } from "lucide-react"
import { api } from "@/api"
import type { ControlSnapshot } from "@/bindings/ControlSnapshot"
import type { ConsensusAlgorithm } from "@/bindings/ConsensusAlgorithm"
import type { LaunchConfig } from "@/bindings/LaunchConfig"
import type { RemoteChoice } from "@/bindings/RemoteChoice"
import type { PollMode } from "@/bindings/PollMode"
import type { TargetNode } from "@/bindings/TargetNode"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { ScrollArea } from "@/components/ui/scroll-area"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"

type Props = {
  snapshot: ControlSnapshot | null
  config: LaunchConfig | null
  setConfig: (config: LaunchConfig) => void
  command: (operation: () => Promise<void>) => Promise<void>
  onLaunch: () => Promise<void>
  onStop: () => Promise<void>
}

function NumberField({ label, value, onChange }: { label: string; value: number; onChange: (value: number) => void }) {
  return <div className="space-y-1.5"><Label>{label}</Label><Input type="number" value={value} onChange={event => onChange(Number(event.target.value))} /></div>
}

function targetFromString(value: string): TargetNode {
  return value === "random" || value === "primary" ? { kind: value } : { kind: "node", node_id: Number(value.slice(5)) }
}

function TargetSelect({ value, onChange, snapshot }: { value: string; onChange: (value: string) => void; snapshot: ControlSnapshot | null }) {
  return <Select value={value} onValueChange={next => next && onChange(next)}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="random">Random node</SelectItem><SelectItem value="primary">Current primary</SelectItem>{snapshot?.nodes.filter(node => node.state !== "dead").map(node => <SelectItem key={node.node_id} value={`node:${node.node_id}`}>Node {node.node_id}</SelectItem>)}</SelectContent></Select>
}

export function OperationsTab({ snapshot, config, setConfig, command, onLaunch, onStop }: Props) {
  const [remoteProfiles, setRemoteProfiles] = useState<RemoteChoice[]>([])
  useEffect(() => { void api.remoteProfiles().then(setRemoteProfiles).catch(() => setRemoteProfiles([])) }, [])
  const [faultTarget, setFaultTarget] = useState("primary"), [pauseMs, setPauseMs] = useState(5000)
  const [visibleLogs, setVisibleLogs] = useState(200)
  const bestStatus = snapshot?.nodes.filter(node => node.state === "ok" && node.status).sort((a, b) => (b.status?.chain_height ?? 0) - (a.status?.chain_height ?? 0))[0]?.status
  const logs = snapshot?.logs ?? []
  const shownLogs = logs.slice(-visibleLogs)

  const setConfigNumber = (key: "node_count" | "block_time_ms" | "tx_batch_window_ms" | "timeout_ms" | "poll_ms" | "claim_limit" | "claim_usd" | "claim_btc") => (value: number) => config && setConfig({ ...config, [key]: value })
  const remoteCount = (id: string) => config?.remote_placements.find(host => host.profile_id === id)?.count ?? 0
  const localCount = (config?.node_count ?? 0) - (config?.remote_placements.reduce((sum, host) => sum + host.count, 0) ?? 0)
  const setRemoteCount = (id: string, value: number) => {
    if (!config) return
    const count = Number.isFinite(value) ? Math.max(0, Math.floor(value)) : 0
    const remote_placements = config.remote_placements.filter(host => host.profile_id !== id)
    if (count > 0) remote_placements.push({ profile_id: id, count })
    setConfig({
      ...config, remote_placements,
      timeout_ms: count > 0 ? Math.max(config.timeout_ms, 8000) : config.timeout_ms,
      block_time_ms: count > 0 ? Math.max(config.block_time_ms, 1000) : config.block_time_ms,
    })
  }

  return <div className="space-y-5">
    <div><p className="data-label text-primary">Configuration</p><h2 className="mt-1 text-2xl font-semibold tracking-tight">Operations</h2><p className="mt-1 text-sm text-muted-foreground">Cluster settings, fault injection, and logs.</p></div>
    {config && <Card className="dashboard-card"><CardHeader><div className="flex flex-wrap items-center justify-between gap-3"><CardTitle>Simulation</CardTitle><Badge variant={snapshot?.running ? "default" : "secondary"}>{snapshot?.stopping ? "Stopping" : snapshot?.launching ? "Launching" : snapshot?.running ? "Running" : "Stopped"}</Badge></div></CardHeader><CardContent className="space-y-5">
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4"><NumberField label="Nodes" value={config.node_count} onChange={setConfigNumber("node_count")} /><NumberField label="Block time (ms)" value={config.block_time_ms} onChange={setConfigNumber("block_time_ms")} />{((config.consensus === "hotstuff" && config.chained_hotstuff) || config.consensus === "hotstuff2" || config.consensus === "votor") && <NumberField label="TX batch window (ms)" value={config.tx_batch_window_ms} onChange={setConfigNumber("tx_batch_window_ms")} />}<NumberField label="Consensus timeout (ms)" value={config.timeout_ms} onChange={setConfigNumber("timeout_ms")} /><NumberField label="Poll interval (ms)" value={config.poll_ms} onChange={setConfigNumber("poll_ms")} /></div>
      <div className="space-y-3"><Label>Validator placement</Label><div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4"><div className="rounded-lg border border-border bg-background/40 p-3"><span className="text-sm font-medium">This laptop</span><p className="mt-1 text-2xl font-semibold">{localCount}</p></div>{remoteProfiles.map(profile => <div key={profile.id} className="rounded-lg border border-border bg-background/40 p-3"><Label htmlFor={`host-${profile.id}`}>{profile.label}</Label><Input id={`host-${profile.id}`} className="mt-2" type="number" min={0} step={1} value={remoteCount(profile.id)} onChange={event => setRemoteCount(profile.id, Number(event.target.value))} /></div>)}</div><p className="text-xs text-muted-foreground">Node IDs are assigned locally first, then to remote hosts. Hosts come from the ignored control-plane/.local/remotes.json. Keep at least one local validator; multi-remote sessions use Tailscale peer addresses.</p>{localCount < 1 && <p className="text-sm text-destructive">Reduce remote counts or increase the total node count.</p>}</div>
      <details className="rounded-lg border border-border bg-background/40 p-3"><summary className="cursor-pointer text-sm font-medium">Airdrop settings <span className="ml-2 text-xs font-normal text-muted-foreground">{bestStatus?.faucet_remaining_claims ?? config.claim_limit} claims remaining · {config.claim_usd} USD + {config.claim_btc} BTC each</span></summary><div className="grid gap-3 pt-3 sm:grid-cols-3"><NumberField label="Maximum claims" value={config.claim_limit} onChange={setConfigNumber("claim_limit")} /><NumberField label="USD per claim" value={config.claim_usd} onChange={setConfigNumber("claim_usd")} /><NumberField label="BTC per claim" value={config.claim_btc} onChange={setConfigNumber("claim_btc")} /></div><p className="pt-2 text-xs text-muted-foreground">Claims mint these amounts when committed. Settings are fixed for the session.</p></details>
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"><div className="space-y-1.5"><Label>Consensus algorithm</Label><Select value={config.consensus} onValueChange={value => value && setConfig({ ...config, consensus: value as ConsensusAlgorithm, tx_batch_window_ms: value === "hotstuff2" || value === "votor" ? 0 : config.tx_batch_window_ms || 50, recover_existing: false })}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="hotstuff">HotStuff</SelectItem><SelectItem value="hotstuff2">HotStuff-2 (research)</SelectItem><SelectItem value="votor">Votor-style (research)</SelectItem><SelectItem value="pbft">PBFT</SelectItem></SelectContent></Select></div><div className="space-y-1.5"><Label htmlFor="auto-view-change">Rotate primary every N blocks</Label><Input id="auto-view-change" type="number" min={1} step={1} placeholder="Off" value={config.auto_view_change_blocks ?? ""} onChange={event => setConfig({ ...config, auto_view_change_blocks: event.target.value === "" ? null : Number(event.target.value) })} /><p className="text-xs text-muted-foreground">Blank disables automatic view changes. Genesis does not count.</p></div><div className="space-y-1.5"><Label>Status polling</Label><Select value={config.poll_mode} onValueChange={value => value && setConfig({ ...config, poll_mode: value as PollMode })}><SelectTrigger><SelectValue /></SelectTrigger><SelectContent><SelectItem value="pinned">One fixed node</SelectItem><SelectItem value="round_robin">Rotate one node (default)</SelectItem><SelectItem value="all">All nodes per interval</SelectItem></SelectContent></Select><p className="text-xs text-muted-foreground">Round-robin polls one node every {config.poll_ms} ms; Query now always fetches a chosen node.</p></div></div>
      {config.consensus === "hotstuff" && config.chained_hotstuff && <p className="text-xs text-muted-foreground">The batch window starts when the leader first has transactions. 0 uses the periodic block timer; 50 ms is the default. Maximum 1000 ms.</p>}
      {(config.consensus === "hotstuff2" || config.consensus === "votor") && <p className="text-xs text-muted-foreground">A zero batch window proposes immediately. A positive window coalesces transactions for at most that many milliseconds; measure its effect under sustained load.</p>}
      {config.consensus === "hotstuff" && <label className="flex items-center gap-2 text-sm"><input type="checkbox" checked={config.chained_hotstuff} onChange={event => setConfig({ ...config, chained_hotstuff: event.target.checked, recover_existing: false })} />Chained HotStuff (default)</label>}
      <details className="rounded-lg border border-border bg-background/40 p-3"><summary className="cursor-pointer text-sm font-medium">Paths and session security</summary><div className="mt-4 grid gap-4 md:grid-cols-2"><div className="space-y-1.5"><Label>Project directory</Label><Input value={config.project_dir} onChange={event => setConfig({ ...config, project_dir: event.target.value })} /></div><div className="space-y-1.5"><Label>Node binary</Label><Input value={config.binary_path} onChange={event => setConfig({ ...config, binary_path: event.target.value })} /><p className="text-xs text-muted-foreground">Release builds are recommended for larger HotStuff clusters.</p></div><div className="space-y-1.5"><Label>Node data directory</Label><Input value={config.data_dir} onChange={event => setConfig({ ...config, data_dir: event.target.value })} /></div><div className="space-y-1.5 text-xs text-muted-foreground"><p>Chain ID: <span className="break-all text-foreground">{bestStatus?.chain_id || "—"}</span></p><p>Ed25519 signing: {bestStatus?.signing_enabled ? "enabled" : "waiting"}</p><p>Rejected authentication: {snapshot?.nodes.reduce((sum, node) => sum + (node.status?.authentication_rejections ?? 0), 0) ?? 0}</p><p>Session: <span className="break-all">{snapshot?.active_data_dir ?? "none"}</span></p><p>Signed sessions cannot restart validators until vote and lock state is durable.</p></div></div></details>
      <div className="flex gap-2"><Button onClick={() => void onLaunch()} disabled={localCount < 1 || snapshot?.running || snapshot?.launching || snapshot?.stopping}><Play />Launch cluster</Button><Button variant="destructive" onClick={() => void onStop()} disabled={snapshot?.stopping || (!snapshot?.running && !snapshot?.launching)}><Square />Stop all</Button></div>
    </CardContent></Card>}


    <div className="grid gap-5 xl:grid-cols-[330px_minmax(0,1fr)]"><Card className="dashboard-card"><CardHeader><CardTitle>Fault injection</CardTitle></CardHeader><CardContent className="space-y-3"><div className="space-y-1.5"><Label>Target</Label><TargetSelect value={faultTarget} onChange={setFaultTarget} snapshot={snapshot} /></div><NumberField label="Pause duration (ms)" value={pauseMs} onChange={setPauseMs} /><div className="flex gap-2"><Button variant="secondary" disabled={!snapshot?.running || pauseMs < 1} onClick={() => void command(() => api.pauseNode(targetFromString(faultTarget), pauseMs))}><Pause />Pause</Button><Button variant="destructive" disabled={!snapshot?.running} onClick={() => { if (window.confirm(`Kill ${faultTarget === "primary" ? "the current primary" : faultTarget === "random" ? "a random node" : faultTarget.replace(":", " ")}?`)) void command(() => api.killNode(targetFromString(faultTarget))) }}><Skull />Kill</Button></div><p className="text-xs text-muted-foreground">For a specific node, use its card on Overview for status and targeted actions.</p></CardContent></Card>
    <Card className="dashboard-card"><CardHeader><div className="flex items-center justify-between"><CardTitle>Controller / node logs</CardTitle><Badge variant="outline">{logs.length} buffered</Badge></div></CardHeader><CardContent><ScrollArea className="h-72 rounded-lg border border-border bg-[#070b0f] p-3"><div className="space-y-1 font-mono text-[11px] leading-relaxed text-slate-300">{shownLogs.map((line, index) => <div key={`${logs.length - shownLogs.length + index}-${line.slice(0, 20)}`}>{line}</div>)}{shownLogs.length === 0 && <p className="text-muted-foreground">No logs yet.</p>}</div></ScrollArea>{visibleLogs < logs.length && <Button className="mt-3" size="sm" variant="ghost" onClick={() => setVisibleLogs(count => count + 200)}>Show older logs</Button>}</CardContent></Card></div>
  </div>
}
