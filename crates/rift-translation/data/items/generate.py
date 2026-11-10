#!/usr/bin/env python3
"""Writes data/items/766.json: the default components of 1.20.5/1.20.6 items
that the item bridge (src/codec/item/bridge) needs, from the 1.20.6 data
generator's items report (`java -DbundlerMainClass=net.minecraft.data.Main
-jar server.jar --reports`, reports/items.json).

Usage: generate.py <path to the 1.20.6 reports/items.json>
"""
import json
import sys
from pathlib import Path


def main():
    report = json.loads(Path(sys.argv[1]).read_text())
    max_damage = {}
    for name, item in sorted(report.items()):
        for component in item["components"]:
            if component["type"] == "minecraft:max_damage":
                max_damage[name.removeprefix("minecraft:")] = component["value"]
    out = {
        "source": "1.20.6 data generator reports/items.json",
        "max_damage": max_damage,
    }
    target = Path(__file__).with_name("766.json")
    target.write_text(json.dumps(out, indent=1, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
