"""Plot aggregate validator results; never copy keys or signed session files.

Usage: python3 scripts/plot_validator_followup.py --out docs/figures/validator-followup-2026-10-19 \
  --run 'baseline=benchmark_results/.../baseline-clean' --run 'pinned=benchmark_results/.../four-pinned-2200'
"""

import argparse
import json
import re
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt


def load(label, root):
    summary_path = max(root.rglob("summary.json"), key=lambda path: path.stat().st_mtime)
    summary = json.loads(summary_path.read_text())[0]
    run_path = next(summary_path.parent.rglob("run.json"))
    run = json.loads(run_path.read_text())
    log = next(summary_path.parent.rglob("node-0.stdout.log"))
    stages = {}
    for line in log.read_text().splitlines():
        if "PROFILE_STAGE node=0 " not in line:
            continue
        fields = dict(re.findall(r"([a-z0-9_]+)=([^\s]+)", line))
        stages[fields["stage"]] = {key: int(fields[key]) for key in ("count", "units", "total_us", "p95_us")}
    return label, summary, run, stages


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--run", action="append", required=True, help="label=benchmark-directory")
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    rows = [load(label, Path(root)) for label, root in (item.split("=", 1) for item in args.run)]
    plt.rcParams.update({"font.family": "DejaVu Sans", "font.size": 9,
                         "axes.spines.top": False, "axes.spines.right": False,
                         "axes.grid": True, "grid.alpha": 0.2})
    labels = [label for label, *_ in rows]
    x = range(len(rows))
    fig, axes = plt.subplots(1, 3, figsize=(13, 4.2), layout="constrained")
    for axis, key, title, unit in zip(axes,
                                       ("successful_tps", "scheduled_to_commit_p95_ms", "blocked_slots"),
                                       ("Useful throughput", "Durable-commit tail", "Missed driver slots"),
                                       ("tx/s", "ms", "slots")):
        values = [summary.get(key) or 0 for _, summary, _, _ in rows]
        colors = ["#148a8a" if summary.get("latency_slo_met") else "#d35c4a"
                  for _, summary, _, _ in rows]
        axis.bar(x, values, color=colors)
        axis.set(title=title, ylabel=unit, xticks=list(x), xticklabels=labels)
        axis.tick_params(axis="x", rotation=28)
        if key == "scheduled_to_commit_p95_ms":
            axis.axhline(500, color="#a83232", linestyle="--", linewidth=1, label="500 ms SLO")
            axis.legend(frameon=False)
        for i, value in enumerate(values):
            axis.annotate(f"{value:,.0f}", (i, value), xytext=(0, 3), textcoords="offset points", ha="center")
    fig.suptitle("Signed HotStuff · six markets · red = missed load/latency SLO")
    fig.savefig(args.out / "capacity.svg")
    fig.savefig(args.out / "capacity.png", dpi=160)
    plt.close(fig)

    selected = ("follower_auth", "follower_execute", "follower_state_root", "journal_sync",
                "proposal_transaction", "proposal_publish", "root_encode_values", "root_apply")
    stage_rows = [row for row in rows if "root_encode_values" in row[3]]
    fig, axis = plt.subplots(figsize=(10, 4.5), layout="constrained")
    width = 0.8 / len(stage_rows)
    for i, (label, _, _, stages) in enumerate(stage_rows):
        values = [stages.get(stage, {}).get("p95_us", 0) / 1000 for stage in selected]
        axis.bar([j - 0.4 + width * (i + 0.5) for j in range(len(selected))], values,
                 width=width, label=label)
    axis.set(ylabel="per-call p95 (ms)", xticks=list(range(len(selected))),
             xticklabels=[stage.replace("_", " ") for stage in selected])
    axis.tick_params(axis="x", rotation=30)
    axis.legend(frameon=False, ncol=2)
    axis.set_title("Overlapping local stages; bars must not be summed into end-to-end latency")
    fig.savefig(args.out / "stages.svg")
    fig.savefig(args.out / "stages.png", dpi=160)
    plt.close(fig)

    fig, axis = plt.subplots(figsize=(9, 4), layout="constrained")
    for label, _, run, _ in rows:
        samples = run.get("samples", [])
        seconds = [sample["elapsed_ms"] / 1000 for sample in samples]
        backlog = [max((node.get("mempool") or 0 for node in sample["nodes"]), default=0)
                   for sample in samples]
        axis.plot(seconds, backlog, label=label)
    axis.set(xlabel="measurement time (s)", ylabel="largest sampled node mempool",
             title="Backlog is a symptom of falling behind, not a mempool CPU profile")
    axis.legend(frameon=False, ncol=2)
    fig.savefig(args.out / "backlog.svg")
    fig.savefig(args.out / "backlog.png", dpi=160)
    plt.close(fig)


if __name__ == "__main__":
    main()
