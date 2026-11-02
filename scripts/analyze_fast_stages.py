#!/usr/bin/env python3
"""Summarize opt-in FAST_STAGE/HANDOFF markers from validator stderr logs.

Use LIGHT_DEX_HANDOFF_TRACE=1 when launching validators, then pass the run's
chains directory. Durations are computed only within one node, so they do not
require synchronized host clocks.
"""

import argparse
from collections import defaultdict
from pathlib import Path


def fields(line: str) -> dict[str, str]:
    return dict(token.split("=", 1) for token in line.split() if "=" in token)


def percentile(values: list[int], percent: int) -> int:
    ordered = sorted(values)
    return ordered[(len(ordered) - 1) * percent // 100]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("chain_dir", type=Path, help="directory containing node-*.stderr.log")
    args = parser.parse_args()

    blocks: dict[tuple[int, int, str], dict[str, int]] = defaultdict(dict)
    durable: dict[tuple[int, int], int] = {}
    handoffs: dict[tuple[int, int], dict[str, int]] = defaultdict(dict)
    proposals_by_view: dict[tuple[int, int], int] = {}
    for path in sorted(args.chain_dir.glob("node-*.stderr.log")):
        for line in path.read_text(errors="replace").splitlines():
            data = fields(line)
            if line.startswith("FAST_STAGE "):
                try:
                    key = (int(data["node"]), int(data["height"]), data["digest"])
                    blocks[key].setdefault(data["stage"], int(data["unix_us"]))
                    if data["stage"] == "proposal_sent":
                        proposals_by_view.setdefault((int(data["node"]), int(data["view"])), int(data["unix_us"]))
                except (KeyError, ValueError):
                    continue
            elif line.startswith("HANDOFF ") and data.get("event") == "durable_commit":
                try:
                    durable[(int(data["node"]), int(data["sequence"]))] = int(data["unix_us"])
                except (KeyError, ValueError):
                    continue
            elif line.startswith("HANDOFF ") and data.get("event") in {
                    "new_view_sent", "new_view_quorum", "start_view_sent"}:
                try:
                    handoffs[(int(data["node"]), int(data["view"]))].setdefault(
                        data["event"], int(data["unix_us"]))
                except (KeyError, ValueError):
                    continue

    intervals = [
        ("leader proposal to Prepare QC", "proposal_sent", "prepare_qc_formed"),
        ("leader Prepare QC to Final QC", "prepare_qc_formed", "final_qc_formed"),
        ("leader proposal to Final QC", "proposal_sent", "final_qc_formed"),
        ("replica proposal to commit decision", "proposal_received", "commit_decided"),
    ]
    for label, start, end in intervals:
        durations = [(marks[end] - marks[start]) // 1000 for marks in blocks.values()
                     if start in marks and end in marks and marks[end] >= marks[start]]
        if durations:
            print(f"{label}: n={len(durations)} p50={percentile(durations, 50)} ms "
                  f"p95={percentile(durations, 95)} ms")

    leaders: dict[int, list[int]] = defaultdict(list)
    for (node, _, _), marks in blocks.items():
        if "proposal_sent" in marks and "final_qc_formed" in marks:
            elapsed = marks["final_qc_formed"] - marks["proposal_sent"]
            if elapsed >= 0:
                leaders[node].append(elapsed // 1000)
    for node, durations in sorted(leaders.items()):
        print(f"leader node {node} proposal to Final QC: n={len(durations)} "
              f"p50={percentile(durations, 50)} ms "
              f"p95={percentile(durations, 95)} ms")

    for label, start, end in [
        ("leader NewView to quorum", "new_view_sent", "new_view_quorum"),
        ("leader quorum to StartView", "new_view_quorum", "start_view_sent"),
    ]:
        durations = [(marks[end] - marks[start]) // 1000 for marks in handoffs.values()
                     if start in marks and end in marks and marks[end] >= marks[start]]
        if durations:
            print(f"{label}: n={len(durations)} p50={percentile(durations, 50)} ms "
                  f"p95={percentile(durations, 95)} ms")
    to_proposal = [(sent - marks["new_view_sent"]) // 1000
                   for key, marks in handoffs.items()
                   if "new_view_sent" in marks
                   for sent in [proposals_by_view.get(key)]
                   if sent is not None and sent >= marks["new_view_sent"]]
    if to_proposal:
        print(f"leader NewView to first proposal: n={len(to_proposal)} "
              f"p50={percentile(to_proposal, 50)} ms "
              f"p95={percentile(to_proposal, 95)} ms")

    commit_to_durable = [(durable[(node, height)] - marks["commit_decided"]) // 1000
                         for (node, height, _), marks in blocks.items()
                         if "commit_decided" in marks and (node, height) in durable
                         and durable[(node, height)] >= marks["commit_decided"]]
    if commit_to_durable:
        print(f"commit decision to durable: n={len(commit_to_durable)} "
              f"p50={percentile(commit_to_durable, 50)} ms "
              f"p95={percentile(commit_to_durable, 95)} ms")
    if not blocks:
        raise SystemExit("no FAST_STAGE records found; enable LIGHT_DEX_HANDOFF_TRACE=1")


if __name__ == "__main__":
    main()
