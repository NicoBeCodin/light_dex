#!/usr/bin/env python3
"""Measure HotStuff2/Votor vote rounds on each leader's own clock.

Validators must be launched with LIGHT_DEX_HANDOFF_TRACE=1. No duration in
this report subtracts timestamps from different machines. The placement list
must be in validator ID order, for example ideapad,ideapad,tokyo,tokyo.
"""

import argparse
import csv
import json
from collections import defaultdict
from pathlib import Path


def fields(line: str) -> dict[str, str]:
    return dict(part.split("=", 1) for part in line.split() if "=" in part)


def percentile(values: list[float], percent: int) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    return round(ordered[(len(ordered) - 1) * percent // 100], 3)


def duration(stages: dict[str, int], first: str, last: str) -> float | None:
    if first not in stages or last not in stages or stages[last] < stages[first]:
        return None
    return round((stages[last] - stages[first]) / 1000, 3)


def summarize(rows: list[dict], key: str) -> dict:
    values = [row[key] for row in rows if row.get(key) is not None]
    return {"count": len(values), "p50_ms": percentile(values, 50),
            "p95_ms": percentile(values, 95), "p99_ms": percentile(values, 99)}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("chain_dir", type=Path)
    parser.add_argument("--placement", required=True,
                        help="comma-separated host name for validator IDs 0 through n-1")
    parser.add_argument("--out-prefix", type=Path, required=True)
    args = parser.parse_args()
    hosts = args.placement.split(",")
    quorum = len(hosts) - (len(hosts) - 1) // 3

    blocks: dict[tuple[int, int, int, str], dict] = defaultdict(
        lambda: {"stages": {}, "votes": defaultdict(list)})
    handoffs: dict[tuple[int, int], dict[str, int]] = defaultdict(dict)
    proposals_by_view: dict[tuple[int, int], int] = {}
    for path in sorted(args.chain_dir.glob("node-*.stderr.log")):
        for line in path.read_text(errors="replace").splitlines():
            if not line.startswith(("FAST_STAGE ", "FAST_VOTE ", "HANDOFF ")):
                continue
            data = fields(line)
            try:
                node = int(data["node"])
                time = int(data["unix_us"])
                if node >= len(hosts):
                    continue
                if line.startswith("HANDOFF "):
                    event = data["event"]
                    if event in {"new_view_sent", "new_view_quorum", "start_view_sent"}:
                        handoffs[(node, int(data["view"]))].setdefault(event, time)
                    continue
                key = (node, int(data["view"]), int(data["height"]), data["digest"])
                if line.startswith("FAST_STAGE "):
                    blocks[key]["stages"].setdefault(data["stage"], time)
                    if data["stage"] == "proposal_sent":
                        proposals_by_view.setdefault((node, int(data["view"])), time)
                else:
                    blocks[key]["votes"][data["kind"]].append(
                        (time, int(data["voter"]), data["event"]))
            except (KeyError, ValueError):
                continue

    rows = []
    for (leader, view, height, digest), block in blocks.items():
        stages = block["stages"]
        if "proposal_sent" not in stages:
            continue
        row = {"leader": leader, "leader_host": hosts[leader], "view": view,
               "height": height, "digest": digest,
               "proposal_to_prepare_ms": duration(stages, "proposal_sent", "prepare_qc_formed"),
               "prepare_to_final_ms": duration(stages, "prepare_qc_formed", "final_qc_formed"),
               "proposal_to_final_ms": duration(stages, "proposal_sent", "final_qc_formed"),
               "final_to_decision_ms": duration(stages, "final_qc_formed", "commit_decided")}
        for kind, start, end, prefix in [
            ("Prepare", "proposal_sent", "prepare_qc_formed", "prepare"),
            ("Notarize", "proposal_sent", "notarize_qc_formed", "prepare"),
            ("Final", "prepare_qc_formed", "final_qc_formed", "final"),
            ("Final", "notarize_qc_formed", "final_qc_formed", "final"),
        ]:
            if start not in stages or end not in stages:
                continue
            votes = sorted((time, voter) for time, voter, event in block["votes"][kind]
                           if (event == "received" or event == "issued" and voter == leader)
                           and time <= stages[end])
            unique = {}
            for time, voter in votes:
                unique.setdefault(voter, time)
            ordered = sorted((time, voter) for voter, time in unique.items())
            row[f"{prefix}_votes_before_qc"] = len(ordered)
            row[f"{prefix}_remote_votes_before_qc"] = sum(
                hosts[voter] != hosts[leader] for _, voter in ordered)
            if len(ordered) >= quorum:
                decisive_time, decisive_voter = ordered[quorum - 1]
                row[f"{prefix}_decisive_voter"] = decisive_voter
                row[f"{prefix}_decisive_host"] = hosts[decisive_voter]
                row[f"{prefix}_qc_after_decisive_vote_ms"] = round(
                    (stages[end] - decisive_time) / 1000, 3)
            for host in sorted(set(hosts)):
                arrivals = [time for time, voter in ordered if hosts[voter] == host and voter != leader]
                if arrivals:
                    row[f"{prefix}_first_{host}_vote_ms"] = round(
                        (min(arrivals) - stages[start]) / 1000, 3)
        rows.append(row)
    rows.sort(key=lambda row: (row["leader"], row["height"], row["view"]))

    summary = {"validator_count": len(hosts), "quorum": quorum, "leader_hosts": {}}
    for host in sorted(set(hosts)):
        group = [row for row in rows if row["leader_host"] == host]
        summary["leader_hosts"][host] = {
            "blocks": len(group),
            **{key: summarize(group, key) for key in (
                "proposal_to_prepare_ms", "prepare_to_final_ms", "proposal_to_final_ms",
                "prepare_qc_after_decisive_vote_ms", "final_qc_after_decisive_vote_ms")},
            "prepare_decisive_hosts": {name: sum(row.get("prepare_decisive_host") == name for row in group)
                                       for name in sorted(set(hosts))},
            "final_decisive_hosts": {name: sum(row.get("final_decisive_host") == name for row in group)
                                     for name in sorted(set(hosts))},
        }
    handoff_rows = []
    for (node, view), marks in handoffs.items():
        first_proposal = proposals_by_view.get((node, view))
        handoff_rows.append({"node": node, "host": hosts[node], "view": view,
                            "new_view_to_quorum_ms": duration(marks, "new_view_sent", "new_view_quorum"),
                            "quorum_to_start_ms": duration(marks, "new_view_quorum", "start_view_sent"),
                            "new_view_to_proposal_ms": round((first_proposal - marks["new_view_sent"]) / 1000, 3)
                            if first_proposal is not None and "new_view_sent" in marks
                            and first_proposal >= marks["new_view_sent"] else None})
    summary["handoff_by_host"] = {host: {key: summarize([row for row in handoff_rows if row["host"] == host], key)
                                           for key in ("new_view_to_quorum_ms", "quorum_to_start_ms", "new_view_to_proposal_ms")}
                                  for host in sorted(set(hosts))}

    args.out_prefix.parent.mkdir(parents=True, exist_ok=True)
    args.out_prefix.with_suffix(".json").write_text(json.dumps(summary, indent=2) + "\n")
    with args.out_prefix.with_suffix(".csv").open("w", newline="") as output:
        columns = sorted(set().union(*(row.keys() for row in rows))) if rows else []
        writer = csv.DictWriter(output, fieldnames=columns)
        writer.writeheader()
        writer.writerows(rows)
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
