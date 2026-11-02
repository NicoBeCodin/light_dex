#!/usr/bin/env python3
"""Render the 2026-10-25 local batching/capacity trial figures from raw runs."""

import argparse
import json
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt


def trial(root: Path, name: str) -> dict:
    paths = sorted((root / name).glob("pilot-*/capacity*/run.json"))
    if len(paths) != 1:
        raise SystemExit(f"expected one run for {name}, found {len(paths)}")
    return json.loads(paths[0].read_text())


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path("benchmark_results/tx-pacing-2026-10-25"))
    parser.add_argument("--out", type=Path, default=Path("docs/assets"))
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    names = ["orderflow500", "orderflow750_clean", "orderflow850_clean",
             "orderflow900_clean", "orderflow1000_clean"]
    runs = [trial(args.root, name) for name in names]
    baseline = trial(args.root, "baseline32")
    batched = trial(args.root, "window50_32")
    six = trial(args.root, "balanced_six750")
    long_800 = trial(args.root, "orderflow800_60sec")
    long_900 = trial(args.root, "orderflow900_60sec")

    fig, axes = plt.subplots(1, 2, figsize=(12, 4.2), layout="constrained")
    axes[0].bar(["1 s timer", "50 ms batch"],
                [baseline["summary"]["end_to_end_p95_ms"], batched["summary"]["end_to_end_p95_ms"]],
                color=["#a3a3a3", "#22c55e"])
    axes[0].set(title="32 TPS transfer: p95 finality", ylabel="Milliseconds")
    axes[0].grid(axis="y", alpha=.2)
    axes[0].set_axisbelow(True)

    offered = [run["trial"]["offered_tps"] for run in runs]
    committed = [run["summary"]["successful_tps"] for run in runs]
    axes[1].plot(offered, committed, "o-", color="#2563eb", label="BTC spot, 25–30 s")
    axes[1].plot([800, 900], [long_800["summary"]["successful_tps"],
                                 long_900["summary"]["successful_tps"]],
                 "^", color="#7c3aed", markersize=8, label="BTC spot, 60 s")
    axes[1].plot([750], [six["summary"]["successful_tps"]], "s", color="#0d9488",
                 markersize=8, label="Six markets, 750 offered")
    axes[1].plot([500, 1000], [500, 1000], linestyle="--", color="#a3a3a3", label="Offered = committed")
    axes[1].set(title="Four local validators, pinned cores", xlabel="Offered transactions/s",
                ylabel="Committed transactions/s", xlim=(460, 1040), ylim=(450, 1050))
    axes[1].grid(alpha=.2)
    axes[1].legend(loc="upper left", fontsize=8)
    fig.savefig(args.out / "tx-pacing-capacity-2026-10-25.png", dpi=180)
    plt.close(fig)

    fig, ax = plt.subplots(figsize=(8.5, 4.2), layout="constrained")
    backlog_runs = [runs[1], long_800, long_900, runs[-1]]
    for run, label, color in zip(backlog_runs, ["750 for 30 s", "800 for 60 s", "900 for 60 s", "1000 for 30 s"],
                                 ["#0d9488", "#2563eb", "#9333ea", "#dc2626"]):
        samples = run["samples"]
        seconds = [sample["elapsed_ms"] / 1000 for sample in samples]
        lag = [sample["submitted"] - min(node["successful"] for node in sample["nodes"])
               for sample in samples]
        ax.plot(seconds, lag, label=f"{label} offered TPS", color=color, linewidth=1.8)
    ax.set(title="Submitted transactions awaiting the slowest validator", xlabel="Measurement time (s)",
           ylabel="Transactions", xlim=(0, 60))
    ax.grid(alpha=.2)
    ax.legend()
    fig.savefig(args.out / "tx-pacing-backlog-2026-10-25.png", dpi=180)
    plt.close(fig)


if __name__ == "__main__":
    main()
