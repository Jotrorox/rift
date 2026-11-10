"""Pure-Python parts of the translation matrix: no network, Java or servers."""

import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import matrix_client  # noqa: E402
import matrix_env  # noqa: E402
import matrix_scenario  # noqa: E402
import protodef  # noqa: E402
import schema_patches  # noqa: E402
import translation_matrix  # noqa: E402


def protocol(packets, types=None):
    """A minimal protocol in minecraft-data's layout with the given packets."""
    names = list(packets)
    section = {"types": {"packet": ["container", [
        {"name": "name", "type": ["mapper", {"type": "varint", "mappings": {
            f"0x{index:02x}": name for index, name in enumerate(names)}}]},
        {"name": "params", "type": ["switch", {"compareTo": "name", "fields": {
            name: f"packet_{name}" for name in names}}]},
    ]]}}
    for name, definition in packets.items():
        section["types"][f"packet_{name}"] = definition
    base = {"string": ["pstring", {"countType": "varint"}], "varint": "native", "nbt": "native",
            "optionalNbt": "native", **(types or {})}
    return {"types": base, "play": {"toClient": section, "toServer": section}}


SLOT_1_12 = ["container", [{"name": "blockId", "type": "i16"}, {"anon": True, "type": ["switch", {
    "compareTo": "blockId", "fields": {"-1": "void"}, "default": ["container", [
        {"name": "itemCount", "type": "i8"}, {"name": "itemDamage", "type": "i16"},
        {"name": "nbtData", "type": "optionalNbt"}]]}]}]]
SET_SLOT = ["container", [{"name": "windowId", "type": "i8"}, {"name": "slot", "type": "i16"},
                          {"name": "item", "type": "slot"}]]


class StrictDecoderTests(unittest.TestCase):
    def setUp(self):
        schema = protocol({"set_slot": SET_SLOT}, {"slot": SLOT_1_12})
        self.decoder = protodef.Decoder(schema, "play", "clientbound")
        self.encoder = protodef.Encoder(schema, "play", "serverbound")
        nbt = b"\x0a\x00\x00" + b"\x03\x00\x01a" + struct.pack(">i", 7) + b"\x00"  # Named root.
        self.body = b"\x00" + struct.pack(">h", 36) + struct.pack(">hbh", 276, 1, 0) + nbt

    def test_named_nbt_slot_decodes(self):
        name, value = self.decoder.packet(0, self.body)
        self.assertEqual(name, "set_slot")
        self.assertEqual(value["item"], {"blockId": 276, "itemCount": 1, "itemDamage": 0, "nbtData": {"a": 7}})

    def test_corrupted_packets_are_caught(self):
        with self.assertRaisesRegex(protodef.DecodeError, "trailing bytes"):
            self.decoder.packet(0, self.body + b"\x00")
        with self.assertRaisesRegex(protodef.DecodeError, r"short packet \(in .*item"):
            self.decoder.packet(0, self.body[:-3])
        with self.assertRaisesRegex(protodef.DecodeError, "invalid NBT tag"):
            self.decoder.packet(0, self.body[:8] + b"\x0e" + self.body[9:])
        with self.assertRaisesRegex(protodef.DecodeError, "unknown packet ID 0x5"):
            self.decoder.packet(5, self.body)
        try:
            self.decoder.packet(0, self.body[:-3])
        except protodef.DecodeError as error:
            self.assertIn("item", error.path[-2:][0] + error.path[-1])
            self.assertIsNotNone(error.offset)

    def test_encoder_defaults_and_round_trip(self):
        packet_id, body = self.encoder.packet("set_slot", {"slot": 36, "item": {"blockId": -1}})
        self.assertEqual((packet_id, body), (0, b"\x00\x00\x24\xff\xff"))
        _, body = self.encoder.packet("set_slot", {"slot": 1, "item": {"blockId": 1, "itemCount": 2}})
        self.assertEqual(self.decoder.packet(0, body)[1]["item"],
                         {"blockId": 1, "itemCount": 2, "itemDamage": 0, "nbtData": None})

    def test_parameterised_types_and_fixed_buffers(self):
        schema = protocol({"p": ["container", [
            {"name": "kind", "type": "varint"},
            {"name": "data", "type": ["particleData", {"compareTo": "kind"}]},
            {"name": "ack", "type": ["buffer", {"count": 3}]}]]},
            {"particleData": ["switch", {"compareTo": "$compareTo", "fields": {"3": "varint"}, "default": "void"}]})
        decoder = protodef.Decoder(schema, "play", "clientbound")
        self.assertEqual(decoder.packet(0, b"\x03\x07abc")[1], {"kind": 3, "data": 7, "ack": b"abc"})
        self.assertEqual(decoder.packet(0, b"\x01xyz")[1]["data"], None)
        encoder = protodef.Encoder(schema, "play", "serverbound")
        self.assertEqual(encoder.packet("p", {"kind": 3, "data": 9})[1], b"\x03\x09\x00\x00\x00")

    def test_unverified_layouts_are_reported_as_such(self):
        schema = protocol({"p": ["container", [{"name": "x", "type": "Odd"}]]},
                          {"Odd": schema_patches.unverified("changed in 26.3")})
        with self.assertRaises(protodef.Unverified):
            protodef.Decoder(schema, "play", "clientbound").packet(0, b"\x01")

    def test_lp_vec3_and_movement_delta(self):
        self.assertEqual(protodef.lp_vec3(protodef.Reader(b"\x00")), [0.0, 0.0, 0.0])
        linear = protodef.Reader(b"\x01" + struct.pack(">hhh", 4096, 0, -4096))
        self.assertEqual(schema_patches.movement_delta(linear), {"onGround": True, "dX": 4096, "dY": 0, "dZ": -4096})
        stepped = protodef.Reader(b"\x04" + b"\x02" + struct.pack(">hhh", 1, 2, 3) + b"\x03" + struct.pack(">hhh", 4, 5, 6))
        value = schema_patches.movement_delta(stepped)
        self.assertEqual([step["ticks"] for step in value["steps"]], [2, 3])
        self.assertFalse(value["onGround"])

    def test_corrections_patch_by_field_name_and_fail_loudly(self):
        document = {"types": {"p": ["container", [{"name": "a", "type": "bool"}]]}}
        protodef.patch(document, ["types", "p", 1, {"field": "a"}, "type"], "u8")
        self.assertEqual(document["types"]["p"][1][0]["type"], "u8")
        with self.assertRaisesRegex(KeyError, "missing"):
            protodef.patch(document, ["types", "p", 1, {"field": "b"}, "type"], "u8")
        for entry in protodef.CORRECTIONS:
            self.assertTrue(entry["why"] and entry["protocols"][0] <= entry["protocols"][1])

    def test_every_release_has_a_pinned_or_derived_schema(self):
        from matrix_fixtures import RELEASES
        pinned = protodef.versions()
        for number, name in RELEASES:
            self.assertTrue(number in pinned or number in schema_patches.DERIVED, name)


class ClientHelperTests(unittest.TestCase):
    def test_text_components(self):
        self.assertEqual(matrix_client.text_of('{"text":"a","extra":[{"text":"b"}]}'), "ab")
        self.assertEqual(matrix_client.text_of({"": "x"}), "x")
        self.assertEqual(matrix_client.text_of(["a", {"text": "b"}]), "ab")

    def test_logs_and_dumps(self):
        self.assertIn("....", matrix_client.hexdump(b"\x00\x01\x02\x03"))
        self.assertTrue(matrix_client.jsonable(b"x" * 600, 8).endswith("(600 bytes)"))
        self.assertEqual(str(matrix_client.offline_uuid("Notch")), "b50ad385-829d-3141-a216-7e7d7539ba7f")


class CommandTableTests(unittest.TestCase):
    def test_give_and_setblock_syntax_by_era(self):
        sword = matrix_scenario.ITEMS[0]
        self.assertTrue(matrix_scenario.Commands(340, "vanilla").give("P", sword).startswith(
            "give P minecraft:diamond_sword 1 0 {ench:"))
        self.assertIn('Name:"{\\"text\\": \\"Rift Sword\\"}"', matrix_scenario.Commands(393, "vanilla").give("P", sword))
        self.assertIn("enchantments={levels:", matrix_scenario.Commands(766, "vanilla").give("P", sword))
        self.assertIn('custom_name="Rift Sword"', matrix_scenario.Commands(770, "vanilla").give("P", sword))
        chest = next(block for name, *_, block in matrix_scenario.BLOCKS if name == "chest")
        self.assertIn("count:3", matrix_scenario.Commands(774, "vanilla").setblock(1, 2, 3, chest))
        self.assertEqual(matrix_scenario.Commands(774, "vanilla").gamerule("doMobSpawning", "spawn_mobs", "false"),
                         "gamerule spawn_mobs false")

    def test_views_parse_multi_block_changes(self):
        class Registries:
            block = staticmethod(lambda state: f"state{state}")
        record = type("R", (), {"name": "multi_block_change", "value": {
            "chunkCoordinates": {"x": 1, "z": -1, "y": 0}, "records": [(9 << 12) | (2 << 8) | (3 << 4) | 4]}})()
        session = mock.Mock()
        session.packets.return_value = [record]
        self.assertEqual(matrix_scenario.blocks_seen(session, Registries, 0), {(18, 4, -13): "state9"})


class MatrixTests(unittest.TestCase):
    def test_predefined_matrices(self):
        matrices = translation_matrix.matrices()
        count = lambda name: sum(len(clients) for clients in matrices[name].values())
        releases = len(translation_matrix.fixtures.RELEASES)
        self.assertEqual(count("adjacent"), 2 * (releases - 1))
        self.assertEqual(count("anchors"), 25)
        self.assertEqual(count("full"), releases * releases)
        for server in matrices["software"]:
            translation_matrix.fixtures.resolve(server)

    def test_versions_and_steps(self):
        self.assertEqual(translation_matrix.client_protocol("1.16.5"), 754)
        self.assertEqual(translation_matrix.client_protocol("1.16.4"), 754)
        self.assertEqual(translation_matrix.client_protocol("777"), 777)
        names = [step.name for step in translation_matrix.selected_steps("items")]
        self.assertEqual(names, ["teleport", "items"])
        self.assertEqual({step.name for step in translation_matrix.selected_steps("join")}, set(translation_matrix.ALWAYS))
        with self.assertRaises(SystemExit):
            translation_matrix.selected_steps("nonsense")

    def test_step_applicability(self):
        boss = next(step for step in matrix_scenario.STEPS if step.name == "boss_bar")
        self.assertIsNotNone(boss.applies(340, 774, "vanilla"))
        self.assertIsNone(boss.applies(774, 340, "vanilla"))
        blocks = next(step for step in matrix_scenario.STEPS if step.name == "blocks")
        self.assertIn("pumpkin", blocks.applies(777, 777, "pumpkin"))


class EnvironmentTests(unittest.TestCase):
    def setUp(self):
        self.home = tempfile.TemporaryDirectory()
        self.patches = [mock.patch.object(matrix_env, "HOME", Path(self.home.name)),
                        mock.patch.object(matrix_env, "LOCKS", Path(self.home.name) / "locks")]
        for patch in self.patches:
            patch.start()

    def tearDown(self):
        for patch in self.patches:
            patch.stop()
        self.home.cleanup()

    def test_locks_exclude_other_processes(self):
        lock = matrix_env.Lock("thing")
        self.assertTrue(lock.try_acquire())
        script = ("import fcntl, os, sys; fd = os.open(sys.argv[1], os.O_RDWR)\n"
                  "try: fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB); print('got')\n"
                  "except OSError: print('busy')")
        check = lambda: subprocess.run([sys.executable, "-c", script, str(lock.path)],
                                       capture_output=True, text=True).stdout.strip()
        self.assertEqual(check(), "busy")
        lock.release()
        self.assertEqual(check(), "got")

    def test_server_slots_are_limited(self):
        with mock.patch.object(matrix_env, "MAX_SERVERS", 2):
            first = matrix_env.acquire_server_slot("a")
            second = matrix_env.acquire_server_slot("b")
            with self.assertRaises(TimeoutError):
                matrix_env.acquire_server_slot("c", timeout=0.5)
            first.release()
            third = matrix_env.acquire_server_slot("c", timeout=2)
            second.release()
            third.release()

    def test_ports_are_unique_and_reserved(self):
        ports = {matrix_env.allocate_port() for _ in range(20)}
        self.assertEqual(len(ports), 20)
        for port in ports:
            self.assertFalse(matrix_env.Lock(f"ports/{port}").try_acquire())
            matrix_env.release_port(port)

    def test_fetch_verifies_checksums(self):
        import hashlib
        source = Path(self.home.name) / "source"
        source.write_bytes(b"jar")
        target = Path(self.home.name) / "servers" / "a.jar"
        digest = hashlib.sha256(b"jar").hexdigest()
        self.assertEqual(matrix_env.fetch(source.as_uri(), target, "sha256", digest).read_bytes(), b"jar")
        target.unlink()
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            matrix_env.fetch(source.as_uri(), target, "sha256", "0" * 64)
        self.assertFalse(target.exists())

    def test_processes_stop_whole_groups(self):
        log = Path(self.home.name) / "log"
        processes = matrix_env.Processes()
        proc = processes.spawn([sys.executable, "-c", "import subprocess, sys, time; "
                                "subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)']); "
                                "time.sleep(60)"], self.home.name, log)
        group = proc.pid
        processes.stop(proc, grace=2)
        with self.assertRaises(ProcessLookupError):
            os.killpg(group, 0)


if __name__ == "__main__":
    unittest.main()
