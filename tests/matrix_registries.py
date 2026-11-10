"""Registry names per protocol, for comparing what two clients of different
versions saw.

From 1.13 on item names and block states come from the vanilla server's own
data generator (`net.minecraft.data.Main --reports`), run once per release
under a lock and cached in RIFT_VIA_CACHE/reports; from 1.14 on every
registry does (registries.json). 1.13's generator has no registry report, so
its entity, effect and particle names, and all of 1.12's names, come from
minecraft-data at the pinned commit. (minecraft-data reuses 1.13's item IDs
for 1.13.1, which added five items, so its item data is not used there.)
"""

import json
from pathlib import Path
import re
import subprocess
import tempfile
import urllib.request

import matrix_env as env
from matrix_fixtures import SERVERS, jar, vanilla_for
import protodef

REGISTRIES = ("item", "entity_type", "mob_effect", "sound_event", "particle_type", "menu", "block",
              "block_entity_type")


def reports(name):
    """registries.json and blocks.json of a vanilla fixture's data generator."""
    fixture = SERVERS[name]
    directory = env.HOME / "reports" / f"{fixture['version']}-{fixture['checksum'][:12]}"
    done = directory / "reports" / "blocks.json"
    if not done.exists():
        with env.locked(f"reports-{fixture['version']}", f"data reports of {fixture['version']}"):
            if not done.exists():
                server = jar(name)
                env.log(f"generating data reports for {fixture['version']} (once)")
                directory.parent.mkdir(parents=True, exist_ok=True)
                with tempfile.TemporaryDirectory(dir=directory.parent) as temporary:
                    java = env.java(fixture["java"])
                    if fixture["protocol"] >= 757:  # Bundler jars from 1.18 on.
                        command = [java, "-DbundlerMainClass=net.minecraft.data.Main", "-jar", str(server)]
                    else:
                        command = [java, "-cp", str(server), "net.minecraft.data.Main"]
                    subprocess.run(command + ["--reports", "--output", str(Path(temporary) / "out")],
                                   cwd=temporary, check=True, stdout=subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL, timeout=900)
                    if directory.exists():
                        import shutil
                        shutil.rmtree(directory)
                    (Path(temporary) / "out").replace(directory)
    load = lambda report: json.loads((directory / "reports" / report).read_text())
    if (directory / "reports" / "registries.json").exists():
        registries = load("registries.json")
    else:  # 1.13 reports only items and blocks.
        registries = {"minecraft:item": {"entries": load("items.json")}}
    return registries, load("blocks.json"), directory / "reports"


def ordered(registry):
    entries = sorted(registry["entries"].items(), key=lambda entry: entry[1]["protocol_id"])
    return [name for name, _ in entries]


def minecraft_data(version, kind):
    """A minecraft-data file for a release at the pinned commit."""
    commit = protodef.PINS["commit"]
    paths = json.loads(_data_file(commit, "dataPaths.json"))["pc"][version]
    return json.loads(_data_file(commit, f"{paths[kind]}/{kind}.json"))


def _data_file(commit, path):
    cache = protodef.CACHE / f"{commit}-{path.replace('/', '-')}"
    if cache.exists():
        return cache.read_bytes()
    data = None
    if (protodef.REFERENCE / ".git").exists():
        result = subprocess.run(["git", "-C", str(protodef.REFERENCE), "show", f"{commit}:data/{path}"],
                                capture_output=True)
        data = result.stdout if result.returncode == 0 else None
    if data is None:
        url = f"https://raw.githubusercontent.com/PrismarineJS/minecraft-data/{commit}/data/{path}"
        with urllib.request.urlopen(url, timeout=60) as response:
            data = response.read()
    cache.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=cache.parent, delete=False) as pending:
        pending.write(data)
    Path(pending.name).replace(cache)
    return data


def snake(name):
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


# minecraft-data names spawn_entity object types of 1.13 descriptively
# ("item_stack", "armorstand", "falling_objects") and gets two of 1.12's
# wrong (73 and 75 are thrown potions and experience bottles, not eggs and
# snowballs). These are the entity registry names vanilla uses for the
# entity class each object ID spawns (1.12's EntityList, 1.13's
# EntityType).
OBJECT_NAMES_1_12 = {73: "potion", 75: "xp_bottle"}
OBJECT_NAMES_1_13 = {
    1: "boat", 2: "item", 3: "area_effect_cloud", 10: "minecart", 50: "tnt", 51: "end_crystal",
    60: "arrow", 61: "snowball", 62: "egg", 63: "fireball", 64: "small_fireball", 65: "ender_pearl",
    66: "wither_skull", 67: "shulker_bullet", 68: "llama_spit", 70: "falling_block", 71: "item_frame",
    72: "eye_of_ender", 73: "potion", 75: "experience_bottle", 76: "firework_rocket", 77: "leash_knot",
    78: "armor_stand", 79: "evoker_fangs", 90: "fishing_bobber", 91: "spectral_arrow",
    93: "dragon_fireball", 94: "trident",
}


class Registries:
    """Names by ID for one protocol."""

    def __init__(self, protocol):
        self.protocol = protocol
        self.names = {}  # registry -> list of names (index = ID)
        self.objects = None  # Pre-1.14 object entity IDs (spawn_entity) -> names.
        self.states = {}  # block state ID -> block name
        self.source = None
        if protocol >= 477:
            self._reports()
        else:
            self._minecraft_data()
            if protocol >= 393:
                registries, blocks, path = reports(vanilla_for(protocol))
                self.source += f"; items and block states from {path}"
                self.names["item"] = ordered(registries["minecraft:item"])
                self.states = {state["id"]: block for block, data in blocks.items() for state in data["states"]}
                self.names["block"] = list(blocks)
        self.ids = {key: {name: index for index, name in enumerate(names)}
                    for key, names in self.names.items()}

    def _reports(self):
        registries, blocks, path = reports(vanilla_for(self.protocol))
        self.source = f"data generator reports ({path})"
        for key in REGISTRIES:
            if f"minecraft:{key}" in registries:
                self.names[key] = ordered(registries[f"minecraft:{key}"])
        if self.protocol < 764 and "mob_effect" in self.names:
            # Until 1.20.2 packets carried a mob effect's registry ID plus one
            # (MobEffect.getId); the reports list the registry IDs.
            self.names["mob_effect"] = [None] + self.names["mob_effect"]
        for block, data in blocks.items():
            for state in data["states"]:
                self.states[state["id"]] = block

    def _minecraft_data(self):
        from matrix_fixtures import RELEASE_OF
        version = RELEASE_OF[self.protocol]
        self.source = f"minecraft-data {protodef.PINS['commit'][:10]} ({version})"
        dense = lambda entries: _dense([(entry["id"], f"minecraft:{entry['name']}") for entry in entries])
        self.names["item"] = dense(minecraft_data(version, "items"))
        blocks = minecraft_data(version, "blocks")
        self.names["block"] = dense(blocks)
        for block in blocks:
            if self.protocol >= 393:
                for state in range(block["minStateId"], block["maxStateId"] + 1):
                    self.states[state] = f"minecraft:{block['name']}"
            else:
                for meta in range(16):
                    self.states[block["id"] << 4 | meta] = f"minecraft:{block['name']}"
        entities = minecraft_data(version, "entities")
        self.names["entity_type"] = dense([entry for entry in entities if entry.get("type") != "object"])
        objects = [entry for entry in entities if entry.get("type") == "object"]
        if self.protocol < 477:
            self.objects = {entry["id"]: f"minecraft:{entry['name']}" for entry in objects}
            fixes = OBJECT_NAMES_1_13 if self.protocol >= 393 else OBJECT_NAMES_1_12
            self.objects.update({id: f"minecraft:{name}" for id, name in fixes.items()})
        effects = minecraft_data(version, "effects")
        # Packets carry these IDs as they are (1-based).
        self.names["mob_effect"] = _dense([(entry["id"], f"minecraft:{snake(entry['name'])}")
                                           for entry in effects])
        try:
            self.names["particle_type"] = dense(minecraft_data(version, "particles"))
        except KeyError:
            pass

    def name(self, registry, value):
        names = self.names.get(registry)
        if names is None:
            return f"<{registry} {value}>"  # No registry data: compare raw.
        if not isinstance(value, int) or not 0 <= value < len(names) or names[value] is None:
            return f"<invalid {registry} {value}>"
        return names[value]

    def id(self, registry, name):
        name = name if ":" in name else f"minecraft:{name}"
        return self.ids.get(registry, {}).get(name)

    def block(self, state):
        if state in self.states:
            return self.states[state]
        return f"<invalid block_state {state}>"

    def entity(self, value, living=True):
        """An entity type name; before 1.14 objects have their own IDs."""
        if self.objects is not None and not living:
            return self.objects.get(value, f"<invalid object {value}>")
        return self.name("entity_type", value)

    def knows(self, registry, name):
        if registry == "block_state":
            return name in set(self.states.values())
        return name in self.ids.get(registry, {})


def _dense(pairs):
    size = max((index for index, _ in pairs), default=-1) + 1
    names = [None] * size
    for index, name in pairs:
        if 0 <= index < size:
            names[index] = name
    return names


_cache = {}


def registries(protocol):
    if protocol not in _cache:
        _cache[protocol] = Registries(protocol)
    return _cache[protocol]
