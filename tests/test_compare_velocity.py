"""Protect fair comparison accounting, resource units, and protocol validation."""

import argparse
import asyncio
from contextlib import redirect_stderr
import io
import json
from pathlib import Path
import tempfile
import tomllib
import unittest
from unittest.mock import AsyncMock, patch

import compare_velocity as compare


class ComparisonSummaryTests(unittest.TestCase):
    def test_failures_remain_in_denominator_and_not_success_percentiles(self):
        samples = [dict(error=None, latency_ms=value, elapsed_ms=value) for value in range(1, 101)]
        samples += [dict(error="TimeoutError", latency_ms=None, elapsed_ms=250)] * 100
        result = compare.summarize(samples, 2)
        self.assertEqual((result["attempts"], result["succeeded"], result["failed"]), (200, 100, 100))
        self.assertEqual(result["failure_rate"], .5)
        self.assertEqual(result["successful_per_s"], 50)
        self.assertEqual(result["latency_p50_ms"], 50)
        self.assertEqual(result["latency_p95_ms"], 95)
        self.assertEqual(result["latency_p99_ms"], 99)
        self.assertEqual(result["failure_p95_ms"], 250)
        self.assertEqual(result["errors"], {"TimeoutError": 100})

    def test_total_failure_or_no_observations_never_claim_zero_latency(self):
        failure = compare.summarize([dict(error="EOFError", latency_ms=None, elapsed_ms=1)], 1)
        empty = compare.summarize([], 0)
        self.assertEqual(failure["failure_rate"], 1)
        self.assertIsNone(empty["failure_rate"])
        self.assertIsNone(empty["successful_per_s"])
        for result in (failure, empty):
            for key in ("latency_p50_ms", "latency_p95_ms", "latency_p99_ms"):
                self.assertIsNone(result[key])

    def test_nearest_rank_handles_unsorted_and_small_samples(self):
        self.assertEqual(compare.percentile([100, 1, 4, 2, 3], .5), 3)
        self.assertEqual(compare.percentile([100, 1, 4, 2, 3], .95), 100)
        self.assertEqual(compare.percentile([42], .99), 42)
        self.assertIsNone(compare.percentile([], .95))

    def test_aggregate_pools_failure_counts_and_retains_trial_spread(self):
        results = []
        for attempts, failed, latency, cpu in ((10, 5, 10, 20), (100, 1, 20, 40)):
            trial = dict(attempts=attempts, failed=failed, latency_p50_ms=latency,
                         latency_p95_ms=latency + 1, latency_p99_ms=latency + 2,
                         successful_per_s=attempts - failed,
                         resources={"proxy": dict(cpu_s=cpu / 100, cpu_percent=cpu, rss_peak_mib=8)})
            results.append(dict(proxy="rift", login=trial, echo=trial))
        report = compare.aggregate(results)
        self.assertEqual(report["velocity"], {})
        for workload in ("login", "echo"):
            values = report["rift"][workload]
            self.assertEqual((values["attempts"], values["failed"]), (110, 6))
            self.assertAlmostEqual(values["failure_rate"], 6 / 110)
            self.assertEqual(values["latency_p95_ms"], dict(median=16, min=11, max=21))
            self.assertEqual(values["cpu_percent"], dict(median=30, min=20, max=40))

    def test_unavailable_resources_remain_null_in_aggregate(self):
        trial = compare.summarize([dict(error="EOFError", latency_ms=None, elapsed_ms=1)], 1)
        trial["resources"] = {"proxy": None}
        report = compare.aggregate([dict(proxy="velocity", login=trial)])
        self.assertIsNone(report["velocity"]["login"]["cpu_percent"])
        self.assertIsNone(report["velocity"]["login"]["rss_peak_mib"])
        self.assertIsNone(report["velocity"]["login"]["latency_p95_ms"])

    def test_strict_mode_detects_every_measured_warmup_and_setup_failure(self):
        workloads = ("login", "echo", "warmup_login", "warmup_echo")
        clean = {workload: dict(failed=0, setup=[dict(error=None)]) for workload in workloads}
        self.assertFalse(compare.report_has_failures(dict(results=[clean])))
        for workload in workloads:
            for failure in (dict(failed=1), dict(failed=0, setup=[dict(error="TimeoutError")])):
                result = dict(clean, **{workload: failure})
                with self.subTest(workload=workload, failure=failure):
                    self.assertTrue(compare.report_has_failures(dict(results=[clean, result])))

    def test_resource_cpu_percent_is_per_core_and_driver_is_separate(self):
        before = {"proxy": dict(cpu_s=1, rss_mib=8), "driver": dict(cpu_s=5, rss_mib=12)}
        after = {"proxy": dict(cpu_s=4, rss_mib=9), "driver": dict(cpu_s=13, rss_mib=14)}
        with patch.object(compare.Resources, "sample", side_effect=[before, after]), \
                patch("bench.threading.Thread"), patch("bench.time.perf_counter", side_effect=[10, 12]):
            with compare.Resources({"proxy": 1, "driver": 2}) as resources:
                resources.peaks["proxy"] = 20
        self.assertEqual(resources.elapsed, 2)
        self.assertEqual(resources.result["proxy"], dict(cpu_s=3, cpu_percent=150,
                         rss_start_mib=8, rss_end_mib=9, rss_peak_mib=20))
        self.assertEqual(resources.result["driver"]["cpu_percent"], 400)

    def test_resource_sampling_failure_cannot_be_silently_published(self):
        sample = {"proxy": dict(cpu_s=0, rss_mib=8)}
        with patch.object(compare.Resources, "sample", return_value=sample), \
                patch("bench.threading.Thread"), patch("bench.time.perf_counter", side_effect=[10, 12]):
            with self.assertRaisesRegex(OSError, "process disappeared"):
                with compare.Resources({"proxy": 1}) as resources:
                    resources.failure = OSError("process disappeared")


class ComparisonConfigurationTests(unittest.TestCase):
    def test_configs_share_backend_and_disable_unequal_velocity_features(self):
        configs = compare.configurations(25565, 25566)
        velocity = tomllib.loads(configs["velocity"])
        self.assertEqual(velocity["bind"], "127.0.0.1:25565")
        self.assertEqual(velocity["servers"]["backend"], "127.0.0.1:25566")
        self.assertEqual(velocity["servers"]["try"], ["backend"])
        self.assertIn('public = "127.0.0.1:25565"', configs["rift"])
        self.assertIn('backend = "127.0.0.1:25566"', configs["rift"])
        self.assertFalse(velocity["online-mode"])
        self.assertFalse(velocity["force-key-authentication"])
        self.assertEqual(velocity["player-info-forwarding-mode"], "none")
        self.assertEqual(velocity["advanced"]["compression-threshold"], -1)
        self.assertEqual(velocity["advanced"]["login-ratelimit"], 0)
        self.assertFalse(velocity["advanced"]["bungee-plugin-message-channel"])
        self.assertTrue(all(value == -1 for value in velocity["packet-limiter"].values()))

    def test_effective_config_rejects_migration_drift_and_telemetry(self):
        source = compare.configurations(25565, 25566)["velocity"]
        compare.verify_velocity_config(source, "enabled=false\nserver-uuid=fixture\n")
        changes = [
            ("online-mode = false", "online-mode = true"),
            ('player-info-forwarding-mode = "none"', 'player-info-forwarding-mode = "modern"'),
            ("compression-threshold = -1", "compression-threshold = 256"),
            ("login-ratelimit = 0", "login-ratelimit = 3000"),
            ("log-player-connections = false", "log-player-connections = true"),
            ("packets-per-second = -1", "packets-per-second = 1000"),
            ("bytes-per-second = -1", "bytes-per-second = 10000"),
            ("decompressed-bytes-per-second = -1", "decompressed-bytes-per-second = 20000"),
        ]
        for before, after in changes:
            with self.subTest(setting=before), self.assertRaisesRegex(ValueError, "effective config changed"):
                compare.verify_velocity_config(source.replace(before, after), "enabled=false\n")
        for metrics in ("enabled=true\n", "", "# enabled=false\n"):
            with self.subTest(metrics=metrics), self.assertRaisesRegex(ValueError, "bStats was not disabled"):
                compare.verify_velocity_config(source, metrics)

    def test_nonfinite_and_nonpositive_workload_inputs_are_rejected(self):
        for value in ("0", "-1", "nan", "inf", "-inf"):
            with self.subTest(value=value), self.assertRaises(argparse.ArgumentTypeError):
                compare.positive_float(value)
        self.assertEqual(compare.positive_float("0.01"), .01)
        for value in ("0", "-1"):
            with self.subTest(value=value), self.assertRaises(argparse.ArgumentTypeError):
                compare.positive_int(value)

    def test_cli_rejects_invalid_workload_shapes_before_starting_processes(self):
        base = ["--velocity-jar", "placeholder.jar", "--report", "placeholder.json"]
        invalid = [
            ["--payload-bytes", "7"], ["--payload-bytes", "32768"],
            ["--clients", "4097"], ["--burst-size", "4097"],
            ["--attempts", "2", "--burst-size", "3"],
            ["--duration", ".01", "--rate", "1"],
            ["--warmup", ".01", "--rate", "1"],
            ["--trials", "0"], ["--timeout", "nan"],
        ]
        for arguments in invalid:
            with self.subTest(arguments=arguments), redirect_stderr(io.StringIO()), \
                    self.assertRaises(SystemExit) as raised:
                compare.parse_args(base + arguments)
            self.assertEqual(raised.exception.code, 2)

    def test_report_rejects_nonfinite_metrics_without_replacing_previous_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "nested" / "report.json"
            report = dict(schema_version=1, status="complete", latency=None)
            compare.write_report(report, path)
            self.assertEqual(json.loads(path.read_text()), report)
            with self.assertRaises(ValueError):
                compare.write_report(dict(latency=float("nan")), path)
            self.assertEqual(json.loads(path.read_text()), report)

    def test_wrong_velocity_checksum_is_rejected_before_any_process_starts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            jar = root / "velocity.jar"
            jar.write_bytes(b"wrong jar")
            manifest = root / "manifest.json"
            manifest.write_text(json.dumps(dict(sha256="0" * 64)))
            args = argparse.Namespace(fixture=False, velocity_manifest=manifest, velocity_jar=jar)
            with patch.object(compare, "parse_args", return_value=args), \
                    patch.object(compare, "process") as process, \
                    self.assertRaisesRegex(SystemExit, "Velocity SHA-256 mismatch"):
                compare.main([])
            process.assert_not_called()


class ComparisonFixtureTests(unittest.IsolatedAsyncioTestCase):
    async def start_server(self, callback=compare.fixture_client):
        server = await compare.fixture_server(callback)
        self.addAsyncCleanup(server.wait_closed)
        self.addCleanup(server.close)
        return server.sockets[0].getsockname()[1]

    async def test_fixture_verifies_login_join_and_echo_at_payload_boundaries(self):
        port = await self.start_server()
        game = await compare.open_game(port, 1)
        try:
            for size in (8, 1024, 32767):
                await game.echo(b"x" * size, 1)
        finally:
            await game.close()

    async def test_bursts_keep_partial_final_wave_and_every_attempt(self):
        port = await self.start_server()
        samples, spreads = await compare.run_bursts(port, attempts=7, burst_size=3, timeout=1)
        self.assertEqual([sample["index"] for sample in samples], list(range(7)))
        self.assertEqual(len(spreads), 3)
        self.assertTrue(all(sample["error"] is None for sample in samples))
        self.assertTrue(all(sample["latency_ms"] > 0 for sample in samples))
        self.assertTrue(all(value >= 0 for value in spreads))

    async def test_wrong_player_identity_never_counts_as_success(self):
        async def wrong_identity(reader, writer):
            try:
                await compare.read_frame(reader)
                await compare.read_frame(reader)
                writer.write(compare.login_success("WrongName") + compare.packet(compare.JOIN_BODY))
                await writer.drain()
            finally:
                await compare.close_writer(writer)
        port = await self.start_server(wrong_identity)
        result = await compare.login_attempt(port, 1, 0)
        self.assertEqual(result["error"], "ValueError")
        self.assertIsNone(result["latency_ms"])

    async def test_missing_join_game_or_invalid_probe_never_counts_as_success(self):
        for stage in ("join", "probe"):
            async def incomplete(reader, writer):
                try:
                    await compare.read_frame(reader)
                    name = compare.login_name(await compare.read_frame(reader))
                    writer.write(compare.login_success(name))
                    if stage == "probe":
                        writer.write(compare.packet(compare.JOIN_BODY))
                    await writer.drain()
                    if stage == "probe":
                        await compare.read_frame(reader)
                        writer.write(compare.packet(b"\x3f" + compare.CHANNEL_BYTES + b"wrong probe"))
                        await writer.drain()
                finally:
                    await compare.close_writer(writer)
            with self.subTest(stage=stage):
                port = await self.start_server(incomplete)
                result = await compare.login_attempt(port, 1, 0)
                self.assertIsNotNone(result["error"])
                self.assertIsNone(result["latency_ms"])

    async def test_connection_timeout_is_recorded_with_no_latency(self):
        with patch.object(compare, "open_game", side_effect=TimeoutError("deadline")):
            result = await compare.login_attempt(1, .01, 0)
        self.assertEqual(result["error"], "TimeoutError")
        self.assertEqual(result["detail"], "deadline")
        self.assertIsNone(result["latency_ms"])

    async def test_missing_connections_keep_every_offered_echo_in_failure_count(self):
        samples = await compare.run_echo([None, None], duration=.11, rate=20, payload_bytes=8, timeout=1)
        self.assertEqual(len(samples), 6)
        self.assertEqual({sample["error"] for sample in samples}, {"connection_unavailable"})
        self.assertEqual(compare.summarize(samples, .11)["failure_rate"], 1)
        self.assertEqual([sample["sequence"] for sample in samples], [0, 1, 2, 0, 1, 2])

    async def test_failed_echo_preserves_later_offered_slots(self):
        game = unittest.mock.Mock()
        game.echo = AsyncMock(side_effect=TimeoutError("response deadline"))
        samples = await compare.run_echo([game], duration=.11, rate=20, payload_bytes=8, timeout=1)
        self.assertEqual(len(samples), 3)
        self.assertEqual(samples[0]["error"], "TimeoutError")
        self.assertEqual([sample["error"] for sample in samples[1:]], ["connection_unavailable"] * 2)
        self.assertTrue(all(sample["latency_ms"] is None for sample in samples))
        game.echo.assert_awaited_once()

    async def test_late_driver_slots_cannot_disappear_from_denominator(self):
        clock = [0.0]
        async def sleep(seconds):
            clock[0] += max(0, seconds)
        async def slow_echo(*_):
            clock[0] += .11
        game = unittest.mock.Mock()
        game.echo = AsyncMock(side_effect=slow_echo)
        with patch.object(compare.time, "perf_counter", side_effect=lambda: clock[0]), \
                patch.object(compare.asyncio, "sleep", side_effect=sleep):
            samples = await compare.run_echo([game], duration=.16, rate=20, payload_bytes=8, timeout=1)
        self.assertEqual(len(samples), 4)
        self.assertEqual([sample["error"] for sample in samples], [None, "schedule_overrun", None, "schedule_overrun"])
        self.assertEqual(compare.summarize(samples, .22)["failure_rate"], .5)
        self.assertGreaterEqual(samples[0]["latency_ms"], 110)


if __name__ == "__main__":
    unittest.main()
