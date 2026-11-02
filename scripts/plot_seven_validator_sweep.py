"""Plot the real 3 ThinkPad + 2 IdeaPad + 2 Tokyo throughput trials."""

from __future__ import annotations

import json
from pathlib import Path

import matplotlib.pyplot as plt
import numpy as np


ROOT = Path(__file__).resolve().parents[1]
RESULTS = ROOT / "benchmark_results" / "hardware-2026-10-26"
OUT = ROOT / "docs" / "figures" / "seven-validator-sweep-2026-10-26.svg"
TRIALS = [
    ("50\nnode 0", "seven50"),
    ("75\nnode 0", "seven75_target_guard"),
    ("85\nnode 0", "seven85_target_guard"),
    ("85\nprimary", "seven85_primary"),
    ("80\nprimary\n2 min", "seven80_primary_120s"),
    ("100\nnode 0", "seven100_target_guard"),
    ("100\n250 ms batch", "seven100_batch250"),
]


def summary(directory: str) -> dict:
    paths = list((RESULTS / directory).glob("**/run.json"))
    if len(paths) != 1:
        raise RuntimeError(f"expected one run.json in {directory}, found {len(paths)}")
    return json.loads(paths[0].read_text())["summary"]


def main() -> None:
    rows = [(label, summary(directory)) for label, directory in TRIALS]
    plt.rcParams.update({"font.size": 10, "axes.spines.top": False, "axes.spines.right": False})
    fig, ax = plt.subplots(figsize=(11, 5.2))
    x = np.arange(len(rows))
    width = 0.26
    offered = [row["offered_tps"] for _, row in rows]
    delivered = [row["delivered_extra_tps"] for _, row in rows]
    committed = [row["successful_tps"] for _, row in rows]
    ax.bar(x - width, offered, width, label="Offered", color="#b9c0cb")
    ax.bar(x, delivered, width, label="Delivered", color="#4778bf")
    ax.bar(x + width, committed, width, label="Successful on chain", color="#319876")
    for index, (_, row) in enumerate(rows):
        label = "PASS" if row["result"] == "pass" else "INVALID"
        color = "#177654" if row["result"] == "pass" else "#b05b23"
        ax.text(index, max(offered[index], delivered[index], committed[index]) + 3,
                label, ha="center", va="bottom", color=color, fontsize=9, fontweight="bold")
    ax.set_xticks(x, [label for label, _ in rows])
    ax.set_ylim(0, 116)
    ax.set_ylabel("Transactions per second")
    ax.set_title("Seven validators across three computers")
    ax.grid(axis="y", color="#dce1e8", linewidth=0.7)
    ax.set_axisbelow(True)
    ax.legend(loc="upper left", ncol=3, frameon=False)
    fig.text(0.02, 0.01,
             "BTC/USD spot order flow; 4-block leader rotation. Successful TPS may include warmup commits."
             " Pass also requires delivery and replay checks.", fontsize=8, color="#4b5563")
    OUT.parent.mkdir(parents=True, exist_ok=True)
    fig.tight_layout(rect=(0, 0.04, 1, 1))
    fig.savefig(OUT)
    print(OUT)


if __name__ == "__main__":
    main()
