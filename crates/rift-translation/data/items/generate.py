#!/usr/bin/env python3
"""Writes data/items/766.json: the default components of 1.20.5/1.20.6 items
that the item bridge (src/codec/item/bridge) needs, from the 1.20.6 data
generator's items report (`java -DbundlerMainClass=net.minecraft.data.Main
-jar server.jar --reports`, reports/items.json):

- `max_damage`: items that wear out (1.20.4 stacks of these always carry
  `Damage`);
- `attribute_modifiers`: items whose prototype sets modifiers (weapons and
  tools), as [attribute, uuid, name, amount, operation, slot] rows, for
  `HideFlags` bit 2 on a stack without `AttributeModifiers`.

Usage: generate.py <path to the 1.20.6 reports/items.json>
"""
import json
import sys
from pathlib import Path

OPERATIONS = ["add_value", "add_multiplied_base", "add_multiplied_total"]


def strip(name):
    return name.removeprefix("minecraft:")


def main():
    report = json.loads(Path(sys.argv[1]).read_text())
    max_damage, modifiers = {}, {}
    for name, item in sorted(report.items()):
        for component in item["components"]:
            if component["type"] == "minecraft:max_damage":
                max_damage[strip(name)] = component["value"]
            elif component["type"] == "minecraft:attribute_modifiers":
                rows = [
                    [strip(m["type"]), m["uuid"], m["name"], m["amount"],
                     OPERATIONS.index(m["operation"]), m.get("slot", "any")]
                    for m in component["value"]["modifiers"]
                ]
                if rows:
                    modifiers[strip(name)] = rows
    out = {
        "source": "1.20.6 data generator reports/items.json",
        "max_damage": max_damage,
        "attribute_modifiers": modifiers,
    }
    target = Path(__file__).with_name("766.json")
    target.write_text(json.dumps(out, indent=1, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
