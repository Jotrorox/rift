"""Failure-detection regressions for the real-server operational harness."""

import hashlib
from pathlib import Path
import tempfile
import subprocess
import unittest
from unittest.mock import Mock, patch
import uuid

import manual_online
import minecraft as mc
import operations as op


class OperationsHarnessTests(unittest.TestCase):
    def test_listener_shutdown_requires_refusal_after_a_racing_reset(self):
        with patch.object(op.socket, "create_connection", side_effect=[
                ConnectionResetError(), TimeoutError(), ConnectionRefusedError()]):
            self.assertFalse(op.listener_closed(1))
            self.assertFalse(op.listener_closed(1))
            self.assertTrue(op.listener_closed(1))

    def test_budgets_reject_quiet_socket_leaks_and_peak_memory_growth(self):
        baseline = dict(rss_bytes=6 * 1024 * 1024, fds=15, sockets=9)
        quiet = [dict(baseline, fds=16, sockets=10)]
        peak = [dict(baseline, fds=45, sockets=39)]
        op.assert_resources(baseline, quiet, peak)
        for key, value in [("fds", 24), ("sockets", 16)]:
            with self.subTest(key=key), self.assertRaises(AssertionError):
                op.assert_resources(baseline, [dict(baseline, **{key: value})], peak)
        for key, value in [("rss_bytes", 23 * 1024 * 1024), ("fds", 164), ("sockets", 158)]:
            with self.subTest(key=key), self.assertRaises(AssertionError):
                op.assert_resources(baseline, quiet, [dict(baseline, **{key: value})])
        with self.assertRaises(AssertionError):
            op.assert_resources(baseline, [], peak)

    def test_gameplay_worker_failure_is_visible_without_reconnect(self):
        for error in [EOFError("backend disappeared"), AssertionError("bad packet")]:
            with self.subTest(error=error), patch.object(mc, "play", side_effect=error) as play:
                with op.Player(1, "vanilla", 774, True, "play.test") as player:
                    player.thread.join(timeout=1)
                    with self.assertRaisesRegex(AssertionError, "disconnected"):
                        player.snapshot()
                    play.assert_called_once()

    def test_sampler_errors_cannot_be_reported_as_bounded_resources(self):
        with tempfile.TemporaryDirectory() as temporary:
            proxy = Mock()
            proxy.wait.side_effect = subprocess.TimeoutExpired("rift", 0.5)
            sampler = op.Resources(proxy, Path(temporary))
            with patch.object(sampler, "sample", side_effect=PermissionError("/proc unavailable")):
                with self.assertRaises(PermissionError):
                    with sampler:
                        sampler.thread.join(timeout=1)

    def test_sampler_handles_proc_disappearing_during_successful_exit(self):
        with tempfile.TemporaryDirectory() as temporary:
            proxy = Mock()
            proxy.wait.return_value = 0
            sampler = op.Resources(proxy, Path(temporary))
            with patch.object(sampler, "sample", side_effect=PermissionError("exiting fd table")):
                with sampler:
                    sampler.thread.join(timeout=1)
            self.assertIsNone(sampler.error)

    def test_authentication_probe_rejects_offline_or_optional_authentication(self):
        body = mc.string("") + mc.varint(162) + b"k" * 162 + mc.varint(4) + b"1234"
        with patch.object(mc, "Client") as client:
            receive = client.return_value.__enter__.return_value.receive
            for packet in [(2, b"offline login"), (1, body + b"\0"), (1, body[:-2])]:
                receive.return_value = packet
                with self.subTest(packet=packet), self.assertRaises((AssertionError, EOFError)):
                    manual_online.encryption_challenge(1, 774)
            receive.return_value = (1, body + b"\x01")
            self.assertTrue(manual_online.encryption_challenge(1, 774)["should_authenticate"])

    def test_manual_profile_requires_online_uuid_and_completed_login(self):
        username = "RiftTester"
        offline = uuid.UUID(bytes=hashlib.md5(f"OfflinePlayer:{username}".encode()).digest(), version=3)
        online = uuid.uuid4()
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "server.log"
            for profile, joined in [(offline, True), (online, False)]:
                log.write_text(f"UUID of player {username} is {profile}\n" +
                               (f"{username}[/127.0.0.1:12345] logged in with entity id 1\n" if joined else ""))
                with self.assertRaises(AssertionError):
                    manual_online.authenticated_profile(log, username)
            log.write_text(f"UUID of player {username} is {online}\n"
                           f"{username}[/127.0.0.1:12345] logged in with entity id 1\n")
            self.assertEqual(manual_online.authenticated_profile(log, username), str(online))


if __name__ == "__main__":
    unittest.main()
