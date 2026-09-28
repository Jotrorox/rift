"""Check that burst measurements cannot turn failed routing into fast successes."""

import asyncio
import unittest
from unittest.mock import patch

import bench


class SummaryTests(unittest.TestCase):
    def test_rejections_stay_in_denominator_and_out_of_latency_percentiles(self):
        samples = [dict(error=None, setup_ms=value, elapsed_ms=value) for value in range(1, 101)]
        samples += [dict(error="EOFError", setup_ms=None, elapsed_ms=0.01)] * 100
        result = bench.summarize(samples, [1, 2], [100, 0], 2)
        self.assertEqual((result["attempts"], result["succeeded"], result["failed"]), (200, 100, 100))
        self.assertEqual(result["success_rate"], 0.5)
        self.assertEqual(result["setup_p95_ms"], 95)
        self.assertEqual(result["setup_p99_ms"], 99)
        self.assertEqual(result["successful_setups_per_s"], 50)
        self.assertEqual(result["errors"], {"EOFError": 100})

    def test_total_failure_has_no_latency_percentiles(self):
        result = bench.summarize([dict(error="TimeoutError", setup_ms=None, elapsed_ms=10)], [0], [0], 1)
        self.assertEqual(result["success_rate"], 0)
        self.assertIsNone(result["setup_p95_ms"])
        self.assertIsNone(result["setup_p99_ms"])

    def test_linux_process_stats_handle_spaces_and_parentheses_in_name(self):
        fields = ["0"] * 22
        fields[11], fields[12], fields[21] = "125", "25", "512"
        with patch("bench.sys.platform", "linux"), \
                patch("bench.Path.read_text", return_value="123 (worker (1)) " + " ".join(fields)), \
                patch("bench.os.sysconf", side_effect=lambda key: 100 if key == "SC_CLK_TCK" else 4096):
            self.assertEqual(bench.process_sample(123), {"cpu_s": 1.5, "rss_mib": 2})


class BurstTests(unittest.IsolatedAsyncioTestCase):
    async def start_server(self, callback):
        server = await asyncio.start_server(callback, "127.0.0.1", 0)
        self.addAsyncCleanup(server.wait_closed)
        self.addCleanup(server.close)
        return server.sockets[0].getsockname()[1]

    async def attempt(self, port, scenario="hostname", timeout=1):
        gate = asyncio.Event()
        gate.set()
        return await bench.setup_attempt(port, scenario, timeout, 123, gate)

    async def test_valid_handshake_and_probe_echo_under_a_partial_final_burst(self):
        port = await self.start_server(bench.fixture_client)
        samples, spreads, successes = await bench.run_bursts(port, "hostname", 4, 10, 1)
        self.assertEqual(len(samples), 10)
        self.assertEqual(len(spreads), 3)
        self.assertEqual(successes, [4, 4, 2])
        self.assertTrue(all(sample["setup_ms"] > 0 for sample in samples))

    async def test_tcp_accept_followed_by_eof_is_not_success(self):
        def close(reader, writer):
            writer.close()
        port = await self.start_server(close)
        result = await self.attempt(port)
        self.assertIsNotNone(result["error"])
        self.assertIsNone(result["setup_ms"])

    async def test_deadline_covers_a_connected_but_silent_server(self):
        accepted = []

        def silent(reader, writer):
            accepted.append(writer)

        port = await self.start_server(silent)
        try:
            result = await self.attempt(port, timeout=0.02)
            self.assertEqual(result["error"], "TimeoutError")
            self.assertIsNone(result["setup_ms"])
        finally:
            for writer in accepted:
                writer.close()
                await writer.wait_closed()

    async def test_status_response_requires_matching_client_ping(self):
        async def wrong_ping(reader, writer):
            try:
                await bench.read_frame(reader)
                await bench.read_frame(reader)
                writer.write(bench.STATUS_REPLY)
                await writer.drain()
                await bench.read_frame(reader)
                writer.write(b"\x09\x01" + b"\0" * 8)
                await writer.drain()
            finally:
                writer.close()
                await writer.wait_closed()

        port = await self.start_server(wrong_ping)
        result = await self.attempt(port, "status_cached")
        self.assertEqual(result["error"], "ValueError")
        self.assertIsNone(result["setup_ms"])

    async def test_status_fixture_answers_the_full_exchange(self):
        port = await self.start_server(
            lambda reader, writer: bench.fixture_client(reader, writer, status=True))
        result = await self.attempt(port, "status_cached")
        self.assertIsNone(result["error"])
        self.assertGreater(result["setup_ms"], 0)


if __name__ == "__main__":
    unittest.main()
