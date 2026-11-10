"""Generate down_recipe_results.json: the result item of each 1.13 crafting
recipe that 1.12.2 numbers (down_recipes.json).

    python3 down_recipe_results.py <vanilla-1.13.jar> > down_recipe_results.json

A backend behind later steps may name its recipes differently from vanilla
1.13 (1.21.2 dropped recipe names from the protocol; plugins add their own):
the 1.12.2 client is then told about the 1.12.2 recipes that make the same
item.
"""
import json, sys, zipfile
from pathlib import Path

here = Path(__file__).resolve().parent
jar = zipfile.ZipFile(sys.argv[1])
raw = json.load(open(here / "down_recipes.json"))
legacy = {old: new for new, old in raw["renames"].items()}
results = {}
for name in raw["ids"]:
    modern = legacy.get(name, name)
    path = f"data/minecraft/recipes/{modern}.json"
    if path not in jar.namelist():
        continue  # Special recipes the game computes in code.
    recipe = json.loads(jar.read(path))
    if recipe["type"] in ("crafting_shaped", "crafting_shapeless"):
        results[modern] = recipe["result"]["item"]
json.dump(dict(sorted(results.items())), sys.stdout, indent=1)
print()
