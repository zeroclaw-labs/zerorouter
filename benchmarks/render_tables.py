#!/usr/bin/env python3
"""Render results/results.json as the REPORT.md markdown tables.

Every number in REPORT.md comes from this renderer over a results.json that a
`./run.sh bench` actually produced — the report never carries a hand-typed
figure. Usage: python3 render_tables.py results/results.json
"""

import json
import sys

ORDER = ["baseline", "bifrost", "litellm", "zerorouter-free", "zerorouter-metered"]
LABEL = {
    "baseline": "**Baseline** (mock direct, no gateway)",
    "bifrost": "**Bifrost** (Go)",
    "litellm": "**LiteLLM** (Python, 1 worker)",
    "zerorouter-free": "**ZeroRouter free lane** (Rust, metering skipped)",
    "zerorouter-metered": "**ZeroRouter metered** (Rust, full reserve→settle, single key)",
    "zerorouter-metered-multiuser": "**ZeroRouter metered** (16 users)",
}


def main(path: str) -> None:
    rows = json.load(open(path))
    by = {(r["name"], r["mode"]): r for r in rows}

    def fixed(name):
        return by.get((name, "fixed"))

    base = fixed("baseline")

    print("### Fixed rate (100 req/s, 60 s per cell)\n")
    print(
        "| Target | p50 (ms) | p95 (ms) | p99 (ms) | Overhead vs baseline (p50) |"
        " Throughput | Success | CPU % (100 = 1 core) | Peak RSS (MB) |"
    )
    print("|---|---:|---:|---:|---:|---:|---:|---:|---:|")
    for name in ORDER:
        r = fixed(name)
        if not r:
            continue
        overhead = (
            "—"
            if name == "baseline" or not base
            else f"+{r['p50_ms'] - base['p50_ms']:.2f} ms"
        )
        print(
            f"| {LABEL[name]} | {r['p50_ms']:.2f} | {r['p95_ms']:.2f} | {r['p99_ms']:.2f} "
            f"| {overhead} | {r['rps']:.0f}/s | {r['success'] * 100:.0f}% "
            f"| {r['cpu_pct']:.0f} | {r['peak_rss_mb']:.0f} |"
        )

    print("\n### Saturation (open loop, 60 s per cell)\n")
    print(
        "| Target | Concurrency | Throughput (req/s) | p50 (ms) | p95 (ms) | p99 (ms) |"
        " Success | CPU % | Peak RSS (MB) |"
    )
    print("|---|---|---:|---:|---:|---:|---:|---:|---:|")
    for name in ORDER:
        r = by.get((name, "saturation"))
        if not r:
            continue
        print(
            f"| {LABEL[name]} | 50 conns | **{r['rps']:,.0f}** | {r['p50_ms']:.2f} "
            f"| {r['p95_ms']:.2f} | {r['p99_ms']:.2f} | {r['success'] * 100:.0f}% "
            f"| {r['cpu_pct']:.0f} | {r['peak_rss_mb']:.0f} |"
        )
    mu = by.get(("zerorouter-metered-multiuser", "saturation-multiuser"))
    if mu:
        p50, p95, p99 = (
            mu["p50_ms_workers"],
            mu["p95_ms_workers"],
            mu["p99_ms_workers"],
        )
        print(
            f"| {LABEL['zerorouter-metered-multiuser']} | {mu['users']}×{mu['conns_per_user']} conns "
            f"| **{mu['rps']:,.0f}** | {p50['median']:.2f} | {p95['median']:.2f} | {p99['median']:.2f} "
            f"| {mu['success'] * 100:.0f}% | {mu['cpu_pct']:.0f} | {mu['peak_rss_mb']:.0f} |"
        )
        print(
            f"\nMulti-user latency cells are the MEDIAN worker's percentile "
            f"(per-worker spread: p50 {p50['min']:.2f}–{p50['max']:.2f} ms, "
            f"p95 {p95['min']:.2f}–{p95['max']:.2f} ms, "
            f"p99 {p99['min']:.2f}–{p99['max']:.2f} ms across {mu['users']} workers); "
            f"throughput is the exact sum of per-worker rates. oha exposes no raw "
            f"samples, and pooling percentiles across workers would fabricate a number."
        )


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "results/results.json")
