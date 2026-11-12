"""Generate down_recipes.json: 1.12.2's numeric recipe IDs and 1.13's names.

    python3 down_recipes.py <vanilla-1.12.2.jar> <vanilla-1.13.jar> <ids.txt> > down_recipes.json

1.12.2 numbers recipes in registration order: eleven special recipes the
game registers in code, then the JSON recipes in the order Java 8's zip
file system walks `assets/minecraft/recipes` in the jar (`ids.txt`, one
name per line, from `Files.walk` over the jar). 1.13 names recipes; most
names carry over, the rest are matched by result item (through the
flattening tables in `data/legacy.json`) and, where several share a
result, by ingredients. Smelting recipes are new in the 1.13 recipe book
and have no 1.12 counterpart.
"""
import json, sys, zipfile
from pathlib import Path

crate = Path(__file__).resolve().parents[4]
old_jar, new_jar, order = zipfile.ZipFile(sys.argv[1]), zipfile.ZipFile(sys.argv[2]), sys.argv[3]
SPECIAL = ["armordye", "bookcloning", "mapcloning", "mapextending", "fireworks", "repairitem",
           "tippedarrow", "bannerduplicate", "banneraddpattern", "shielddecoration",
           "shulkerboxcoloring"]
ids = SPECIAL + [line.strip() for line in open(order) if line.strip()]
legacy = json.load(open(crate / "data/legacy.json"))
old_items = json.load(open(crate / "data/340.json"))["registries"]["item"]
item_ids = {name: index for index, name in enumerate(old_items) if name}

def strip(name):
    return name.removeprefix("minecraft:")

def flat(entry):
    """A 1.12 item reference as its 1.13 item name."""
    name, data = strip(entry["item"]), entry.get("data", 0)
    key = f"{item_ids[name]}:{data}"
    return legacy["items"].get(key) or legacy["items"].get(f"{item_ids[name]}:0") or name

def load(jar, prefix):
    out = {}
    for info in jar.infolist():
        if info.filename.startswith(prefix) and info.filename.endswith(".json"):
            out[info.filename[len(prefix):-5]] = json.loads(jar.read(info))
    return out

old = load(old_jar, "assets/minecraft/recipes/")
new = load(new_jar, "data/minecraft/recipes/")
assert sorted(old) == sorted(ids[len(SPECIAL):]), "ids.txt lists every 1.12 recipe once"

def ingredients_old(recipe):
    raw = list(recipe.get("key", {}).values()) + recipe.get("ingredients", [])
    out = set()
    for entry in raw:
        for choice in entry if isinstance(entry, list) else [entry]:
            out.add(flat(choice))
    return out

def ingredients_new(recipe):
    raw = list(recipe.get("key", {}).values()) + recipe.get("ingredients", [])
    out = set()
    for entry in raw:
        for choice in entry if isinstance(entry, list) else [entry]:
            out.add(strip(choice.get("item") or "#" + choice["tag"]))
    return out

def result_new(recipe):
    result = recipe["result"]
    return strip(result if isinstance(result, str) else result["item"])

crafting = {name: r for name, r in new.items()
            if r["type"] in ("crafting_shaped", "crafting_shapeless")}
same = {name for name in old
        if name in crafting and flat(old[name]["result"]) == result_new(crafting[name])}
renames = {}
unmatched_old = set(old) - same
unmatched_new = set(crafting) - same


def match(score):
    """Pair each unmatched 1.12 recipe with its best-scoring 1.13 one."""
    for name in sorted(unmatched_old):
        scored = [(score(name, other), other) for other in sorted(unmatched_new)]
        scored = [entry for entry in scored if entry[0] > 0]
        if scored:
            best = max(scored)[1]
            renames[best] = name
            unmatched_new.discard(best)
            unmatched_old.discard(name)


def overlap(name, other):
    return len(ingredients_old(old[name]) & ingredients_new(crafting[other]))


# Same result first, ties broken by shared ingredients; then shared
# ingredients alone (a recipe whose result 1.13 changed, such as the
# shulker box).
match(lambda name, other: (flat(old[name]["result"]) == result_new(crafting[other]))
      * (1 + overlap(name, other)))
match(lambda name, other: overlap(name, other)
      if ingredients_old(old[name]) == ingredients_new(crafting[other]) else 0)
assert not unmatched_old, f"1.12 recipes without a 1.13 counterpart: {sorted(unmatched_old)}"
json.dump({"ids": ids, "renames": dict(sorted(renames.items()))}, sys.stdout, indent=1)
print()
print(f"{len(ids)} ids, {len(renames)} renamed, 1.13 crafting recipes without a 1.12 one: "
      f"{sorted(unmatched_new)}", file=sys.stderr)
