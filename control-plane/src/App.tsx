import { useEffect, useState } from "react"
import { Activity, ArrowRight, Boxes, Clock3, Gauge, RadioTower, Send, Settings2, Users } from "lucide-react"
import { api } from "@/api"
import { NodeOverview } from "@/NodeOverview"
import { OperationsTab } from "@/OperationsTab"
import { OrderBookView } from "@/OrderBookView"
import { ParticipantsTab } from "@/ParticipantsTab"
import type { ControlSnapshot } from "@/bindings/ControlSnapshot"
import type { LaunchConfig } from "@/bindings/LaunchConfig"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"

type Section = "overview" | "market" | "participants" | "operations"

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

  useEffect(() => {
    let cancelled = false
    let timer: number | undefined
    void api.defaultConfig().then(value => { if (!cancelled) setConfig(value) }).catch(err => { if (!cancelled) setError(String(err)) })
    async function refresh() {
      try {
        const next = await api.snapshot()
        if (!cancelled) setSnapshot(next)
      } catch (err) {
        if (!cancelled) setError(String(err))
      } finally {
        if (!cancelled) timer = window.setTimeout(refresh, 500)
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
  const consensus = config?.consensus === "hotstuff" ? "HotStuff" : "PBFT"

  return <div className="min-h-screen bg-background text-foreground">
    <header className="border-b border-border/80 bg-card/75">
      <div className="mx-auto flex max-w-[1760px] flex-wrap items-center justify-between gap-4 px-4 py-4 sm:px-6 lg:px-8">
        <div className="flex items-center gap-3"><div className="flex size-10 items-center justify-center rounded-xl border border-primary/30 bg-primary/10 text-primary"><RadioTower className="size-5" /></div><div><h1 className="text-lg font-semibold leading-tight tracking-tight">LIGHT DEX <span className="font-normal text-muted-foreground">/ Control Plane</span></h1><p className="text-xs text-muted-foreground">Validators, markets, participants</p></div></div>
        <div className="flex flex-wrap items-center gap-2"><Badge variant="outline">{consensus}</Badge><Badge variant={snapshot?.running ? "default" : "secondary"} className="gap-1.5"><span className={`size-1.5 rounded-full ${snapshot?.running ? "bg-background" : "bg-muted-foreground"}`} />{runState}</Badge><Button size="sm" variant="outline" onClick={() => setSection("operations")}><Settings2 className="size-4" />Configure</Button></div>
      </div>
    </header>
    <main className="mx-auto max-w-[1760px] space-y-6 px-4 py-6 sm:px-6 lg:px-8">
      {error && <div role="alert" className="flex items-start justify-between gap-3 rounded-lg border border-destructive/50 bg-destructive/10 px-4 py-3 text-sm text-destructive"><span>{error}</span><button type="button" aria-label="Dismiss error" onClick={() => setError(null)}>×</button></div>}
      <div className="flex flex-wrap items-end justify-between gap-4"><div><p className="data-label text-primary">Network</p><h2 className="mt-1 text-3xl font-semibold tracking-tight">Validator status</h2></div><div className="flex items-center gap-2 text-xs text-muted-foreground"><Activity className="size-4 text-primary" />{metrics?.responsive_nodes ?? 0} responsive of {snapshot?.nodes.length ?? 0} validators</div></div>
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3 2xl:grid-cols-6">
        <Metric label="Alive nodes" value={String(metrics?.alive_nodes ?? 0)} detail={`${metrics?.responsive_nodes ?? 0} answering status`} icon={<RadioTower className="size-4" />} />
        <Metric label="Submitted" value={(metrics?.submitted ?? 0).toLocaleString()} detail="Control-plane total" icon={<Send className="size-4" />} />
        <Metric label="Submit rate" value={`${(metrics?.submit_tps ?? 0).toFixed(1)} tx/s`} detail="Rolling 5-second window" icon={<ArrowRight className="size-4" />} />
        <Metric label="Commit rate" value={`${(metrics?.commit_tps ?? 0).toFixed(1)} tx/s`} detail="Median responsive node" icon={<Gauge className="size-4" />} />
        <Metric label="Blocks" value={`${(metrics?.blocks_per_second ?? 0).toFixed(2)} /s`} detail="Median responsive node" icon={<Boxes className="size-4" />} />
        <Metric label="Commit latency" value={`${(metrics?.commit_latency_ms ?? 0).toFixed(1)} ms`} detail="Reported by validators" icon={<Clock3 className="size-4" />} />
      </div>
      <Tabs value={section} onValueChange={value => setSection(value as Section)} className="space-y-5">
        <TabsList className="h-auto w-full justify-start gap-1 overflow-x-auto rounded-xl border border-border bg-card p-1.5 sm:w-auto">
          <TabsTrigger value="overview">Overview</TabsTrigger><TabsTrigger value="market">Market</TabsTrigger><TabsTrigger value="participants"><Users className="size-4" />Participants</TabsTrigger><TabsTrigger value="operations">Operations</TabsTrigger>
        </TabsList>
        <TabsContent value="overview"><NodeOverview snapshot={snapshot} onQuery={queryNode} onPause={pauseNode} onKill={killNode} onOpenMarket={() => setSection("market")} onOpenParticipants={() => setSection("participants")} /></TabsContent>
        <TabsContent value="market"><OrderBookView snapshot={snapshot} /></TabsContent>
        <TabsContent value="participants"><ParticipantsTab snapshot={snapshot} command={command} /></TabsContent>
        <TabsContent value="operations"><OperationsTab snapshot={snapshot} config={config} setConfig={setConfig} command={command} onLaunch={() => config ? command(() => api.launch(config)) : Promise.resolve()} onStop={() => command(api.stopAll)} /></TabsContent>
      </Tabs>
    </main>
  </div>
}

export default App
