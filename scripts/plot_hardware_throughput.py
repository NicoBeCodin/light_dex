"""Plot the 2026-10-26 hardware capacity runs from their ignored raw JSON."""

from pathlib import Path
import json

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt


ROOT = Path(__file__).resolve().parents[1]
RAW = ROOT / "benchmark_results/hardware-2026-10-26"
OUT = ROOT / "docs/figures/hardware-throughput-2026-10-26.svg"
SEVEN_OUT = ROOT / "docs/figures/hardware-seven-2026-10-26.svg"
NAMES = [
    "local200", "local400", "local800", "local1200", "local1400", "local1600",
    "local1600_window32",
    "ideapad200", "ideapad400", "ideapad800",
    "tokyo200_long", "tokyo300", "tokyo400",
    "mixed200",
]


def load(name):
    paths = list((RAW / name).glob("pilot-*/**/run.json"))
    return json.loads(paths[0].read_text()) if paths else None


def main():
    runs = [(name, load(name)) for name in NAMES]
    runs = [(name, run) for name, run in runs if run]
    colors = {"local": "#56b4e9", "ideapad": "#009e73",
              "tokyo": "#d55e00", "mixed": "#8856a7"}
    fig, (ax, bx) = plt.subplots(2, 1, figsize=(11, 9), constrained_layout=True)
    for topology in colors:
        rows = [(name, run) for name, run in runs if name.startswith(topology)]
        passing = [(name, run) for name, run in rows if run["summary"]["result"] == "pass"]
        offered = [run["summary"]["offered_tps"] for _, run in passing]
        committed = [run["summary"]["successful_tps"] for _, run in passing]
        ax.plot(offered, committed, "o-", lw=2, label=topology, color=colors[topology])
        for _, run in rows:
            summary = run["summary"]
            if summary["result"] != "pass":
                ax.scatter([summary["offered_tps"]], [summary["successful_tps"]],
                           marker="x", s=110, lw=2.5, color=colors[topology])
    ax.plot([0, 1650], [0, 1650], color="#999999", ls="--", lw=1, label="offered = committed")
    ax.set(xlabel="Offered transactions / second", ylabel="Successful commits / second",
           title="Four validators: successful order flow under offered load")
    ax.legend(ncol=4)
    ax.grid(alpha=.2)

    queue_colors = {
        "local1400": "#56b4e9", "ideapad400": "#009e73",
        "ideapad800": "#006d50", "tokyo200_long": "#d55e00",
        "tokyo300": "#bb3410", "mixed200": "#8856a7",
    }
    for name, color in queue_colors.items():
        run = load(name)
        if not run:
            continue
        samples = run["samples"]
        x = [sample["elapsed_ms"] / 1000 for sample in samples]
        y = [max(node["mempool"] for node in sample["nodes"] if node["mempool"] is not None)
             for sample in samples]
        bx.plot(x, y, lw=1.8, label=name, color=color,
                ls="--" if run["summary"]["result"] != "pass" else "-")
    bx.set(xlabel="Seconds into measurement", ylabel="Largest validator mempool (transactions)",
           title="Queue growth distinguishes throughput from sustainable throughput")
    bx.legend(ncol=3)
    bx.grid(alpha=.2)
    OUT.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(OUT)
    fig.savefig(OUT.with_suffix(".png"), dpi=170)
    print(OUT)

    seven = [(name, load(name)) for name in ["seven50", "seven100"]]
    seven = [(name, run) for name, run in seven if run]
    if seven:
        fig2, (cx, dx) = plt.subplots(1, 2, figsize=(11, 4), constrained_layout=True)
        labels = [name.replace("seven", "") + " offered" for name, _ in seven]
        positions = list(range(len(seven)))
        cx.bar([p - .2 for p in positions], [run["summary"]["delivered_extra_tps"] for _, run in seven],
               width=.4, color="#56b4e9", label="admitted")
        cx.bar([p + .2 for p in positions], [run["summary"]["successful_tps"] for _, run in seven],
               width=.4, color="#8856a7", label="successful commits")
        cx.set_xticks(positions, labels)
        cx.set(ylabel="Transactions / second", title="Three computers, seven validators")
        cx.legend()
        cx.grid(axis="y", alpha=.2)
        for name, run in seven:
            samples = run["samples"]
            dx.plot([sample["elapsed_ms"] / 1000 for sample in samples],
                    [max(node["mempool"] for node in sample["nodes"] if node["mempool"] is not None)
                     for sample in samples], label=name)
        dx.set(xlabel="Seconds into measurement", ylabel="Largest validator mempool",
               title="Queue on the most delayed validator")
        dx.legend()
        dx.grid(alpha=.2)
        fig2.savefig(SEVEN_OUT)
        fig2.savefig(SEVEN_OUT.with_suffix(".png"), dpi=170)
        print(SEVEN_OUT)


if __name__ == "__main__":
    main()
