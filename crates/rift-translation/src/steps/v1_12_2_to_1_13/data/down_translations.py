"""Generate down_translations.json: 1.13 translation keys for 1.12.2 clients.

    python3 down_translations.py <vanilla-1.12.2.jar> <vanilla-1.13.jar> > down_translations.json

1.13 renamed most translation keys (`tile.stone.stone.name` became
`block.minecraft.stone`). For every 1.13 key a 1.12.2 client does not know,
`renames` names a 1.12.2 key with the same English text (preferring one of
the same kind: blocks, items, entities, ...), and `fallback` holds the
English text of the keys with none, for showing as literal text.
"""
import json, re, sys, zipfile

old_jar, new_jar = zipfile.ZipFile(sys.argv[1]), zipfile.ZipFile(sys.argv[2])
old = {}
for line in old_jar.read("assets/minecraft/lang/en_us.lang").decode("utf-8").splitlines():
    if "=" in line and not line.startswith("#"):
        key, text = line.split("=", 1)
        old[key] = text
new = json.loads(new_jar.read("assets/minecraft/lang/en_us.json"))

# 1.13 key prefixes and the 1.12 prefixes of the same kind of thing.
KINDS = [
    ("block.", ("tile.",)),
    ("item.", ("item.", "tile.")),
    ("entity.", ("entity.",)),
    ("effect.", ("effect.", "potion.")),
    ("enchantment.", ("enchantment.",)),
    ("biome.", ("biome.",)),
    ("container.", ("container.",)),
    ("death.", ("death.",)),
    ("stat", ("stat",)),
    ("advancements.", ("advancements.",)),
    ("commands.", ("commands.",)),
    ("gui.", ("gui.",)),
]


def placeholders(text):
    return sorted(re.findall(r"%(?:\d+\$)?[sd]", text))


by_text = {}
for key, text in old.items():
    by_text.setdefault(text, []).append(key)
renames, fallback = {}, {}
for key, text in sorted(new.items()):
    if key in old:
        continue
    candidates = [c for c in by_text.get(text, []) if placeholders(old[c]) == placeholders(text)]
    # A key 1.13 still has means something else there; prefer renamed ones.
    candidates.sort(key=lambda c: (c in new, len(c), c))
    preferred = [c for prefix, olds in KINDS if key.startswith(prefix)
                 for c in candidates if c.startswith(olds)]
    choice = (preferred or candidates or [None])[0]
    if choice is not None:
        renames[key] = choice
    elif text:
        fallback[key] = text
json.dump({"renames": renames, "fallback": fallback}, sys.stdout, indent=0, sort_keys=True,
          ensure_ascii=False)
print()
print(f"{len(new)} 1.13 keys, {len(renames)} renamed, {len(fallback)} without a 1.12 key",
      file=sys.stderr)
