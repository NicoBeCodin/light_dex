#!/usr/bin/env python3
"""Group exact benchmark commit latency by the block's proposing leader.

Usage: python3 scripts/analyze_handoff_latency.py path/to/run.json
The benchmark must have run with LIGHT_DEX_COMMIT_TRACE=1.
"""

import csv
import json
import math
import sys
from collections import defaultdict
from pathlib import Path


def percentile(values: list[int], fraction: float) -> int | None:
    if not values:
        return None
    ordered = sorted(values)
    return ordered[math.ceil((len(ordered) - 1) * fraction)]


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: analyze_handoff_latency.py RUN_JSON")
    run_path = Path(sys.argv[1]).resolve()
    run = json.loads(run_path.read_text())
    data_dir = Path(run["summary"]["data_dir"])
    submissions = {
        row["digest"]: row["scheduled_unix_us"]
        for row in json.loads((run_path.parent / "driver-submissions.json").read_text())
    }
    views: dict[int, int] = {}
    with (data_dir / "node-0.json.journal").open() as journal:
        for line in journal:
            record = json.loads(line)
            certificate = record["certificate"]
            if "Chained" not in certificate:
                raise SystemExit("this report currently requires chained HotStuff")
            views[record["block"]["sequence"]] = certificate["Chained"]["qcs"][0]["view"]

    by_leader: dict[int, list[int]] = defaultdict(list)
    matched: set[str] = set()
    with (data_dir / "node-0.latency.jsonl").open() as trace:
        for line in trace:
            record = json.loads(line)
            view = views.get(record["sequence"])
            if view is None:
                continue
            leader = view % run["summary"]["nodes"]
            for digest in record["successful"]:
                scheduled = submissions.get(digest)
                if scheduled is None or digest in matched:
                    continue
                latency_us = record["committed_unix_us"] - scheduled
                if latency_us < 0:
                    raise SystemExit("driver and validator 0 clocks disagree")
                matched.add(digest)
                by_leader[leader].append(latency_us // 1000)

    labels = {
        node["id"]: node["host_label"]
        for sample in run["samples"][:1]
        for node in sample["nodes"]
    }
    writer = csv.writer(sys.stdout, lineterminator="\n")
    writer.writerow(["leader", "host", "matched_transactions", "scheduled_p50_ms", "scheduled_p95_ms"])
    for leader in range(run["summary"]["nodes"]):
        values = by_leader[leader]
        writer.writerow([leader, labels.get(leader, ""), len(values),
                         percentile(values, 0.50), percentile(values, 0.95)])
    print(f"matched={len(matched)} unmatched={len(submissions) - len(matched)}", file=sys.stderr)


if __name__ == "__main__":
    main()
