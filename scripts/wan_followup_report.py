#!/usr/bin/env python3
"""Regenerate the curated cross-region A/B table and latency figure.

Input is the ignored, log-only rsync copy of the IdeaPad benchmark runs under
benchmark_results/wan-consensus-2026-10-29/ab_*/remote-copy. The script reads
no validator keys and never compares timestamps from different machines.
"""

import csv
import json
import re
import statistics
from pathlib import Path

import matplotlib.pyplot as plt

ROOT = Path(__file__).resolve().parents[1]
RUNS = (
    ("ab_baseline", "Baseline A", False, False),
    ("ab_handoff", "Handoff", True, False),
    ("ab_compact", "Compact", False, True),
    ("ab_both", "Both A", True, True),
    ("ab_baseline_repeat", "Baseline B", False, False),
    ("ab_both_notice", "Both + recovery", True, True),
    ("ab_both_final", "Both + retry", True, True),
)


def percentile(values, percent):
    if not values:
        return None
    values = sorted(values)
    return round(values[(len(values) - 1) * percent // 100], 3)


def fields(line):
    return dict(part.split("=", 1) for part in line.split() if "=" in part)


def collect(name, label, handoff, compact):
    copy = ROOT / "benchmark_results/wan-consensus-2026-10-29" / name / "remote-copy"
    passing = [(path, json.loads(path.read_text())[0])
               for path in copy.glob("pilot-*/summary.json")]
    passing = [(path, summary) for path, summary in passing if summary["result"] == "pass"]
    if len(passing) != 1:
        raise RuntimeError(f"expected one passing summary under {copy}, got {len(passing)}")
    summary_path, summary = passing[0]
    logs = list(summary_path.parent.glob("capacity_*/chains/run-*/node-*.stderr.log"))
    stdout_logs = list(summary_path.parent.glob("capacity_*/chains/run-*/node-*.stdout.log"))
    if len(logs) != 4:
        raise RuntimeError(f"{name}: expected four validator stderr logs, got {len(logs)}")

    sends = []
    handoffs = {}
    rotations = 0
    body_requests = 0
    for path in logs:
        for line in path.read_text(errors="replace").splitlines():
            if line.startswith("FAST_SEND "):
                data = fields(line)
                sends.append((int(data["txs"]), int(data["bytes"]), data.get("compact") == "true"))
            elif line.startswith("HANDOFF "):
                data = fields(line)
                if data.get("event") == "certified_rotation":
                    rotations += 1
                elif data.get("event") in ("new_view_sent", "new_view_quorum"):
                    if "unix_us" in data:
                        handoffs.setdefault((data["node"], data["view"]), {}).setdefault(
                            data["event"], int(data["unix_us"]))
    for path in stdout_logs:
        for line in path.read_text(errors="replace").splitlines():
            if "PROFILE compact_received=" in line:
                match = re.search(r"compact_body_requests=(\d+)", line)
                if match:
                    body_requests += int(match.group(1))
    waits = [(events["new_view_quorum"] - events["new_view_sent"]) / 1000
             for events in handoffs.values()
             if "new_view_quorum" in events and "new_view_sent" in events]
    full_64_127 = [size for txs, size, is_compact in sends
                   if 64 <= txs <= 127 and not is_compact]
    compact_64_127 = [size for txs, size, is_compact in sends
                      if 64 <= txs <= 127 and is_compact]
    return {
        "run": name, "label": label, "certificate_handoff": handoff,
        "compact_proposals": compact,
        "committed_tps": round(summary["successful_tps"], 2),
        "end_to_end_p50_ms": summary["end_to_end_p50_ms"],
        "end_to_end_p95_ms": summary["end_to_end_p95_ms"],
        "final_consistency": summary["final_consistency"],
        "height_min": summary["height_min"], "height_max": summary["height_max"],
        "catch_up_events": summary["catch_up_events"],
        "pending_at_end": summary["pending_at_end"],
        "max_backlog": summary["max_backlog"],
        "compact_sends": sum(is_compact for _, _, is_compact in sends),
        "full_sends": sum(not is_compact for _, _, is_compact in sends),
        "full_64_127_bytes_p50": percentile(full_64_127, 50),
        "compact_64_127_bytes_p50": percentile(compact_64_127, 50),
        "compact_body_requests": body_requests,
        "certified_rotations": rotations,
        "new_view_wait_p50_ms": percentile(waits, 50),
        "new_view_wait_p95_ms": percentile(waits, 95),
    }


def main():
    rows = [collect(*run) for run in RUNS]
    output = ROOT / "docs/data/wan_consensus_followup_2026_10_29.csv"
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("w", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)

    fig, ax = plt.subplots(figsize=(11.5, 4.6))
    colors = ["#778899" if not row["certificate_handoff"] and not row["compact_proposals"]
              else "#3477a4" if row["certificate_handoff"] and not row["compact_proposals"]
              else "#159376" if row["compact_proposals"] and not row["certificate_handoff"]
              else "#7654a3" for row in rows]
    bars = ax.bar(range(len(rows)), [row["end_to_end_p95_ms"] for row in rows], color=colors)
    ax.bar_label(bars, labels=[f'{row["end_to_end_p95_ms"] / 1000:.2f}s' for row in rows], padding=3)
    ax.set_xticks(range(len(rows)), [row["label"] for row in rows])
    ax.set_ylim(0, max(row["end_to_end_p95_ms"] for row in rows) * 1.2)
    ax.set_ylabel("End-to-end p95 (ms)")
    ax.set_title("2 IdeaPad + 2 Tokyo validators, 200 offered order TPS")
    ax.grid(axis="y", alpha=0.2)
    ax.set_axisbelow(True)
    fig.text(0.01, 0.01, "Sequential live WAN runs; the two baselines show network variability.",
             fontsize=9, color="#555555")
    fig.tight_layout(rect=(0, 0.05, 1, 1))
    figure = ROOT / "docs/figures/wan_consensus_followup_2026_10_29.svg"
    figure.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(figure)
    fig.savefig(figure.with_suffix(".png"), dpi=160)
    plt.close(fig)
    print(output)
    print(figure)


if __name__ == "__main__":
    main()
