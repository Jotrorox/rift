#!/usr/bin/env python3
"""Render the measured comparison aggregates as Markdown; accept JSON or JSON.gz."""

import argparse
import gzip
import json
from pathlib import Path


def metric(summary, key, spread=False):
    value = summary.get(key)
    if value is None:
        return "n/a"
    rendered = f"{value['median']:.2f}"
    if spread:
        rendered += f" ({value['min']:.2f}–{value['max']:.2f})"
    return rendered


def render(report):
    if report.get("schema_version") != 1 or report.get("status") != "complete":
        raise ValueError("only complete schema-1 comparison reports can be published")
    settings = report["settings"]
    lines = [
        f"{settings['clients']} clients, {settings['rate']:g} echoes/s/client, "
        f"{settings['payload_bytes']} payload bytes, {settings['trials']} trials.",
        "",
        "Medians of per-trial values; parentheses show minimum–maximum. "
        "Failure counts pool all measured trials.",
        "",
        "| Workload | Proxy | Mean CPU % | Peak RSS MiB | p50 ms | p95 ms | p99 ms | Successes/s | Failed/attempted |",
        "|---|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for workload in ("echo", "login"):
        for proxy in ("rift", "velocity"):
            summary = report["aggregate"][proxy][workload]
            if summary["trials"] != settings["trials"]:
                raise ValueError("report has an incomplete trial count")
            columns = [workload, "Rift" if proxy == "rift" else "Velocity",
                       metric(summary, "cpu_percent", True),
                       metric(summary, "rss_peak_mib", True),
                       metric(summary, "latency_p50_ms"),
                       metric(summary, "latency_p95_ms", True),
                       metric(summary, "latency_p99_ms", True),
                       metric(summary, "successful_per_s"),
                       f"{summary['failed']:,}/{summary['attempts']:,} "
                       f"({100 * summary['failure_rate']:.2f}%)"]
            lines.append("| " + " | ".join(columns) + " |")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path)
    args = parser.parse_args()
    opener = gzip.open if args.report.suffix == ".gz" else open
    with opener(args.report, "rt", encoding="utf-8") as source:
        print(render(json.load(source)), end="")


if __name__ == "__main__":
    main()
