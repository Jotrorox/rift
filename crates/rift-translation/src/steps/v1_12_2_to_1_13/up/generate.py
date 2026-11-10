"""Generate the data files of the 1.12.2 -> 1.13 upgrade step.

    /opt/homebrew/bin/python3 crates/rift-translation/src/steps/v1_12_2_to_1_13/up/generate.py

Inputs are Mojang's own artifacts only: the official 1.12.2 and 1.13 server
jars (``~/.cache/rift-via/servers``) and the 1.13 vanilla datapack extracted
from its jar (``~/.cache/rift-via/reports/1.13/datapack``, or the jar itself).

* ``recipes.json``: 1.12.2's crafting recipes in the order the 1.12.2 game
  numbers them (its 11 built-in special recipes first, then the JSON recipes
  in the order ``Files.walk`` lists them inside the jar, which a JDK reports;
  the 1.12.2 server and client both number recipes this way), with items
  still in 1.12 terms: ``name:damage``, or ``name:*`` for any damage value.
* ``tags.json``: the complete resolved vanilla 1.13 block, item and fluid
  tags, as entry names.
* ``translations.json``: 1.12.2 translation keys whose 1.13 key differs,
  matched by identical English text in the two versions' ``en_us`` files
  (``.lang`` in 1.12.2, ``.json`` in 1.13). A text used by several 1.13
  keys pairs only with the one in the matching key family (``tile.*`` to
  ``block.*``, ...) or, failing that, with the last segment in common, so
  that generic words do not swap meaning; otherwise the key stays as it is.
"""
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import zipfile

HERE = pathlib.Path(__file__).resolve().parent
CACHE = pathlib.Path.home() / ".cache/rift-via"
OLD_JAR = CACHE / "servers/vanilla-1.12.2.jar"
NEW_JAR = CACHE / "servers/vanilla-1.13.jar"
JDK = CACHE / "jdks/jdk8/bin"

# 1.12.2 registers these before any JSON recipe (CraftingManager.init).
SPECIAL = [
    "armordye", "bookcloning", "mapcloning", "mapextending", "fireworks",
    "repairitem", "tippedarrow", "bannerduplicate", "banneraddpattern",
    "shielddecoration", "shulkerboxcoloring",
]

WALK = """
import java.net.URI;
import java.nio.file.*;
import java.util.*;
public class Walk {
    public static void main(String[] a) throws Exception {
        URI uri = URI.create("jar:" + Paths.get(a[0]).toUri());
        try (FileSystem fs = FileSystems.newFileSystem(uri, Collections.<String, Object>emptyMap())) {
            Path root = fs.getPath("/assets/minecraft/recipes");
            Iterator<Path> it = Files.walk(root).iterator();
            while (it.hasNext()) {
                Path p = it.next();
                if (p.toString().endsWith(".json")) System.out.println(root.relativize(p));
            }
        }
    }
}
"""


def walk_order(jar):
    with tempfile.TemporaryDirectory() as tmp:
        source = pathlib.Path(tmp) / "Walk.java"
        source.write_text(WALK)
        subprocess.run([str(JDK / "javac"), "-d", tmp, str(source)], check=True)
        out = subprocess.run(
            [str(JDK / "java"), "-cp", tmp, "Walk", str(jar)],
            check=True, capture_output=True, text=True,
        ).stdout
    return [line[: -len(".json")] for line in out.split()]


def item(value, wildcard):
    name = value["item"].removeprefix("minecraft:")
    if "data" in value:
        return f"{name}:{value['data']}"
    return f"{name}:*" if wildcard else f"{name}:0"


def ingredient(value):
    values = value if isinstance(value, list) else [value]
    return [item(entry, True) for entry in values]


def recipes():
    order = walk_order(OLD_JAR)
    out = [{"name": name, "type": "special"} for name in SPECIAL]
    with zipfile.ZipFile(OLD_JAR) as jar:
        for name in order:
            raw = json.loads(jar.read(f"assets/minecraft/recipes/{name}.json"))
            result = raw["result"]
            recipe = {
                "name": name,
                "type": raw["type"],
                "group": raw.get("group", ""),
                "result": [item(result, False), result.get("count", 1)],
            }
            if raw["type"] == "crafting_shaped":
                pattern = raw["pattern"]
                recipe["width"] = max(len(row) for row in pattern)
                recipe["height"] = len(pattern)
                recipe["ingredients"] = [
                    ingredient(raw["key"][symbol]) if symbol != " " else []
                    for row in pattern
                    for symbol in row.ljust(recipe["width"])
                ]
            elif raw["type"] == "crafting_shapeless":
                recipe["ingredients"] = [ingredient(entry) for entry in raw["ingredients"]]
            else:
                sys.exit(f"unknown 1.12.2 recipe type {raw['type']} in {name}")
            out.append(recipe)
    return out


def tags():
    datapack = CACHE / "reports/1.13/datapack/tags"
    files = {}
    if datapack.is_dir():
        for path in datapack.rglob("*.json"):
            relative = path.relative_to(datapack).with_suffix("")
            files[str(relative)] = json.loads(path.read_text())
    else:
        with zipfile.ZipFile(NEW_JAR) as jar:
            for entry in jar.namelist():
                if entry.startswith("data/minecraft/tags/") and entry.endswith(".json"):
                    relative = entry[len("data/minecraft/tags/"): -len(".json")]
                    files[relative] = json.loads(jar.read(entry))
    out = {}
    for kind in ("blocks", "items", "fluids"):
        raw = {
            name.split("/", 1)[1]: value
            for name, value in files.items()
            if name.startswith(kind + "/")
        }

        def resolve(name, seen=()):
            if name in seen:
                sys.exit(f"tag cycle at {kind}/{name}")
            entries = []
            for value in raw[name]["values"]:
                if value.startswith("#"):
                    entries += resolve(value[1:].removeprefix("minecraft:"), seen + (name,))
                else:
                    entries.append(value.removeprefix("minecraft:"))
            return list(dict.fromkeys(entries))

        out[kind] = {name: resolve(name) for name in sorted(raw)}
    return out


# Where a 1.12 key family's texts went in 1.13, to pick among keys that
# share an English text.
FAMILIES = {
    "tile": ("block",),
    "item": ("item", "block"),
    "entity": ("entity",),
    "enchantment": ("enchantment",),
    "effect": ("effect",),
    "potion": ("item", "effect"),
    "biome": ("biome",),
    "stat": ("stat", "stat_type"),
    "container": ("container",),
}


def translations():
    with zipfile.ZipFile(OLD_JAR) as jar:
        lines = jar.read("assets/minecraft/lang/en_us.lang").decode("utf-8").splitlines()
    old = {}
    for line in lines:
        if "=" in line and not line.startswith("#"):
            key, text = line.split("=", 1)
            old[key] = text
    with zipfile.ZipFile(NEW_JAR) as jar:
        new = json.loads(jar.read("assets/minecraft/lang/en_us.json"))
    by_text = {}
    for key, text in new.items():
        by_text.setdefault(text, []).append(key)
    out = {}
    for key, text in old.items():
        if key in new:
            continue
        # 1.12 formats with %s/%d, 1.13 with %s only.
        normalised = text.replace("%d", "%s").replace("%1$d", "%1$s").replace("%2$d", "%2$s")
        candidates = by_text.get(text, []) or by_text.get(normalised, [])
        candidates = [candidate for candidate in candidates if candidate not in old]
        if len(candidates) > 1:
            families = FAMILIES.get(key.split(".", 1)[0], ())
            candidates = [c for c in candidates if c.split(".", 1)[0] in families] or candidates
        if len(candidates) > 1:
            tail = key.rsplit(".", 1)[-1].lower()
            candidates = [c for c in candidates if c.rsplit(".", 1)[-1].lower() == tail]
        if len(candidates) == 1:
            out[key] = candidates[0]
    return out


def write(name, value):
    text = json.dumps(value, separators=(",", ":"), sort_keys=name != "recipes.json")
    (HERE / name).write_text(text + "\n")
    print(f"{name}: {len(value)} entries, {len(text)} bytes")


def main():
    for jar in (OLD_JAR, NEW_JAR):
        if not jar.is_file():
            sys.exit(f"missing {jar}")
    write("recipes.json", recipes())
    write("tags.json", tags())
    write("translations.json", translations())


if __name__ == "__main__":
    main()
