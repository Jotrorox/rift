"""Offline unit tests for tests/real_client.py: metadata rules, log classification, the relay."""

import json
import socket
import sys
import threading
import time
import unittest
import zlib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import real_client as rc  # noqa: E402


class Rules(unittest.TestCase):
    def test_no_rules_allow(self):
        self.assertTrue(rc.rules_allow(None))
        self.assertTrue(rc.rules_allow([]))

    def test_last_matching_rule_wins(self):
        rules = [{"action": "allow"}, {"action": "disallow", "os": {"name": "osx"}}]
        self.assertFalse(rc.rules_allow(rules, os_name="osx", arch="arm64"))
        self.assertTrue(rc.rules_allow(rules, os_name="linux", arch="x86_64"))

    def test_os_only_allow(self):
        rules = [{"action": "allow", "os": {"name": "osx"}}]
        self.assertTrue(rc.rules_allow(rules, os_name="osx", arch="arm64"))
        self.assertFalse(rc.rules_allow(rules, os_name="windows", arch="x86_64"))

    def test_x86_means_32_bit(self):
        rules = [{"action": "allow", "os": {"arch": "x86"}}]
        self.assertFalse(rc.rules_allow(rules, os_name="osx", arch="x86_64"))
        self.assertFalse(rc.rules_allow(rules, os_name="osx", arch="arm64"))

    def test_windows_version_ranges_never_match_elsewhere(self):
        rules = [{"action": "allow", "os": {"name": "windows", "versionRange": {"min": "10.0.17134"}}}]
        self.assertFalse(rc.rules_allow(rules, os_name="osx", arch="arm64"))

    def test_features(self):
        rules = [{"action": "allow", "features": {"is_quick_play_multiplayer": True}}]
        self.assertFalse(rc.rules_allow(rules, os_name="osx", arch="arm64"))
        self.assertTrue(rc.rules_allow(rules, os_name="osx", arch="arm64",
                                       features={"is_quick_play_multiplayer": True}))


class Metadata(unittest.TestCase):
    VERSION = {
        "libraries": [
            {"name": "com.mojang:brigadier:1.0.18",
             "downloads": {"artifact": {"path": "com/mojang/brigadier/1.0.18/brigadier-1.0.18.jar",
                                        "url": "https://libraries.minecraft.net/b.jar", "sha1": "a" * 40,
                                        "size": 1}}},
            {"name": "org.lwjgl:lwjgl:3.3.1:natives-macos-arm64",
             "rules": [{"action": "allow", "os": {"name": "osx"}}],
             "downloads": {"artifact": {"path": "x.jar", "url": "https://x", "sha1": "b" * 40, "size": 1}}},
            {"name": "org.lwjgl.lwjgl:lwjgl-platform:2.9.4", "natives": {"osx": "natives-osx"},
             "downloads": {"classifiers": {"natives-osx": {"path": "y.jar", "url": "https://y"}}}},
            {"name": "ca.weblite:java-objc-bridge:1.1", "rules": [{"action": "allow", "os": {"name": "osx"}}],
             "downloads": {"artifact": {"path": "objc.jar", "url": "https://o", "sha1": "c" * 40, "size": 1}}},
        ],
        "arguments": {
            "game": ["--username", "${auth_player_name}",
                     {"rules": [{"action": "allow", "features": {"is_quick_play_multiplayer": True}}],
                      "value": ["--quickPlayMultiplayer", "${quickPlayMultiplayer}"]}],
            "jvm": [{"rules": [{"action": "allow", "os": {"name": "osx"}}], "value": ["-XstartOnFirstThread"]},
                    "-cp", "${classpath}"],
        },
    }

    def test_natives_are_skipped_and_rules_applied(self):
        paths = [path for path, *_ in rc.libraries(self.VERSION, os_name="osx", arch="arm64")]
        self.assertEqual(paths, ["com/mojang/brigadier/1.0.18/brigadier-1.0.18.jar", "objc.jar"])
        paths = [path for path, *_ in rc.libraries(self.VERSION, os_name="linux", arch="x86_64")]
        self.assertEqual(paths, ["com/mojang/brigadier/1.0.18/brigadier-1.0.18.jar"])

    def test_maven_path(self):
        self.assertEqual(rc.maven_path("io.netty:netty-all:4.1.9.Final"),
                         "io/netty/netty-all/4.1.9.Final/netty-all-4.1.9.Final.jar")
        self.assertEqual(rc.maven_path("a.b:c:1:natives-osx"), "a/b/c/1/c-1-natives-osx.jar")

    def test_arguments_and_features(self):
        variables = {"auth_player_name": "Bot", "quickPlayMultiplayer": "h:1", "classpath": "cp"}
        self.assertEqual(rc.arguments(self.VERSION, "game", variables), ["--username", "Bot"])
        self.assertEqual(rc.arguments(self.VERSION, "game", variables, {"is_quick_play_multiplayer": True}),
                         ["--username", "Bot", "--quickPlayMultiplayer", "h:1"])
        self.assertTrue(rc.supports_quick_play(self.VERSION))

    def test_legacy_arguments(self):
        legacy = {"minecraftArguments": "--username ${auth_player_name} --version ${version_name}"}
        self.assertEqual(rc.arguments(legacy, "game", {"auth_player_name": "A", "version_name": "1.12.2"}),
                         ["--username", "A", "--version", "1.12.2"])
        self.assertEqual(rc.arguments(legacy, "jvm", {"natives_directory": "/n", "classpath": "cp"}),
                         ["-Djava.library.path=/n", "-cp", "cp"])
        self.assertFalse(rc.supports_quick_play(legacy))

    def test_assets_skip_sounds_and_other_languages(self):
        self.assertFalse(rc.wanted_asset("minecraft/sounds/ambient/cave/cave1.ogg"))
        self.assertFalse(rc.wanted_asset("minecraft/lang/de_de.json"))
        self.assertTrue(rc.wanted_asset("minecraft/lang/en_us.json"))
        self.assertFalse(rc.wanted_asset("minecraft/sounds.json"))
        self.assertTrue(rc.wanted_asset("icons/icon_16x16.png"))

    def test_offline_uuid_matches_the_server(self):
        # Offline-mode servers derive UUID v3 from "OfflinePlayer:<name>".
        self.assertEqual(str(rc.offline_uuid("Notch")), "b50ad385-829d-3141-a216-7e7d7539ba7f")

    def test_version_key(self):
        self.assertLess(rc.version_key("1.12.2"), rc.version_key("1.13"))
        self.assertLess(rc.version_key("1.21.11"), rc.version_key("26.1"))


def entry(level, message, logger="Minecraft", thread="Render thread", trace=()):
    return rc.Entry(level, thread, logger, message, list(trace))


class Classification(unittest.TestCase):
    def test_parse_log_groups_traces(self):
        lines = [
            "[06:43:15.849] [Download-2/ERROR] [Minecraft]: Failed to fetch user properties",
            "com.mojang.authlib.exceptions.AuthenticationUnavailableException",
            "\tat net.minecraft.client.Minecraft.lambda$new$1(Minecraft.java:451)",
            "WARNING: sun.misc.Unsafe::objectFieldOffset has been called",
            "[06:43:17.289] [Render thread/INFO] [ConnectScreen]: Connecting to 127.0.0.1, 63185",
        ]
        entries = rc.parse_log(lines)
        self.assertEqual(len(entries), 2)
        self.assertEqual(entries[0].level, "ERROR")
        self.assertEqual(len(entries[0].trace), 2)  # exception header + frame
        self.assertEqual(rc.classify(entries[0]), "noise")
        self.assertTrue(rc.CONNECTING.search(entries[1].message))

    def test_info_is_ok(self):
        self.assertEqual(rc.classify(entry("INFO", "Internal Exception: whatever")), "ok")

    def test_headless_noise(self):
        for message in ("Error starting SoundSystem. Turning off sounds & music",
                        "File minecraft:sounds/step/grass2.ogg does not exist, cannot add it to event x",
                        "Failed to fetch Realms feature flags",
                        "Ignoring chat session from Bot due to missing Services public key",
                        "Unable to parse version 27.3 to a codename.",
                        "Requested post effect does not exist: minecraft:end_of_frame",
                        "Error parsing option value 0 for option Smooth Lighting: Not a boolean: 0"):
            self.assertEqual(rc.classify(entry("WARN", message)), "noise", message)

    def test_network_failures(self):
        for message, trace in (
                ("Error receiving packet 127", ["io.netty.handler.codec.DecoderException: Bad packet id 16383"]),
                ("Failed to handle packet net.minecraft.network.protocol.game.ClientboundSystemChatPacket", []),
                ("Packet 0x26 was larger than I expected, found 3 bytes extra", []),
                ("Unknown registry key in ResourceKey[minecraft:root / minecraft:dimension_type]", []),
                ("Reported exception thrown!", [])):
            self.assertEqual(rc.classify(entry("ERROR", message, trace=trace)), "failure", message)

    def test_fatal_and_network_threads_fail(self):
        self.assertEqual(rc.classify(entry("FATAL", "something odd")), "failure")
        self.assertEqual(rc.classify(entry("WARN", "something odd", thread="Netty Client IO #0")), "failure")

    def test_noise_with_a_network_stack_is_a_failure(self):
        trace = ["at net.minecraft.network.Connection.channelRead0(Connection.java:1)"]
        self.assertEqual(rc.classify(entry("ERROR", "Unable to load texture xyz", trace=trace)), "failure")

    def test_unknown_warnings_are_triaged(self):
        self.assertEqual(rc.classify(entry("WARN", "Something new happened")), "unknown")

    def test_disconnect_reason(self):
        match = rc.DISCONNECT.search("Client disconnected with reason: Outdated server! I'm still on 1.20.1")
        self.assertEqual(match.group("reason"), "Outdated server! I'm still on 1.20.1")


class Wire(unittest.TestCase):
    def test_varints(self):
        for value in (0, 1, 127, 128, 255, 2097151, 2147483647, -1):
            encoded = rc.encode_varint(value)
            decoded, offset = rc.read_varint_from(encoded, 0)
            self.assertEqual(decoded, value & 0xFFFFFFFF)
            self.assertEqual(offset, len(encoded))
        self.assertEqual(rc.read_varint_from(b"\x80", 0), (None, 0))

    def test_reasons(self):
        text = json.dumps({"translate": "multiplayer.disconnect.outdated_client", "with": ["1.20.1"]})
        body = rc.encode_string(text)
        self.assertEqual(rc.decode_reason(body, 763, False), "multiplayer.disconnect.outdated_client ['1.20.1']")
        # NBT compound {"text": "Kicked", "extra": ["!"]} as network NBT (no root name).
        nbt = (b"\x0a" + b"\x08\x00\x04text\x00\x06Kicked"
               + b"\x09\x00\x05extra\x08\x00\x00\x00\x01\x00\x01!" + b"\x00")
        self.assertEqual(rc.decode_reason(nbt, 774, True), "Kicked!")
        self.assertEqual(rc.decode_reason(b"\x08\x00\x02hi", 774, True), "hi")

    def test_corrupt_payload(self):
        payload = rc.encode_varint(0x26) + b"\x00" * 10
        packet_id, _ = rc.read_varint_from(rc.corrupt_payload(payload, "badid"), 0)
        self.assertEqual(packet_id, 0x3FFF)
        flipped = rc.corrupt_payload(payload, "flip")
        self.assertEqual(flipped[:1], b"\x26")
        self.assertEqual(flipped[1:], b"\xff" * 10)


def frame(packet_id, body, compression=-1):
    payload = rc.encode_varint(packet_id) + body
    if compression >= 0:
        if len(payload) >= compression:
            payload = rc.encode_varint(len(payload)) + zlib.compress(payload)
        else:
            payload = rc.encode_varint(0) + payload
    return rc.encode_varint(len(payload)) + payload


BUFFERS = {}


def read_frame(sock, compression=-1):
    data = BUFFERS.get(sock, b"")
    while True:
        length, offset = rc.read_varint_from(data, 0)
        if length is not None and len(data) >= offset + length:
            payload = data[offset:offset + length]
            BUFFERS[sock] = data[offset + length:]
            break
        sock.settimeout(10)
        chunk = sock.recv(65536)
        if not chunk:
            raise EOFError
        data += chunk
    if compression >= 0:
        size, start = rc.read_varint_from(payload, 0)
        payload = payload[start:] if size == 0 else zlib.decompress(payload[start:])
    packet_id, start = rc.read_varint_from(payload, 0)
    return packet_id, payload[start:]


class RelayTest(unittest.TestCase):
    """A scripted 1.12.2-style (protocol 340) session through the relay."""

    def session(self, corrupt=None):
        server = socket.create_server(("127.0.0.1", 0))
        port = server.getsockname()[1]
        relay = rc.Relay("localhost", port, corrupt=corrupt, min_corrupt_size=4)
        relay.ready.set()
        seen = {}

        def backend():
            conn, _ = server.accept()
            with conn:
                seen["handshake"] = read_frame(conn)
                seen["login"] = read_frame(conn)
                conn.sendall(frame(0x03, rc.encode_varint(16)))  # set compression, threshold 16
                conn.sendall(frame(0x02, rc.encode_string("uuid") + rc.encode_string("Bot"), 16))
                conn.sendall(frame(0x23, b"\x00" * 30, 16))  # join game
                conn.sendall(frame(0x1F, (42).to_bytes(8, "big"), 16))  # keep-alive
                seen["reply"] = read_frame(conn, 16)
                if corrupt:
                    time.sleep(0.2)
                    conn.sendall(frame(0x0F, rc.encode_string("hello world, corrupt me"), 16))
                    time.sleep(0.2)
                conn.sendall(frame(0x1A, rc.encode_string(json.dumps({"text": "bye"})), 16))
                time.sleep(0.2)

        thread = threading.Thread(target=backend, daemon=True)
        thread.start()
        client = socket.create_connection(("127.0.0.1", relay.port))
        handshake = rc.encode_varint(340) + rc.encode_string("127.0.0.1") + relay.port.to_bytes(2, "big") \
            + rc.encode_varint(2)
        client.sendall(frame(0x00, handshake) + frame(0x00, rc.encode_string("Bot")))
        self.assertEqual(read_frame(client)[0], 0x03)
        self.assertEqual(read_frame(client, 16)[0], 0x02)
        self.assertEqual(read_frame(client, 16)[0], 0x23)
        packet_id, body = read_frame(client, 16)
        self.assertEqual(packet_id, 0x1F)
        relay.armed.set()
        client.sendall(frame(0x0B, body, 16))
        received = []
        try:
            while True:
                received.append(read_frame(client, 16))
        except (EOFError, OSError):
            pass
        thread.join(5)
        client.close()
        relay.close()
        server.close()
        return relay, seen, received

    def test_observes_join_keepalives_and_disconnect(self):
        relay, seen, received = self.session()
        self.assertEqual(relay.protocol, 340)
        self.assertIsNotNone(relay.joined_at)
        self.assertEqual((relay.keepalives, relay.answered), (1, 1))
        self.assertEqual(relay.disconnect, "play: bye")
        self.assertEqual(relay.closed_by, "server")
        # The backend sees the real target in the handshake.
        _, body = seen["handshake"]
        protocol, offset = rc.read_varint_from(body, 0)
        length, offset = rc.read_varint_from(body, offset)
        self.assertEqual(body[offset:offset + length], b"localhost")
        self.assertEqual(seen["reply"], (0x0B, (42).to_bytes(8, "big")))
        self.assertEqual(received[-1][0], 0x1A)

    def test_corrupts_one_play_packet(self):
        relay, _, received = self.session(corrupt="badid")
        self.assertIn("0x0f", relay.corrupted)
        self.assertEqual([packet_id for packet_id, _ in received], [0x3FFF, 0x1A])


if __name__ == "__main__":
    unittest.main()
