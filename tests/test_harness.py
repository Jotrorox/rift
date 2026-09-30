"""Fast, offline tests for the integration client's failure detection."""

import hashlib
import io
from pathlib import Path
import socket
import struct
import tempfile
import time
import unittest
from unittest.mock import patch
import zlib

import minecraft as mc


class HarnessTests(unittest.TestCase):
    def test_varint_wire_values_and_boundaries(self):
        for value, encoded in [(0, b"\x00"), (127, b"\x7f"), (128, b"\x80\x01"),
                               (774, b"\x86\x06"), (0xFFFFFFFF, b"\xff\xff\xff\xff\x0f")]:
            with self.subTest(value=value):
                self.assertEqual(mc.varint(value), encoded)
                self.assertEqual(mc.read_varint(io.BytesIO(encoded)), value)

    def test_bad_varints_fail_instead_of_truncating_or_hanging(self):
        for data in [b"\x80" * 6, b"\xff\xff\xff\xff\x10"]:
            with self.assertRaises(ValueError):
                mc.read_varint(io.BytesIO(data))
        with self.assertRaises(EOFError):
            mc.read_varint(io.BytesIO(b"\x80"))
        for value in [-1, 1 << 32]:
            with self.assertRaises(ValueError):
                mc.varint(value)

    def client(self, data, threshold=None):
        # Socket-backed reader: exercise actual EOF/framing behavior without Java.
        reader, writer = socket.socketpair()
        self.addCleanup(reader.close)
        self.addCleanup(writer.close)
        writer.sendall(data)
        writer.shutdown(socket.SHUT_WR)
        client = mc.Client.__new__(mc.Client)
        client.socket = reader
        client.reader = reader.makefile("rb")
        self.addCleanup(client.reader.close)
        client.threshold = threshold
        client.compressed_packets = 0
        client.deadline = time.monotonic() + 2
        return client

    def test_multiple_packets_in_one_tcp_write(self):
        client = self.client(b"\x03\x01ab\x02\x02c")
        self.assertEqual(client.receive(), (1, b"ab"))
        self.assertEqual(client.receive(), (2, b"c"))
        with self.assertRaises(EOFError):
            client.receive()

    def test_compressed_and_below_threshold_packets(self):
        payload = b"\x05" + b"a" * 512
        frame = mc.varint(len(payload)) + zlib.compress(payload)
        client = self.client(mc.varint(len(frame)) + frame + b"\x03\x00\x01x", 256)
        self.assertEqual(client.receive(), (5, b"a" * 512))
        self.assertEqual(client.receive(), (1, b"x"))
        self.assertEqual(client.compressed_packets, 1)

    def test_truncated_or_invalid_compressed_packets_fail(self):
        with self.assertRaises(EOFError):
            self.client(b"\x05\x00a").receive()
        frame = mc.varint(600) + zlib.compress(b"\x05" + b"a" * 512)
        with self.assertRaises(AssertionError):
            self.client(mc.varint(len(frame)) + frame, 256).receive()
        with self.assertRaises(AssertionError):
            self.client(mc.varint(9 * 1024 * 1024)).receive()

    def test_version_specific_teleport_acknowledgements(self):
        position = struct.pack(">ddd", 1, 64, -2)
        rotation = struct.pack(">ff", 90, 0)
        body = b"\x01" + position + bytes(24) + rotation + bytes(4)
        old_body = position + rotation + b"\0\x81\x01"
        self.assertEqual(mc.teleport_acknowledgement(old_body, 767), b"\x81\x01")
        for protocol in range(768, 777):
            self.assertEqual(mc.teleport_acknowledgement(body, protocol), b"\x01")
        self.assertEqual(mc.teleport_acknowledgement(body, 777), b"\x01" + position + rotation)
        with self.assertRaises(AssertionError):
            mc.teleport_acknowledgement(body[:-1] + b"\x01", 777)

    def test_client_settings_and_chat_update_layout_boundaries(self):
        self.assertEqual(mc.client_settings(768), mc.client_settings(767) + b"\2")
        self.assertEqual(mc.empty_chat_update(769), bytes(5))
        self.assertEqual(mc.empty_chat_update(770), bytes(5) + b"\1")

    def test_every_switchable_fixture_has_an_explicit_client_mapping(self):
        covered = {fixture["protocol"] for fixture in mc.SERVERS.values()
                   if fixture.get("switchable")}
        self.assertEqual(len(covered), 50)
        self.assertEqual((min(covered), max(covered)), (47, 777))
        self.assertTrue(set(range(767, 778)) <= covered)
        self.assertEqual(covered, set(mc.PROTOCOLS))
        fixtures = [fixture for fixture in mc.SERVERS.values() if fixture.get("switchable")]
        self.assertEqual(len(fixtures), 66)
        self.assertEqual({fixture["java"] for fixture in fixtures}, {8, 17, 21, 25})
        self.assertIsNone(mc.PROTOCOLS[767]["loaded"])
        self.assertIsNone(mc.PROTOCOLS[768]["loaded"])
        self.assertEqual(mc.PROTOCOLS[769]["loaded"], 0x2A)

    def test_download_verifies_fresh_and_cached_artifacts(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.write_bytes(b"fixture")
            fixture = dict(url=source.as_uri(), algorithm="sha256", filename="fixture.bin",
                           checksum=hashlib.sha256(b"fixture").hexdigest())
            with patch.object(mc, "CACHE", root / "cache"), patch.dict(mc.SERVERS, fixture=fixture):
                downloaded = mc.download("fixture")
                self.assertEqual(downloaded.read_bytes(), b"fixture")
                downloaded.write_bytes(b"corrupt cache")
                with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                    mc.download("fixture")
                downloaded.unlink()
                source.write_bytes(b"corrupt download")
                with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                    mc.download("fixture")
                self.assertFalse(downloaded.exists())


if __name__ == "__main__":
    unittest.main()
