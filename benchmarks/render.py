#!/usr/bin/env python3
"""Render article-ready SVG figures and a compact CSV from bench run JSON."""
import argparse
import csv
import json
import re
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt


COLORS = {"pbft": "#2563eb", "hotstuff": "#e76f51"}


def load_summary(path: Path):
    rows = json.loads(path.read_text())
    if not isinstance(rows, list):
        raise ValueError("summary.json must contain a list")
    return rows


def save_plot(fig, output: Path, name: str):
    fig.tight_layout()
    fig.savefig(output / name, format="svg", bbox_inches="tight")
    plt.close(fig)


def style(ax, title, xlabel, ylabel):
    ax.set_title(title, fontweight="bold")
    ax.set_xlabel(xlabel)
    ax.set_ylabel(ylabel)
    ax.grid(alpha=0.2)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--summary", type=Path, required=True)
    parser.add_argument("--supplement", type=Path, action="append", default=[],
                        help="Later summary overrides trials with the same name (for corrected reruns)")
    parser.add_argument("--micro", type=Path)
    parser.add_argument("--perf-report", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    rows_by_name = {}
    source_by_name = {}
    for summary_file in [args.summary, *args.supplement]:
        for row in load_summary(summary_file):
            rows_by_name[row["name"]] = row
            source_by_name[row["name"]] = str(summary_file)
    rows = list(rows_by_name.values())
    if not rows:
        raise ValueError("no benchmark trials in summary")
    fields = list(dict.fromkeys(key for row in rows for key in row)) + ["source_summary"]
    with (args.out / "summary.csv").open("w", newline="") as file:
        writer = csv.DictWriter(file, fieldnames=fields)
        writer.writeheader()
        writer.writerows({**row, "source_summary": source_by_name[row["name"]]} for row in rows)
    valid = [row for row in rows if row["result"] == "pass"]

    scale = [row for row in valid if row["name"].startswith("scale_")]
    if scale:
        fig, axes = plt.subplots(1, 2, figsize=(10, 3.8))
        for algorithm in COLORS:
            subset = sorted((row for row in scale if row["algorithm"] == algorithm), key=lambda row: row["nodes"])
            if not subset:
                continue
            axes[0].plot([row["nodes"] for row in subset], [row["successful_tps"] for row in subset],
                         "o-", label=algorithm.upper(), color=COLORS[algorithm])
            axes[1].plot([row["nodes"] for row in subset], [row["latency_p95_ms"] for row in subset],
                         "o-", label=algorithm.upper(), color=COLORS[algorithm])
        style(axes[0], "Live agents: node scaling", "Nodes", "Successful TPS")
        style(axes[1], "Node receipt to commit", "Nodes", "Sampled p95 latency (ms)")
        axes[0].legend()
        save_plot(fig, args.out, "node_scaling.svg")

    timeout = [row for row in rows if row["name"].startswith("timeout_")]
    if timeout:
        fig, axes = plt.subplots(1, 2, figsize=(10, 3.8))
        for algorithm in COLORS:
            subset = sorted((row for row in timeout if row["algorithm"] == algorithm and row["result"] == "pass"),
                            key=lambda row: row["timeout_ms"])
            if not subset:
                continue
            axes[0].plot([row["timeout_ms"] for row in subset], [row["successful_tps"] for row in subset],
                         "o-", label=algorithm.upper(), color=COLORS[algorithm])
            axes[1].plot([row["timeout_ms"] for row in subset], [row["view_changes"] for row in subset],
                         "o-", label=algorithm.upper(), color=COLORS[algorithm])
        for row in timeout:
            if row["result"] != "pass":
                axes[0].scatter(row["timeout_ms"], 0, marker="x", s=90, color=COLORS[row["algorithm"]])
        style(axes[0], "Timeout pilot (× = invalid)", "Timeout (ms)", "Successful TPS")
        style(axes[1], "View churn", "Timeout (ms)", "Sum of node view advances")
        axes[0].legend()
        save_plot(fig, args.out, "timeout_floor.svg")

    stress = [row for row in valid if row["name"].startswith("stress_")]
    if stress:
        fig, axes = plt.subplots(1, 3, figsize=(14, 3.8))
        for algorithm in COLORS:
            subset = sorted((row for row in stress if row["algorithm"] == algorithm), key=lambda row: row["offered_tps"])
            if subset:
                actual = [row["attempted_extra"] / row["measured_seconds"] for row in subset]
                axes[0].plot([row["offered_tps"] for row in subset], actual,
                             "o-", label=algorithm.upper(), color=COLORS[algorithm])
                axes[1].plot(actual, [row["successful_tps"] for row in subset],
                             "o-", label=f"{algorithm.upper()} successful", color=COLORS[algorithm])
                axes[1].plot(actual, [row["successful_tps"] + row["failed_tps"] for row in subset],
                             "o--", label=f"{algorithm.upper()} included", color=COLORS[algorithm], alpha=0.65)
                axes[2].plot(actual, [row["max_backlog"] for row in subset],
                             "o-", label=algorithm.upper(), color=COLORS[algorithm])
        style(axes[0], "Driver delivery", "Extra TPS target", "Actual extra attempts/s")
        style(axes[1], "Load with 13 live agents", "Actual extra attempts/s", "Included transactions/s")
        style(axes[2], "Queue pressure", "Actual extra attempts/s", "Max sampled mempool")
        axes[0].legend()
        axes[1].legend(fontsize=7)
        save_plot(fig, args.out, "capacity_curve.svg")

    if args.micro and args.micro.exists():
        with args.micro.open(newline="") as file:
            micro = list(csv.DictReader(file))
        fig, ax = plt.subplots(figsize=(7, 4.2))
        for operation in sorted({row["operation"] for row in micro}):
            subset = sorted((row for row in micro if row["operation"] == operation), key=lambda row: int(row["book_orders"]))
            ax.plot([int(row["book_orders"]) for row in subset], [int(row["p95_ns"]) / 1_000_000 for row in subset],
                    "o-", label=operation)
        style(ax, "Exchange operation cost", "Resting orders in fixture", "p95 operation time (ms)")
        ax.legend(fontsize=8)
        save_plot(fig, args.out, "exchange_cost.svg")

    if args.perf_report and args.perf_report.exists():
        symbols = []
        for line in args.perf_report.read_text().splitlines():
            match = re.match(r"\s*([\d.]+)%\s+.*?\[\.\]\s+(.+)$", line)
            if match and len(symbols) < 12:
                symbols.append((match.group(2).strip()[:70], float(match.group(1))))
        if symbols:
            fig, ax = plt.subplots(figsize=(8, max(3, len(symbols) * 0.35)))
            ax.barh([name for name, _ in symbols[::-1]], [share for _, share in symbols[::-1]], color="#64748b")
            style(ax, "Exchange microbench CPU samples", "Self sample share (%)", "Function")
            save_plot(fig, args.out, "hot_functions.svg")

    trial_dirs = sorted(args.summary.parent.glob("scale_*/run.json"))
    if trial_dirs:
        run = json.loads(trial_dirs[0].read_text())
        times, heights, backlogs = [], [], []
        for point in run["samples"]:
            nodes = [node for node in point["nodes"] if node["height"] is not None]
            if nodes:
                times.append(point["elapsed_ms"] / 1000)
                heights.append(max(node["height"] for node in nodes))
                backlogs.append(max((node["mempool"] or 0) for node in nodes))
        if times:
            fig, ax = plt.subplots(figsize=(7, 4))
            ax.plot(times, heights, color="#2563eb", label="Best chain height")
            other = ax.twinx()
            other.plot(times, backlogs, color="#e76f51", alpha=0.8, label="Max mempool")
            style(ax, "Live run progression", "Measurement time (s)", "Chain height")
            other.set_ylabel("Max mempool size")
            save_plot(fig, args.out, "height_and_backlog.svg")

    (args.out / "figure_notes.md").write_text(
        "Figures use only trials marked `pass`; × marks invalid timeout trials. "
        "PBFT/HotStuff live runs had different Binance data and stochastic agent choices, "
        "so ratios are observational. Latency samples are node receipt-to-commit, not client end-to-end. "
        "The optional hotspot chart profiles the exchange microbench only, not a cluster node. "
        "See summary.csv for the source run of each plotted trial.\n"
    )
    print(f"Rendered {len(list(args.out.glob('*.svg')))} SVG figures in {args.out}")


if __name__ == "__main__":
    main()
