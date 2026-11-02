#!/usr/bin/env python3
"""Print scheduled-submission to durable-commit latency per minute for a capacity trial."""

import argparse
from collections import defaultdict
import json
from pathlib import Path


def percentile(values: list[float], percent: int) -> float:
    ordered = sorted(values)
    return ordered[(len(ordered) - 1) * percent // 100]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("trial_dir", type=Path, help="directory with driver-submissions.json and chains/")
    args = parser.parse_args()

    submissions = json.loads((args.trial_dir / "driver-submissions.json").read_text())
    if not submissions:
        raise SystemExit("no driver submissions")
    schedule = {row["digest"]: row["scheduled_unix_us"] for row in submissions}
    start = min(schedule.values())
    buckets: dict[int, list[float]] = defaultdict(list)
    seen: set[str] = set()
    traces = sorted((args.trial_dir / "chains").glob("run-*/node-0.latency.jsonl"))
    if len(traces) != 1:
        raise SystemExit(f"expected one node-0 commit trace, found {len(traces)}")
    with traces[0].open() as trace:
        for line in trace:
            block = json.loads(line)
            for digest in block["successful"]:
                if digest not in schedule or digest in seen:
                    continue
                seen.add(digest)
                scheduled = schedule[digest]
                minute = (scheduled - start) // 60_000_000 + 1
                buckets[minute].append((block["committed_unix_us"] - scheduled) / 1_000)

    print("minute\tmatched\tp50_ms\tp95_ms")
    for minute, values in sorted(buckets.items()):
        print(f"{minute}\t{len(values)}\t{percentile(values, 50):.0f}\t{percentile(values, 95):.0f}")
    unmatched = len(schedule) - len(seen)
    print(f"unmatched\t{unmatched}")


if __name__ == "__main__":
    main()
