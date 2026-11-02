import { useEffect, useState } from "react"
import { Activity, ArrowRight, Boxes, Clock3, Gauge, RadioTower, Send, Settings2 } from "lucide-react"
import { api } from "@/api"
import { NodeOverview } from "@/NodeOverview"
import { OperationsTab } from "@/OperationsTab"
import { MarketWorkspace } from "@/MarketWorkspace"
import { ParticipantsTab } from "@/ParticipantsTab"
import { BlockExplorer } from "@/BlockExplorer"
import type { ControlSnapshot } from "@/bindings/ControlSnapshot"
import type { LaunchConfig } from "@/bindings/LaunchConfig"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Tabs, TabsContent } from "@/components/ui/tabs"

type Section = "overview" | "market" | "participants" | "operations" | "explorer"

function Metric({ label, value, detail, icon }: { label: string; value: string; detail: string; icon: React.ReactNode }) {
  return <div className="dashboard-card min-w-0 rounded-xl border border-border p-4">
    <div className="flex items-center justify-between gap-2"><p className="data-label">{label}</p><span className="text-primary/80">{icon}</span></div>
    <p className="mt-3 truncate text-2xl font-semibold tabular-nums tracking-tight" title={value}>{value}</p>
    <p className="mt-1 text-xs text-muted-foreground">{detail}</p>
  </div>
}

function App() {
  const [section, setSection] = useState<Section>("overview")
  const [config, setConfig] = useState<LaunchConfig | null>(null)
  const [snapshot, setSnapshot] = useState<ControlSnapshot | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [walletLabel, setWalletLabel] = useState<number | null>(null)

  useEffect(() => { setWalletLabel(null) }, [snapshot?.active_data_dir])

  useEffect(() => {
    let cancelled = false
    let timer: number | undefined
    void api.defaultConfig().then(value => { if (!cancelled) setConfig(value) }).catch(err => { if (!cancelled) setError(String(err)) })
    async function refresh() {
      let nextDelay = 500
      try {
        const next = await api.snapshot()
        if (next.agents.length >= 100) nextDelay = 1500
        else if (next.agents.length >= 50) nextDelay = 1000
        if (!cancelled) setSnapshot(next)
      } catch (err) {
        if (!cancelled) setError(String(err))
      } finally {
        if (!cancelled) timer = window.setTimeout(refresh, nextDelay)
      }
    }
    void refresh()
    return () => { cancelled = true; window.clearTimeout(timer) }
  }, [])

  async function command(operation: () => Promise<void>) {
    setError(null)
    try { await operation() } catch (err) { setError(String(err)) }
  }

  async function queryNode(nodeId: number) {
    const fresh = await api.refreshNodeStatus(nodeId)
    setSnapshot(current => current && ({ ...current, nodes: current.nodes.map(node => node.node_id === nodeId ? fresh : node) }))
  }

  async function pauseNode(nodeId: number, millis: number) {
    await api.pauseNode({ kind: "node", node_id: nodeId }, millis)
    setSnapshot(await api.snapshot())
  }

  async function killNode(nodeId: number) {
    await api.killNode({ kind: "node", node_id: nodeId })
    setSnapshot(await api.snapshot())
  }

  const metrics = snapshot?.metrics
  const runState = snapshot?.stopping ? "Stopping" : snapshot?.launching ? "Launching" : snapshot?.running ? "Live" : "Offline"
  const consensus = config?.consensus === "pbft" ? "PBFT"
    : config?.consensus === "hotstuff2" ? "HotStuff-2"
    : config?.consensus === "votor" ? "Votor-style"
    : config?.chained_hotstuff ? "Chained HotStuff" : "HotStuff (classic)"

  return <div className="min-h-screen bg-background text-foreground">
    <header className="border-b border-border/80 bg-card/75">
      <div className="mx-auto flex max-w-[1760px] flex-wrap items-center justify-between gap-4 px-4 py-4 sm:px-6 lg:px-8">
        <div className="flex items-center gap-3"><div className="flex size-10 items-center justify-center rounded-xl border border-primary/30 bg-primary/10 text-primary"><RadioTower className="size-5" /></div><div><h1 className="text-lg font-semibold leading-tight tracking-tight">LIGHT DEX <span className="font-normal text-muted-foreground">/ Control Plane</span></h1><p className="text-xs text-muted-foreground">Validators, markets, participants</p></div></div>
        <nav aria-label="Control plane sections" className="flex flex-wrap gap-1">
          {([ ["overview", "Node monitoring"], ["operations", "Node operations"], ["market", "Market"], ["participants", "Market participants"], ["explorer", "Explorer"] ] as const).map(([key, label]) =>
            <button key={key} type="button" aria-current={section === key ? "page" : undefined} onClick={() => setSection(key)} className={`rounded-md px-3 py-2 text-sm transition-colors ${section === key ? "bg-primary/15 text-primary" : "text-muted-foreground hover:bg-muted hover:text-foreground"}`}>{label}</button>
          )}
        </nav>
        <div className="flex flex-wrap items-center gap-2"><Badge variant="outline">{consensus}</Badge><Badge variant={snapshot?.running ? "default" : "secondary"} className="gap-1.5"><span className={`size-1.5 rounded-full ${snapshot?.running ? "bg-background" : "bg-muted-foreground"}`} />{runState}</Badge><Button size="sm" variant="outline" onClick={() => setSection("operations")}><Settings2 className="size-4" />Configure</Button></div>
      </div>
    </header>
    <main className="mx-auto max-w-[1760px] space-y-6 px-4 py-6 sm:px-6 lg:px-8">
      {error && <div role="alert" className="flex items-start justify-between gap-3 rounded-lg border border-destructive/50 bg-destructive/10 px-4 py-3 text-sm text-destructive"><span>{error}</span><button type="button" aria-label="Dismiss error" onClick={() => setError(null)}>×</button></div>}
      {section === "overview" && <><div className="flex flex-wrap items-end justify-between gap-4"><div><p className="data-label text-primary">Network</p><h2 className="mt-1 text-3xl font-semibold tracking-tight">Validator status</h2></div><div className="flex items-center gap-2 text-xs text-muted-foreground"><Activity className="size-4 text-primary" />{metrics?.responsive_nodes ?? 0} responsive of {snapshot?.nodes.length ?? 0} validators</div></div>
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3 2xl:grid-cols-6">
        <Metric label="Alive nodes" value={String(metrics?.alive_nodes ?? 0)} detail={`${metrics?.responsive_nodes ?? 0} answering status`} icon={<RadioTower className="size-4" />} />
        <Metric label="Submitted" value={(metrics?.submitted ?? 0).toLocaleString()} detail="Control-plane total" icon={<Send className="size-4" />} />
        <Metric label="Submit rate" value={`${(metrics?.submit_tps ?? 0).toFixed(1)} tx/s`} detail="Rolling 5-second window" icon={<ArrowRight className="size-4" />} />
        <Metric label="Commit rate" value={`${(metrics?.commit_tps ?? 0).toFixed(1)} tx/s`} detail="Median responsive node" icon={<Gauge className="size-4" />} />
        <Metric label="Blocks" value={`${(metrics?.blocks_per_second ?? 0).toFixed(2)} /s`} detail="Median node, since last poll" icon={<Boxes className="size-4" />} />
        <Metric label="Tx commit latency" value={`${(metrics?.commit_latency_ms ?? 0).toFixed(1)} ms`} detail="Lifetime mean, receipt to commit" icon={<Clock3 className="size-4" />} />
      </div></>}
      <Tabs value={section} onValueChange={value => setSection(value as Section)} className="space-y-5">
        <TabsContent value="overview"><NodeOverview snapshot={snapshot} onQuery={queryNode} onPause={pauseNode} onKill={killNode} onOpenMarket={() => setSection("market")} onOpenParticipants={() => setSection("participants")} /></TabsContent>
        <TabsContent value="market"><MarketWorkspace snapshot={snapshot} onError={setError} walletLabel={walletLabel} onWalletChange={setWalletLabel} /></TabsContent>
        <TabsContent value="participants" keepMounted><ParticipantsTab snapshot={snapshot} command={command} onLoginAs={label => { setWalletLabel(label); setSection("market") }} /></TabsContent>
        <TabsContent value="operations"><OperationsTab snapshot={snapshot} config={config} setConfig={setConfig} command={command} onLaunch={() => config ? command(() => api.launch(config)) : Promise.resolve()} onStop={() => command(api.stopAll)} /></TabsContent>
        <TabsContent value="explorer"><BlockExplorer snapshot={snapshot} /></TabsContent>
      </Tabs>
    </main>
  </div>
}

export default App
