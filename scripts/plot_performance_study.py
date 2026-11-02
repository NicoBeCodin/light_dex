"""Render article-ready graphs from ignored Light DEX benchmark artifacts.

Only aggregate summaries and timing profiles are read; simulation keys and
signed session manifests are never copied into the output directory.
"""

from __future__ import annotations

import argparse
import json
import re
from statistics import median
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt


MARKETS = {"0": "BTC spot", "1": "BTC perp", "2": "ETH spot",
           "3": "ETH perp", "8": "SOL spot", "9": "SOL perp"}
COLORS = {"all": "#00b7c2", "data": "#f59e0b"}


def records(root: Path):
    for path in sorted(root.rglob("summary.json")):
        # Exclude an overlapped probe and the pre-integer-scheduler 2200 run.
        if "driveronly1400-all" in path.parts or "driveronly2200-all-16w" in path.parts:
            continue
        for summary in json.loads(path.read_text()):
            if summary.get("workload") != "balanced_six":
                continue
            if summary.get("unaccounted_slots", 0) != 0:
                continue
            run = next(path.parent.rglob("run.json"), None)
            mode = "data" if "-data" in str(path) else "all"
            yield {"summary": summary, "path": path, "run": run, "mode": mode}


def style():
    plt.rcParams.update({
        "font.family": "DejaVu Sans", "font.size": 10,
        "axes.spines.top": False, "axes.spines.right": False,
        "axes.grid": True, "grid.alpha": 0.2,
        "figure.facecolor": "#ffffff", "axes.facecolor": "#ffffff",
        "savefig.facecolor": "#ffffff",
    })


def save_graph(fig, out, name):
    fig.savefig(out / f"{name}.svg")
    fig.savefig(out / f"{name}.png", dpi=160)


def capacity(rows, out):
    selected = [row for row in rows if row["summary"]["nodes"] == 7 and
                row["summary"]["participants"] == 480 and
                row["summary"]["algorithm"] == "hotstuff" and
                row["summary"]["result"] == "pass"]
    if not selected:
        return
    fig, axes = plt.subplots(1, 2, figsize=(11, 4.1), layout="constrained")
    for mode in ["all", "data"]:
        points = sorted((row for row in selected if row["mode"] == mode),
                        key=lambda row: row["summary"]["offered_tps"])
        if not points:
            continue
        x = [p["summary"]["offered_tps"] for p in points]
        throughput = [p["summary"]["successful_tps"] for p in points]
        latency = [p["summary"].get("scheduled_to_commit_p95_ms") for p in points]
        axes[0].plot(x, throughput, "o-", label=f"journal {mode}", color=COLORS[mode])
        axes[1].plot(x, latency, "o-", label=f"journal {mode}", color=COLORS[mode])
        for point in points:
            summary = point["summary"]
            if not summary.get("latency_slo_met", False):
                axes[0].scatter(summary["offered_tps"], summary["successful_tps"],
                                marker="x", s=95, color="#d94841", zorder=5)
                axes[1].scatter(summary["offered_tps"],
                                summary.get("scheduled_to_commit_p95_ms"),
                                marker="x", s=95, color="#d94841", zorder=5)
    axes[0].set(title="Useful throughput", xlabel="Additional scheduled transactions/s",
                ylabel="Successful transactions/s")
    axes[1].axhline(500, linestyle="--", color="#d94841", linewidth=1, label="500 ms target")
    axes[1].set(title="Submission-to-durable-commit tail", xlabel="Additional scheduled transactions/s",
                ylabel="Scheduled-to-commit p95 (ms)")
    for axis in axes:
        axis.legend(frameon=False)
    fig.suptitle("Seven validators · BTC/ETH/SOL spot and perp · red X = SLO failure")
    save_graph(fig, out, "capacity")
    plt.close(fig)


def driver_control(rows, out):
    selected = sorted((row for row in rows if row["summary"]["nodes"] == 7 and
                       row["summary"]["participants"] == 0 and
                       row["summary"]["algorithm"] == "hotstuff" and
                       row["summary"]["result"] == "pass" and
                       55 <= row["summary"]["measured_seconds"] <= 65),
                      key=lambda row: row["summary"]["offered_tps"])
    if not selected:
        return
    fig, axes = plt.subplots(1, 2, figsize=(11, 4.1), layout="constrained")
    series = [
        ("all", 8, "#00b7c2"),
        ("all", 16, "#5b6ee1"),
        ("data", 8, "#f59e0b"),
        ("data", 16, "#a35bd6"),
    ]
    for mode, workers, color in series:
        samples = [row["summary"] for row in selected if row["mode"] == mode and
                   ("-16w" in str(row["path"])) == (workers == 16)]
        if not samples:
            continue
        grouped = {rate: [point for point in samples if point["offered_tps"] == rate]
                   for rate in sorted({point["offered_tps"] for point in samples})}
        x = list(grouped)
        medians = lambda key: [median(point[key] for point in grouped[rate]) for rate in x]
        axes[0].plot(x, medians("successful_tps"),
                     "o-", color=color, label=f"{mode} · {workers} workers")
        axes[1].plot(x, medians("scheduled_to_commit_p95_ms"),
                     "o-", color=color, label=f"{mode} · {workers} workers")
        for point in samples:
            axes[0].scatter(point["offered_tps"], point["successful_tps"],
                            s=16, color=color, alpha=0.45, zorder=4)
            axes[1].scatter(point["offered_tps"], point["scheduled_to_commit_p95_ms"],
                            s=16, color=color, alpha=0.45, zorder=4)
            if not point.get("latency_slo_met", False):
                for axis, key in zip(axes, ["successful_tps", "scheduled_to_commit_p95_ms"]):
                    axis.scatter(point["offered_tps"], point[key], marker="x", s=95,
                                 color="#d94841", zorder=5)
    axes[0].plot([min(row["summary"]["offered_tps"] for row in selected),
                  max(row["summary"]["offered_tps"] for row in selected)],
                 [min(row["summary"]["offered_tps"] for row in selected),
                  max(row["summary"]["offered_tps"] for row in selected)],
                 linestyle="--", color="#777777", linewidth=1, label="perfect delivery")
    axes[0].set(title="Successful transactions", xlabel="Scheduled driver transactions/s",
                ylabel="Successful transactions/s")
    axes[1].axhline(500, linestyle="--", color="#d94841", linewidth=1)
    axes[1].set(title="Submission-to-durable-commit p95", xlabel="Scheduled driver transactions/s",
                ylabel="Latency (ms)")
    for axis in axes:
        axis.legend(frameon=False)
    fig.suptitle("Seven validators · no stochastic participants · all six markets")
    save_graph(fig, out, "driver-control")
    plt.close(fig)


def node_scaling(rows, out):
    selected = [row for row in rows if row["summary"]["participants"] == 0 and
                row["summary"]["algorithm"] == "hotstuff" and
                row["mode"] == "all" and row["summary"]["nodes"] in (7, 13) and
                row["summary"]["offered_tps"] in (1000, 1800) and
                55 <= row["summary"]["measured_seconds"] <= 65 and
                (row["summary"]["offered_tps"] != 1800 or "-16w" in str(row["path"]))]
    if not selected:
        return
    fig, axes = plt.subplots(1, 2, figsize=(11, 4.1), layout="constrained")
    for rate, color in [(1000, "#00b7c2"), (1800, "#6c5ce7")]:
        subset = [row["summary"] for row in selected if row["summary"]["offered_tps"] == rate]
        groups = {count: [point for point in subset if point["nodes"] == count]
                  for count in sorted({point["nodes"] for point in subset})}
        if not groups:
            continue
        counts = list(groups)
        for axis, key in zip(axes, ["successful_tps", "scheduled_to_commit_p95_ms"]):
            axis.plot(counts, [median(point[key] for point in groups[count]) for count in counts],
                      "o-", color=color, label=f"{rate} scheduled TPS")
            for point in subset:
                if point["result"] != "pass" or not point.get("latency_slo_met", False):
                    axis.scatter(point["nodes"], point[key], marker="x", s=100,
                                 color="#d94841", zorder=5)
    axes[0].set(title="Useful throughput", xlabel="Validators on one laptop",
                ylabel="Successful transactions/s", xticks=[7, 13])
    axes[1].axhline(500, linestyle="--", color="#d94841", linewidth=1)
    axes[1].set(title="Durable-commit tail", xlabel="Validators on one laptop",
                ylabel="Scheduled-to-commit p95 (ms)", xticks=[7, 13])
    for axis in axes:
        axis.legend(frameon=False)
    fig.suptitle("Same six-market driver · red X = invalid or SLO failure")
    save_graph(fig, out, "node-scaling")
    plt.close(fig)


def market_mix(rows, out):
    selected = [row for row in rows if row["summary"]["nodes"] == 7 and
                row["summary"]["participants"] == 480 and
                row["summary"]["algorithm"] == "hotstuff" and
                row["summary"]["result"] == "pass" and row["mode"] == "all"]
    if not selected:
        return
    row = min(selected, key=lambda item: abs(item["summary"]["offered_tps"] - 500))
    markets = row["summary"].get("per_market_outcomes", {})
    keys = list(MARKETS)
    success = [markets.get(key, {}).get("successful", 0) for key in keys]
    failed = [markets.get(key, {}).get("failed", 0) for key in keys]
    fills = [markets.get(key, {}).get("fills", 0) for key in keys]
    fig, axes = plt.subplots(1, 2, figsize=(11, 4.1), layout="constrained")
    x = range(len(keys))
    axes[0].bar(x, success, color="#00b7c2", label="Successful")
    axes[0].bar(x, failed, bottom=success, color="#d94841", label="Failed")
    axes[0].set(title="Executed order outcomes", ylabel="Transactions during measurement")
    axes[0].legend(frameon=False)
    axes[1].bar(x, fills, color="#6c5ce7")
    axes[1].set(title="Matched fills", ylabel="Fills during measurement")
    for axis in axes:
        axis.set_xticks(list(x), [MARKETS[key] for key in keys], rotation=35, ha="right")
    fig.suptitle(f"Six-market workload · {row['summary']['offered_tps']} additional scheduled TPS")
    save_graph(fig, out, "market-mix")
    plt.close(fig)


def profile_values(row):
    if row["run"] is None:
        return {}
    logs = list(row["run"].parent.rglob("node-0.stdout.log"))
    if not logs:
        return {}
    pattern = re.compile(r"PROFILE_STAGE node=0 stage=(\S+) count=(\d+) units=(\d+) "
                         r"total_us=(\d+) p50_us=(\d+) p95_us=(\d+) p99_us=(\d+)")
    result = {}
    for line in logs[0].read_text().splitlines():
        match = pattern.fullmatch(line)
        if match:
            stage, count, units, total, p50, p95, p99 = match.groups()
            result[stage] = {"count": int(count), "units": int(units),
                             "total": int(total), "p50": int(p50),
                             "p95": int(p95), "p99": int(p99)}
    return result


def stages(rows, out):
    selected = [row for row in rows if row["summary"]["nodes"] == 7 and
                row["summary"]["participants"] == 480 and
                row["summary"]["offered_tps"] == 500 and
                row["summary"]["result"] == "pass"]
    if not selected:
        return
    names = ["proposal_select", "proposal_execute", "proposal_state_root",
             "follower_auth", "follower_execute", "follower_state_root",
             "journal_encode", "journal_write", "journal_sync", "commit_projection"]
    fig, ax = plt.subplots(figsize=(9, 5.6), layout="constrained")
    width = 0.38
    for index, mode in enumerate(["all", "data"]):
        row = next((item for item in selected if item["mode"] == mode), None)
        if row is None:
            continue
        profile = profile_values(row)
        values = [profile.get(name, {}).get("p95", 0) / 1000 for name in names]
        positions = [n + (index - 0.5) * width for n in range(len(names))]
        ax.barh(positions, values, height=width, color=COLORS[mode], label=f"journal {mode}")
    ax.set_yticks(range(len(names)), [name.replace("_", " ") for name in names])
    ax.invert_yaxis()
    ax.set(xlabel="Per-call p95 (ms)", title="Where time goes inside validator 0")
    ax.legend(frameon=False)
    save_graph(fig, out, "stage-profile")
    plt.close(fig)


def backlog(rows, out):
    selected = [row for row in rows if row["summary"]["nodes"] == 7 and
                row["summary"]["participants"] == 480 and
                row["mode"] == "all" and row["summary"]["result"] == "pass" and row["run"]]
    if not selected:
        return
    fig, ax = plt.subplots(figsize=(9, 4.1), layout="constrained")
    for row in sorted(selected, key=lambda item: item["summary"]["offered_tps"]):
        run = json.loads(row["run"].read_text())
        x = [sample["elapsed_ms"] / 1000 for sample in run["samples"]]
        y = [max((node["mempool"] or 0 for node in sample["nodes"]), default=0)
             for sample in run["samples"]]
        ax.plot(x, y, label=f"{row['summary']['offered_tps']} scheduled TPS")
    ax.set(xlabel="Seconds into measurement", ylabel="Largest sampled node mempool",
           title="Backlog growth under six-market load")
    ax.legend(frameon=False)
    save_graph(fig, out, "backlog")
    plt.close(fig)


def memory_growth(rows, out):
    selected = [row for row in rows if row["summary"]["nodes"] == 7 and
                row["summary"]["participants"] == 0 and
                row["summary"]["algorithm"] == "hotstuff" and
                row["summary"]["offered_tps"] == 1800 and row["mode"] == "all" and
                row["run"] and
                ("3min" in str(row["path"]) or "repeat" in str(row["path"]))]
    if not selected:
        return
    fig, ax = plt.subplots(figsize=(9, 4.1), layout="constrained")
    for row in selected:
        run = json.loads(row["run"].read_text())
        x = [sample["elapsed_ms"] / 1000 for sample in run["samples"]]
        y = [max((node["rss_kib"] or 0 for node in sample["nodes"]), default=0) / 1024
             for sample in run["samples"]]
        label = "three-minute soak" if "3min" in str(row["path"]) else "one-minute repeat"
        ax.plot(x, y, label=label)
    ax.set(xlabel="Seconds into measurement", ylabel="Largest sampled validator RSS (MiB)",
           title="Resident memory under 1,800 scheduled TPS")
    ax.legend(frameon=False)
    save_graph(fig, out, "memory")
    plt.close(fig)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--results", type=Path,
                        default=Path("benchmark_results/performance-study-2026-10-19"))
    parser.add_argument("--out", type=Path,
                        default=Path("docs/figures/performance-study-2026-10-19"))
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    style()
    rows = list(records(args.results))
    capacity(rows, args.out)
    driver_control(rows, args.out)
    node_scaling(rows, args.out)
    market_mix(rows, args.out)
    stages(rows, args.out)
    backlog(rows, args.out)
    memory_growth(rows, args.out)
    print(f"Rendered figures from {len(rows)} runs to {args.out}")


if __name__ == "__main__":
    main()
