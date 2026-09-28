"""Failure-detection regressions for the real-server operational harness."""

import base64
import copy
import json
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

    def test_manual_profile_requires_mojang_uuid_ip_and_signed_textures(self):
        username = "RiftTester"
        player_id = str(uuid.uuid4())
        expected = dict(id=player_id, name=username)
        texture = base64.b64encode(json.dumps(dict(profileId=player_id, profileName=username)).encode()).decode()
        record = dict(uuid=player_id, name=username, ip="127.0.0.1", properties=[
            dict(name="textures", value=texture, signature="fixture-signature")])
        manual_online.verify_profile(record, expected, "127.0.0.1")
        for key, value in [("uuid", str(uuid.uuid4())), ("name", "Impostor"),
                           ("ip", "203.0.113.1"), ("properties", [])]:
            with self.subTest(key=key), self.assertRaises(AssertionError):
                manual_online.verify_profile(dict(record, **{key: value}), expected, "127.0.0.1")
        unsigned = copy.deepcopy(record)
        del unsigned["properties"][0]["signature"]
        with self.assertRaisesRegex(AssertionError, "signature"):
            manual_online.verify_profile(unsigned, expected, "127.0.0.1")
        other_account = copy.deepcopy(record)
        other_account["properties"][0]["value"] = base64.b64encode(json.dumps(
            dict(profileId=str(uuid.uuid4()), profileName=username)).encode()).decode()
        with self.assertRaises(AssertionError):
            manual_online.verify_profile(other_account, expected, "127.0.0.1")

    def test_manual_inventory_requires_preserved_marker_count(self):
        record = dict(inventory=[dict(item="minecraft:diamond", count=3),
                                 dict(item="minecraft:diamond", count=4),
                                 dict(item="minecraft:emerald", count=11)])
        manual_online.verify_inventory(record, "minecraft:diamond", 7)
        with self.assertRaises(AssertionError):
            manual_online.verify_inventory(record, "minecraft:diamond", 6)
        with self.assertRaises(AssertionError):
            manual_online.verify_inventory(record, "minecraft:gold_ingot", 7)

    def test_manual_wrong_secret_requires_a_switch_failure_for_the_target(self):
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "proxy.log"
            unrelated = [dict(event="connection_failed", backend="primary"),
                         dict(event="backend_switch_failed", backend="lobby")]
            log.write_text("rift: listening\n" + "".join(json.dumps(event) + "\n" for event in unrelated))
            self.assertEqual(manual_online.switch_failures(log, "primary"), [])
            expected = dict(event="backend_switch_failed", backend="primary", connection_id=3,
                            error_kind="PermissionDenied", message="Unable to verify player details")
            with log.open("a") as output:
                output.write(json.dumps(expected) + "\n" + '{"event":')
            self.assertEqual(manual_online.switch_failures(log, "primary"), [expected])


if __name__ == "__main__":
    unittest.main()
