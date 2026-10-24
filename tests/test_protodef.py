"""Check the minecraft-data schema interpreter used as a translation oracle."""

import struct
import unittest

from tests import protodef


def protocol(packet):
    """A one-packet protocol in minecraft-data's layout."""
    section = {"types": {"packet": ["container", [
        {"name": "name", "type": ["mapper", {"type": "varint", "mappings": {"0x00": "test"}}]},
        {"name": "params", "type": ["switch", {"compareTo": "name", "fields": {"test": "packet_test"}}]},
    ]], "packet_test": packet}}
    return {"types": {"string": ["pstring", {"countType": "varint"}]},
            "play": {"toClient": section, "toServer": section}}


def decode(packet, body):
    return protodef.Decoder(protocol(packet), "play", "clientbound").packet(0, body)[1]


class DecoderTests(unittest.TestCase):
    def test_containers_switches_options_and_counted_arrays(self):
        packet = ["container", [
            {"name": "kind", "type": "varint"},
            {"name": "value", "type": ["switch", {"compareTo": "kind", "fields": {
                "1": "i32"}, "default": "void"}]},
            {"name": "label", "type": ["option", "string"]},
            {"name": "count", "type": "u8"},
            {"name": "items", "type": ["array", {"count": "count", "type": "bool"}]},
        ]]
        body = b"\x01" + struct.pack(">i", -5) + b"\x01\x02hi" + b"\x02\x01\x00"
        self.assertEqual(decode(packet, body),
                         {"kind": 1, "value": -5, "label": "hi", "count": 2, "items": [True, False]})

    def test_registry_holders_and_holder_sets(self):
        holder = ["registryEntryHolder", {"baseName": "id", "otherwise": {"name": "inline", "type": "string"}}]
        holder_set = ["registryEntryHolderSet", {"base": {"name": "tag", "type": "string"},
                                                 "otherwise": {"name": "ids", "type": "varint"}}]
        packet = ["container", [{"name": "a", "type": holder}, {"name": "b", "type": holder},
                                {"name": "c", "type": holder_set}, {"name": "d", "type": holder_set}]]
        body = b"\x05" + b"\x00\x01x" + b"\x03\x07\x08" + b"\x00\x01t"
        self.assertEqual(decode(packet, body), {"a": {"id": 4}, "b": {"inline": "x"},
                                                "c": {"ids": [7, 8]}, "d": {"tag": "t"}})

    def test_network_nbt_and_metadata_loops(self):
        entry = ["container", [{"name": "key", "type": "u8"}, {"name": "value", "type": "anonymousNbt"}]]
        packet = ["entityMetadataLoop", {"endVal": 255, "type": entry}]
        compound = b"\x0a" + b"\x08\x00\x01k\x00\x01v" + b"\x00"
        self.assertEqual(decode(packet, b"\x03" + compound + b"\xff"), [{"key": 3, "value": {"k": "v"}}])

    def test_hooks_see_named_types_and_trailing_bytes_fail(self):
        seen = []
        decoder = protodef.Decoder(protocol(["container", [{"name": "x", "type": "string"}]]),
                                   "play", "clientbound")
        decoder.hooks["string"] = seen.append
        self.assertEqual(decoder.packet(0, b"\x01a")[1], {"x": "a"})
        self.assertEqual(seen, ["a"])
        with self.assertRaisesRegex(protodef.DecodeError, "trailing"):
            decoder.packet(0, b"\x01ab")
        with self.assertRaises(protodef.DecodeError):
            decoder.packet(0, b"\x05a")


if __name__ == "__main__":
    unittest.main()
