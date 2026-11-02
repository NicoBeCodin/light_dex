"""Plot the matched seven-validator handoff trials from their raw run.json files."""

from __future__ import annotations

import json
from pathlib import Path

import matplotlib.pyplot as plt
import numpy as np


ROOT = Path(__file__).resolve().parents[1]
RUNS = ROOT / "benchmark_results" / "hardware-2026-10-26"
OUT = ROOT / "docs" / "figures" / "seven-validator-handoff-2026-10-26.svg"
PAIRS = [
    ("80 TPS, 120 s", "seven80_primary_120s", "seven80_branch8_120s"),
    ("85 TPS, 60 s", "seven85_primary", "seven85_branch8"),
]


def load(name: str) -> tuple[dict, dict]:
    paths = list((RUNS / name).glob("**/run.json"))
    if len(paths) != 1:
        raise RuntimeError(f"expected one run.json for {name}, found {len(paths)}")
    record = json.loads(paths[0].read_text())
    return record["summary"], record["samples"][-1]


def main() -> None:
    labels = []
    old_misses = []
    new_misses = []
    old_delivery = []
    new_delivery = []
    old_pass = []
    new_pass = []
    for label, before, after in PAIRS:
        labels.append(label)
        old_summary, old_sample = load(before)
        new_summary, new_sample = load(after)
        old_misses.append(sum(node.get("proposal_parent_misses") or 0 for node in old_sample["nodes"]))
        new_misses.append(sum(node.get("proposal_parent_misses") or 0 for node in new_sample["nodes"]))
        old_delivery.append(old_summary["delivered_extra_tps"])
        new_delivery.append(new_summary["delivered_extra_tps"])
        old_pass.append(old_summary["result"] == "pass")
        new_pass.append(new_summary["result"] == "pass")

    plt.rcParams.update({"font.size": 10, "axes.spines.top": False, "axes.spines.right": False})
    fig, axes = plt.subplots(1, 2, figsize=(10, 4.3))
    x = np.arange(len(labels))
    width = 0.3
    old_color = "#a9b2bf"
    new_color = "#2b9475"
    axes[0].bar(x - width / 2, old_misses, width, color=old_color, label="3 proposals")
    axes[0].bar(x + width / 2, new_misses, width, color=new_color, label="8 proposals")
    axes[0].set_ylabel("Missing speculative parents")
    axes[0].set_title("Availability during handoff")
    axes[0].set_ylim(0, max(old_misses) * 1.2)
    axes[1].bar(x - width / 2, old_delivery, width, color=old_color)
    axes[1].bar(x + width / 2, new_delivery, width, color=new_color)
    axes[1].set_ylabel("Delivered transactions per second")
    axes[1].set_title("Driver admission")
    axes[1].set_ylim(0, 100)
    for index, (passed_before, passed_after) in enumerate(zip(old_pass, new_pass)):
        for offset, delivered, passed in [(-width / 2, old_delivery[index], passed_before),
                                          (width / 2, new_delivery[index], passed_after)]:
            axes[1].text(index + offset, delivered + 2, "pass" if passed else "invalid",
                         ha="center", fontsize=8, color="#256d55" if passed else "#a85524")
    for ax in axes:
        ax.set_xticks(x, labels)
        ax.grid(axis="y", color="#e0e5eb", linewidth=0.7)
        ax.set_axisbelow(True)
    axes[0].legend(frameon=False, loc="upper right")
    fig.suptitle("Chained HotStuff, 3 ThinkPad + 2 IdeaPad + 2 Tokyo validators")
    fig.tight_layout()
    OUT.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(OUT)
    print(OUT)


if __name__ == "__main__":
    main()
