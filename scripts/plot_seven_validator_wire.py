"""Plot the controlled seven-validator wire-format comparison.

Run from the repository root: python3 scripts/plot_seven_validator_wire.py
"""

import csv
from pathlib import Path

import matplotlib.pyplot as plt


ROOT = Path(__file__).resolve().parents[1]
RESULTS = ROOT / "docs/seven-validator-wire-results-2026-10-27.csv"
OUTPUT = ROOT / "docs/figures/seven-validator-wire-2026-10-27.svg"

with RESULTS.open(newline="") as file:
    rows = {row["run"]: row for row in csv.DictReader(file)}

before = rows["uncompressed-control-200"]
after = rows["compressed-control-200"]
labels = ["Uncompressed", "Compressed"]
colors = ["#718096", "#176b9a"]

fig, axes = plt.subplots(1, 3, figsize=(11, 3.4), layout="constrained")
metrics = [
    ("delivered_tps", "Delivered TPS", 220, "{:.0f}"),
    ("observed_confirmation_p95_ms", "Confirmation p95 (s)", 12, "{:.1f}"),
    ("max_backlog", "Peak node mempool", 1900, "{:.0f}"),
]

for ax, (field, title, ceiling, display) in zip(axes, metrics):
    values = [float(before[field]), float(after[field])]
    if field == "observed_confirmation_p95_ms":
        values = [value / 1000 for value in values]
    bars = ax.bar(labels, values, color=colors, width=0.58)
    ax.set_title(title, fontsize=11)
    ax.set_ylim(0, ceiling)
    ax.spines[["top", "right"]].set_visible(False)
    ax.grid(axis="y", alpha=0.2)
    ax.set_axisbelow(True)
    for bar, value in zip(bars, values):
        ax.text(bar.get_x() + bar.get_width() / 2, value + ceiling * 0.025,
                display.format(value), ha="center", fontsize=10)
    if field == "delivered_tps":
        ax.axhline(200, color="#b45309", linewidth=1, linestyle="--")
        ax.text(0.02, 0.98, "200 TPS offered", transform=ax.transAxes,
                va="top", fontsize=8, color="#92400e")

fig.suptitle("Same 7 validators and node binary, 60 s at 200 offered TPS", fontsize=12)
OUTPUT.parent.mkdir(parents=True, exist_ok=True)
fig.savefig(OUTPUT)
print(OUTPUT)
