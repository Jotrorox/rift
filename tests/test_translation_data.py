"""Check how translation data is derived from Mojang's registry reports."""

import unittest
from unittest.mock import patch

from scripts import translation_crate, translation_data


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


class CrateRustTablesTests(unittest.TestCase):
    """The reader of Rift's hard-coded packet ID tables."""

    SOURCE = """
        // A comment with => and { braces }.
        const BASE: Caps = Caps { join: 0x10, chat: 2, flag: true };
        const NEXT: Caps = Caps { chat: 3, ..BASE };
        fn caps(number: i32) -> Option<Caps> {
            Some(match number {
                1 | 2 => NEXT,
                5..=7 => Caps { join: if number >= 6 { 0x20 } else { 0x21 }, ..BASE },
                _ => return None,
            })
        }
        fn name() -> &'static str { "match 5 => 9" }
    """

    def test_matches_constants_and_conditions_evaluate_per_protocol(self):
        tables = translation_crate.RustTables(self.SOURCE)
        for name in ("BASE", "NEXT"):
            tables.find("const", name, ":", "Caps", "=")
            tables.constants[name] = tables.expression()
        tables.find("fn", "caps")
        while tables.peek() != "match":
            tables.take()
        caps = tables.expression()
        self.assertEqual(caps(2), {"join": 0x10, "chat": 3, "flag": True})
        self.assertEqual(caps(5)["join"], 0x21)
        self.assertEqual(caps(7), {"join": 0x20, "chat": 2, "flag": True})
        self.assertIsNone(caps(3))

    def test_unknown_forms_fail_loudly(self):
        tables = translation_crate.RustTables("fn f() { match number { 1 => x + 1 } }")
        tables.find("fn", "f")
        while tables.peek() != "match":
            tables.take()
        with self.assertRaisesRegex(ValueError, "expected"):
            tables.expression()


class CrateDataTests(unittest.TestCase):
    def test_registries_keep_gaps_and_reject_repeated_ids(self):
        self.assertEqual(translation_crate.dense([("a", 0), ("b", 2)], "test"), ["a", None, "b"])
        with self.assertRaisesRegex(ValueError, "both"):
            translation_crate.dense([("a", 1), ("b", 1)], "test")

    def test_packet_renames_apply_per_table(self):
        renamed = translation_crate.renamed({
            "login/clientbound": ["login_disconnect", "hello", "game_profile"],
            "play/serverbound": ["resource_pack"],
        })
        self.assertEqual(renamed["login/clientbound"][2], "login_finished")
        # Only clientbound resource packs became resource_pack_push.
        self.assertEqual(renamed["play/serverbound"], ["resource_pack"])
        with self.assertRaisesRegex(ValueError, "repeats"):
            translation_crate.check_packet_names({1: {"play/clientbound": ["a", "a"]}})

    def test_block_states_number_properties_in_whichever_order_matches(self):
        # Listed type-first (as 1.19 reports do) but numbered by sorted name.
        listed = [("type", ["normal", "sticky"]), ("facing", ["north", "south"])]
        states = [{"id": 10 + facing * 2 + kind, "default": facing == kind == 0,
                   "properties": {"type": ["normal", "sticky"][kind],
                                  "facing": ["north", "south"][facing]}}
                  for facing in range(2) for kind in range(2)]
        self.assertIsNone(translation_crate.numbered(listed, states, 10))
        self.assertEqual(translation_crate.numbered(sorted(listed), states, 10), 0)

    def test_minecraft_data_effect_names_become_registry_names(self):
        rename = lambda name: translation_crate.MCDATA_EFFECTS.get(
            translation_crate.snake(name), translation_crate.snake(name))
        self.assertEqual(rename("JumpBoost"), "jump_boost")
        self.assertEqual(rename("BadLuck"), "unluck")


class CrateLegacyTests(unittest.TestCase):
    BLOCKS = {"oak_door": ([("facing", ["north", "south"]), ("open", ["true", "false"])], 1)}

    def test_states_fill_defaults_and_reject_unknown_values(self):
        name, properties = translation_crate.split_state("minecraft:oak_door[facing=south]")
        self.assertEqual(translation_crate.state_values(self.BLOCKS, name, properties),
                         ["south", "false"])
        with self.assertRaisesRegex(ValueError, "no open=maybe"):
            translation_crate.state_values(self.BLOCKS, "oak_door", {"open": "maybe"})
        with self.assertRaisesRegex(ValueError, "no properties"):
            translation_crate.state_values(self.BLOCKS, "oak_door", {"hinge": "left"})

    def test_variants_kept_in_block_entities_share_a_source(self):
        families = translation_crate.legacy_families(["potted_red_tulip", "cow_spawn_egg"])
        self.assertEqual(families["white_bed"], "red_bed")
        self.assertEqual(families["zombie_wall_head"], "skeleton_wall_skull")
        self.assertEqual(families["spruce_button"], "oak_button")
        self.assertEqual(families["potted_red_tulip"], "flower_pot")
        self.assertEqual(families["cow_spawn_egg"], "pig_spawn_egg")


if __name__ == "__main__":
    unittest.main()
