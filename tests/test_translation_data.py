"""Check how translation data is derived from Mojang's registry reports."""

import unittest
from unittest.mock import patch

from scripts import translation_data


class RegistryDiffTests(unittest.TestCase):
    def diff(self, old, new, fallbacks):
        with patch.dict(translation_data.FALLBACKS, {"minecraft:test": fallbacks}):
            return translation_data.registry_diff("minecraft:test", old, new)

    def test_insertions_and_removals_keep_order_and_name_stand_ins(self):
        diff = self.diff(["a", "b", "c", "d"], ["a", "x", "b", "d"], {"x": "d", "c": "b"})
        self.assertEqual(diff, {
            "old_size": 4,
            "new_size": 4,
            "inserted": [[1, "x", 3, "d"]],
            "removed": [[2, "c", 2, "b"]],
        })

    def test_reordering_and_missing_stand_ins_are_rejected(self):
        with self.assertRaisesRegex(ValueError, "reordered"):
            self.diff(["a", "b"], ["b", "a"], {})
        with self.assertRaisesRegex(ValueError, "FALLBACKS"):
            self.diff(["a"], ["a", "x"], {})
        with self.assertRaisesRegex(ValueError, "does not exist in the older version"):
            self.diff(["a"], ["a", "x"], {"x": "x"})

    def test_registries_must_be_dense(self):
        registry = {"entries": {"a": {"protocol_id": 0}, "b": {"protocol_id": 2}}}
        with self.assertRaisesRegex(ValueError, "dense"):
            translation_data.ordered(registry)


class PacketDiffTests(unittest.TestCase):
    def test_only_changed_packet_ids_are_listed(self):
        packet = lambda **ids: {"play": {"clientbound": {
            name: {"protocol_id": value} for name, value in ids.items()}, "serverbound": {}}}
        self.assertEqual(translation_data.packet_diff(packet(a=0, b=1), packet(a=0, b=1)), {})
        self.assertEqual(
            translation_data.packet_diff(packet(a=0, b=1), packet(a=0, c=1, b=2)),
            {"play/clientbound": {"b": [1, 2], "c": [None, 1]}})


if __name__ == "__main__":
    unittest.main()
