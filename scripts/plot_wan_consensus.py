#!/usr/bin/env python3
"""Render the reproducible WAN latency figures from docs/data CSV files."""

import csv
from pathlib import Path

import matplotlib.pyplot as plt


ROOT = Path(__file__).resolve().parents[1]
DATA = ROOT / "docs/data"
FIGURES = ROOT / "docs/figures"
FIGURES.mkdir(parents=True, exist_ok=True)


def rows(name: str) -> list[dict]:
    with (DATA / name).open(newline="") as source:
        return list(csv.DictReader(source))


summary = rows("wan_consensus_2026_10_29.csv")
phase = rows("wan_consensus_phases_2026_10_29.csv")
topologies = ["2 IdeaPad + 2 Tokyo", "3 ThinkPad + 2 IdeaPad + 2 Tokyo"]

fig, ax = plt.subplots(figsize=(8.5, 4.6), layout="constrained")
colors = {"leader-only": "#586879", "vote-gossip": "#008a73"}
offsets = {"leader-only": -0.13, "vote-gossip": 0.13}
for topology_index, topology in enumerate(topologies):
    for row in summary:
        if row["topology"] != topology or row["offered_tps"] != "200":
            continue
        x = topology_index + offsets[row["collector"]]
        y = int(row["end_to_end_p95_ms"])
        ax.scatter(x, y, s=68, color=colors[row["collector"]], zorder=3)
        ax.annotate(f"{y:,}", (x, y), xytext=(6, 5),
                    textcoords="offset points", fontsize=9)
ax.axhline(1500, color="#b05b37", linestyle="--", linewidth=1.2)
ax.text(1.43, 1525, "1.5 s reference", color="#b05b37", fontsize=8,
        ha="right")
ax.set_xticks([0, 1], ["2 IdeaPad + 2 Tokyo\nremote vote required",
                       "3 ThinkPad + 2 IdeaPad + 2 Tokyo\nlocal quorum possible"])
ax.set_xlim(-0.43, 1.48)
ax.set_ylim(650, 2750)
ax.set_ylabel("End-to-end p95 (ms)")
ax.set_title("200 offered transactions per second")
ax.grid(axis="y", alpha=0.25)
for label, color in [("Leader-only HotStuff2", colors["leader-only"]),
                     ("Vote-gossip HotStuff2", colors["vote-gossip"])]:
    ax.scatter([], [], color=color, s=68, label=label)
ax.legend(loc="upper right", frameon=False, fontsize=9)
fig.savefig(FIGURES / "wan_consensus_p95_2026_10_29.svg")
fig.savefig("/tmp/wan_consensus_p95_preview.png", dpi=150)
plt.close(fig)

fig, ax = plt.subplots(figsize=(10.5, 4.6), layout="constrained")
names = ["4 nodes\nleader-only", "4 nodes\nvote gossip",
         "7 nodes\nleader-only", "7 nodes\nvote gossip"]
first = [float(row["proposal_to_prepare_p50_ms"]) for row in phase]
second = [float(row["prepare_to_final_p50_ms"]) for row in phase]
positions = list(range(4))
ax.bar(positions, first, color="#60758b", label="Proposal to Prepare QC")
ax.bar(positions, second, bottom=first, color="#62b29d",
       label="Prepare QC to Final QC")
for index in positions:
    total = first[index] + second[index]
    ax.text(index, total + 15, f"{total:.0f} ms", ha="center", fontsize=9)
ax.set_xticks(positions, names)
ax.set_ylim(0, 1170)
ax.set_ylabel("Tokyo leader phase time (ms)")
ax.set_title("Median measured on each leader's own clock")
ax.grid(axis="y", alpha=0.25)
ax.set_axisbelow(True)
ax.legend(loc="upper right", frameon=False, fontsize=9)
fig.savefig(FIGURES / "wan_consensus_phases_2026_10_29.svg")
fig.savefig("/tmp/wan_consensus_phases_preview.png", dpi=150)
plt.close(fig)
