#!/usr/bin/env python3
"""Summarize sampled validator transaction stages from one local benchmark trial.

Usage: python3 scripts/analyze_tx_stages.py PATH/TO/capacity_trial [--out report.json]
All validators and the driver must share the same host clock for cross-node
stage durations. The script does not compare clocks across remote machines.
"""

import argparse
import json
import math
from collections import defaultdict
from pathlib import Path


def percentile(values: list[float], pct: float) -> float | None:
    if not values:
        return None
    values = sorted(values)
    return round(values[min(len(values) - 1, math.ceil((len(values) - 1) * pct))], 3)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("trial", type=Path)
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()

    submissions = {
        row["digest"]: row
        for row in json.loads((args.trial / "driver-submissions.json").read_text())
    }
    events: dict[str, list[dict]] = defaultdict(list)
    paths = sorted((args.trial / "chains").glob("run-*/node-*.stages.jsonl"))
    if not paths:
        parser.error("no sampled stage files found; set LIGHT_DEX_TX_STAGE_SAMPLE_MOD")
    for path in paths:
        for line in path.read_text().splitlines():
            row = json.loads(line)
            if row["digest"] in submissions:
                events[row["digest"]].append(row)

    segment_names = [
        ("scheduled_to_submitted", "scheduled", "submitted"),
        ("scheduled_to_admitted", "scheduled", "admitted"),
        ("submitted_to_admitted", "submitted", "admitted"),
        ("admitted_to_proposed", "admitted", "proposed"),
        ("proposed_to_certified", "proposed", "certified"),
        ("certified_to_decided", "certified", "commit_decided"),
        ("decided_to_durable", "commit_decided", "durable_commit"),
        ("submitted_to_durable", "submitted", "durable_commit"),
        ("scheduled_to_durable", "scheduled", "durable_commit"),
    ]
    durations: dict[str, list[float]] = defaultdict(list)
    complete = 0
    invalid_order = 0
    for digest, rows in events.items():
        durable = [row for row in rows if row["stage"] == "durable_commit" and row["node_id"] == 0]
        if not durable:
            continue
        final = min(durable, key=lambda row: row["unix_us"])
        stages: dict[str, int] = {
            "scheduled": submissions[digest]["scheduled_unix_us"],
            "submitted": submissions[digest]["submitted_unix_us"],
            "durable_commit": final["unix_us"],
        }
        proposal_rows = [row for row in rows if row["stage"] == "proposed"
                         and row["block_digest"] == final["block_digest"]
                         and row["unix_us"] <= final["unix_us"]]
        proposer = min(proposal_rows, key=lambda row: row["unix_us"]) if proposal_rows else None
        for stage in ("admitted", "proposed", "certified", "commit_decided"):
            candidates = [row["unix_us"] for row in rows if row["stage"] == stage
                          and row["unix_us"] <= final["unix_us"]
                          and (stage != "admitted" or proposer is None
                               or row["node_id"] == proposer["node_id"])
                          and (stage == "admitted"
                               or row["block_digest"] == final["block_digest"])]
            if candidates:
                stages[stage] = min(candidates)
        if all(stage in stages for stage in ("admitted", "proposed", "certified", "commit_decided")):
            complete += 1
        for name, start, end in segment_names:
            if start not in stages or end not in stages:
                continue
            delta = stages[end] - stages[start]
            if delta < 0:
                invalid_order += 1
                continue
            durations[name].append(delta / 1000)

    result = {
        "trial": str(args.trial),
        "clock_scope": "one host only",
        "sampled_digests_with_events": len(events),
        "complete_stage_paths": complete,
        "negative_stage_intervals": invalid_order,
        "segments_ms": {
            name: {"count": len(values), "p50": percentile(values, .50),
                   "p95": percentile(values, .95), "p99": percentile(values, .99)}
            for name, _, _ in segment_names
            for values in [durations[name]]
        },
    }
    payload = json.dumps(result, indent=2)
    if args.out:
        args.out.write_text(payload + "\n")
    print(payload)


if __name__ == "__main__":
    main()
