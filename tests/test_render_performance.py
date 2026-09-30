"""Publication refuses incomplete comparisons and preserves measured failure data."""

import copy
import gzip
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from scripts import render_performance


def report_fixture():
    metric = dict(median=12, min=10, max=15)
    summary = dict(trials=3, cpu_percent=metric, rss_peak_mib=metric,
                   latency_p50_ms=metric, latency_p95_ms=metric,
                   latency_p99_ms=metric, successful_per_s=metric,
                   failed=5, attempts=100, failure_rate=.05)
    return dict(schema_version=1, status="complete",
                settings=dict(clients=16, rate=20, payload_bytes=1024, trials=3),
                aggregate={proxy: {workload: copy.deepcopy(summary) for workload in ("login", "echo")}
                           for proxy in ("rift", "velocity")})


class PerformanceRenderingTests(unittest.TestCase):
    def test_published_table_keeps_failure_counts_rates_and_trial_spread(self):
        report = report_fixture()
        report["aggregate"]["rift"]["login"]["latency_p99_ms"] = {
            "median": 12.57, "min": 11.0, "max": 135.73}
        rendered = render_performance.render(report)
        self.assertIn("16 clients, 20 echoes/s/client, 1024 payload bytes, 3 trials.", rendered)
        self.assertEqual(rendered.count("5/100 (5.00%)"), 4)
        self.assertEqual(rendered.count("| echo |"), 2)
        self.assertEqual(rendered.count("| login |"), 2)
        self.assertIn("12.00 (10.00–15.00)", rendered)
        self.assertIn("12.57 (11.00–135.73)", rendered)
        self.assertIn("Medians of per-trial values", rendered)

    def test_null_metrics_are_unavailable_and_never_fabricated_as_zero(self):
        report = report_fixture()
        summary = report["aggregate"]["velocity"]["echo"]
        summary.update(cpu_percent=None, rss_peak_mib=None, latency_p95_ms=None)
        row = next(row for row in render_performance.render(report).splitlines()
                   if row.startswith("| echo | Velocity |"))
        self.assertEqual(row.count("n/a"), 3)
        self.assertNotIn("| 0.00", row)

    def test_running_incomplete_or_unknown_schema_cannot_be_published(self):
        for field, value in (("status", "running"), ("status", "incomplete"), ("schema_version", 2)):
            report = report_fixture()
            report[field] = value
            with self.subTest(field=field, value=value), self.assertRaisesRegex(ValueError, "complete schema-1"):
                render_performance.render(report)

    def test_missing_trial_cannot_be_published_as_complete(self):
        report = report_fixture()
        report["aggregate"]["velocity"]["echo"]["trials"] = 2
        with self.assertRaisesRegex(ValueError, "incomplete trial count"):
            render_performance.render(report)

    def test_cli_renders_plain_and_compressed_evidence_identically(self):
        report = report_fixture()
        payload = json.dumps(report).encode()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            plain, compressed = root / "report.json", root / "report.json.gz"
            plain.write_bytes(payload)
            compressed.write_bytes(gzip.compress(payload, mtime=0))
            outputs = [subprocess.check_output([sys.executable, str(Path(render_performance.__file__).resolve()),
                                               str(path)], text=True) for path in (plain, compressed)]
        self.assertEqual(outputs[0], outputs[1])
        self.assertEqual(outputs[0], render_performance.render(report))


if __name__ == "__main__":
    unittest.main()
