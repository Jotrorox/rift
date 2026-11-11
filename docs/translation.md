# Protocol translation

Rift connects players to backends that run another Minecraft version than
their client, the way Velocity does with ViaVersion and ViaBackwards installed,
without a plugin on the proxy or on any backend. A player on any release from
**1.12 to 26.3** can join a backend on any other release in that range: newer
clients on older servers and older clients on newer ones, on first login and
across `/server` switches between backends of different versions.

Translation is on unless you turn it off, and needs no configuration: Rift asks
each backend which version it runs.

## Configuration

```lua
local config = require("rift.config")
config.listeners.public = "0.0.0.0:25565"
config.backends.lobby = "127.0.0.1:25566"      -- Rift asks it for its version.
config.backends.minigames = { address = "127.0.0.1:25567", version = "1.12.2" }
config.routes.public = "lobby"
config.network = { initial = { "lobby" }, hubs = { "lobby" } }

-- config.translation = false                       -- Every backend must run the player's version.
-- config.translation = { detect = false }          -- Translate only for declared versions.
```

### Which versions a backend takes

Translation is all-or-nothing only if you leave it that way. The proxy, a
backend or a [service group](managed-servers.md#service-groups-and-dynamic-instances)
can each say which client versions are translated for it. Players on the
version a backend runs can always join it; the setting decides who else can.

```lua
config.translation = { clients = "all" }             -- The default for everything below.

config.backends.creative = { address = "127.0.0.1:25567", translation = "newer" }
config.backends.anarchy  = { address = "127.0.0.1:25568", translation = false }
config.backends.classic  = {
    address = "127.0.0.1:25569", version = "1.12.2",
    translation = { "1.12.*", "1.16.5", "1.20-1.21.4" },
}
config.service_groups.bedwars = {
    template = "bedwars", port_range = { 25600, 25650 },
    translation = { "1.20.*", ">=1.21.4" },
}
```

| Value | Who can join besides the backend's own version |
| --- | --- |
| `"all"` or `true` | Every version Rift translates, 1.12 to 26.3. The default. |
| `"newer"` | Newer clients only, as ViaVersion on the server would allow. |
| `"older"` | Older clients only, as ViaBackwards would allow. |
| `"none"` or `false` | Nobody: no translation for this backend. |
| a pattern or a list of patterns | The versions the patterns name; `"newer"` and `"older"` may be listed too. |

| Pattern | Covers |
| --- | --- |
| `"1.20.4"` | One release. Releases that share a protocol count as one: `"1.20.1"` also admits 1.20. |
| `"1.20.*"` (or `"1.20.x"`) | Every release of a family: 1.20 through 1.20.6. `"26.*"` covers 26.1 through 26.3. |
| `"1.16-1.18.2"` | A range, both ends included; either end may be a family (`"1.16.*-1.18.*"`). |
| `">=1.19"`, `"1.19+"` | That release and everything newer. |
| `"<=1.16.5"` | That release and everything older, down to 1.12. |

A backend's or group's own `translation` replaces `translation.clients`; a
service group's applies to all its instances and may be changed while they
run. `rift check` rejects a pattern that names no known version or lies
outside 1.12–26.3.

A player whose version is left out is not sent to that backend: initial-server
selection and fallbacks pass over it, `/server` reports "This server does not
accept your Minecraft version.", and the server list shows the backend on the
version it really runs. For a backend without a declared `version` Rift finds
out when the player connects, and moves on to the next fallback then.

**In the web interface**, *Groups & templates* has a **Minecraft versions**
section on every service group: follow the proxy setting, all versions, same
version or newer, same version or older, same version only, or chosen
versions with one switch per release family and a field for other patterns. A
line underneath lists the versions the choice covers before you save, and
*Service groups* shows each group's setting in the *Player versions* column.
Saving writes `translation` into the group's definition in the Lua script.

| Setting | Default | Meaning |
| --- | --- | --- |
| `translation` | `true` | `false` turns translation off: Rift speaks each player's own protocol to every backend, which accepts or refuses the player itself, as before this feature. |
| `translation.enabled` | `true` | The same switch in table form. |
| `translation.detect` | `true` | Ask backends without a declared `version` which version they run. With `false`, only backends that declare a `version` are translated. |
| `translation.clients` | `"all"` | The client versions translated for backends and service groups that do not set their own; see [below](#which-versions-a-backend-takes). |
| `backends.<name>.translation` | none | The client versions translated for this backend. |
| `service_groups.<name>.translation` | none | The client versions translated for every instance of this group. |
| `backends.<name>.version` | none | The release the backend runs, by name from the [release matrix](network-protocol.md#release-matrix). Takes precedence over detection. |

Without a configuration file, `rift <listen> --default <backend> --translation off`
turns translation off; it is on otherwise, with detection.

**Detection.** For a backend without `version`, Rift sends a status request
with the player's own protocol in the handshake, as the player's client would,
and reads the protocol the answer names. A backend that already accepts the
player's version, natively or through its own ViaVersion, answers with that
version and is left alone. Answers are kept for 30 seconds per backend and
client version, failures for 5; a backend that does not answer, or names a
version outside 1.12–26.3, is spoken to in the player's own protocol. Managed
servers and service-group instances are detected the same way. Rift waits up
to 2 seconds for the answer while a player logs in, so declare the `version`
of a backend that accepts connections but never answers status requests.

**Selection.** Initial-server selection, fallbacks, hub recovery and `/server`
skip backends whose declared version cannot serve a player; with no candidate
left the player sees "No server on this network supports your Minecraft
version." A relayed server-list response names the player's protocol when Rift
translates to the backend's, so clients do not show the server as incompatible.

**Forwarding and authentication.** Online authentication, BungeeCord and
BungeeGuard forwarding work for every pair. Velocity modern forwarding needs a
backend on 1.13 or newer; the player's version does not matter, because Rift
answers the backend's request in the backend's own protocol. A 1.19–1.19.2
player's chat key is not forwarded to a backend on another version.

## What players get

Everything a version has in common with the other is converted: chunks, light
and block changes, block entities, items in every container and menu, entities
and their data, particles and sounds, chat and every other text, titles, boss
bars, scoreboards and teams, the tab list, commands and tab completion,
statistics, recipes and the recipe book, advancements, villager trades, maps,
world borders, resource packs, plugin channels and the registries servers send.

Where a version lacks something, the player gets the closest thing it has:

- **Stand-ins.** A block, item, entity, biome, sound or particle the player's
  client does not know appears as the nearest one it has, keeping shape and
  state where it can (copper stairs as stone stairs, a crossbow as a bow, an
  axolotl as a tropical fish, cherry wood as birch). Items carry the name of
  what they really are ("Mace"), and what the client sends back to the server
  from the creative inventory or a container is the original item. Particles
  and sounds without a counterpart are left out. The choices live in
  `crates/rift-translation/data/rules/`.
- **World height.** A 1.16 or older client shows only Y 0–255: on a 1.17+
  backend everything below 0 and above 255 is cut off.
- **Chat signing.** Signatures cannot cross a change of the signed format, so
  across 1.18.2, 1.19, 1.19.1 and 1.19.3 chat reaches players as unsigned
  system chat and backends receive unsigned messages; from 1.19.3 on, both
  sides share one format and signatures are passed on as they are. A backend behind a
  proxy runs with `online-mode=false`, where `enforce-secure-profile` has no
  effect from 1.19.1 on; a 1.19 backend must keep its default
  `enforce-secure-profile=false`, or it refuses translated players. Pumpkin
  0.2.0 announces secure-chat enforcement whatever its configuration: it works
  as a single backend, but Rift refuses it as a backend of an offline-mode
  [network](network-protocol.md), with or without translation.
- **Features one side lacks** are answered or emulated where a player would
  notice, and dropped otherwise. Examples: the configuration phase a 1.20.2+
  client needs is built from what an older backend sends at join, and answered
  on behalf of an older client on a newer backend; a 1.21.1 client on a 1.21.2+
  backend gets a recipe book rebuilt from what the server shows it; a transfer
  to another server becomes a disconnect naming the target for clients before
  1.20.5; dialogs, waypoints and server links are not shown to clients that
  have none; a 1.12 client completes commands through the server because it
  has no command tree; translation keys a client lacks show as their raw key.
- **Recipes on a 1.12 client.** On a 1.21.2+ backend its recipe book is
  unlocked by matching what a recipe makes, so it may show a few more 1.12
  recipes than the server unlocked, and no smelting recipes.
- **Entity 0.** 1.12 and 1.13 servers number entities from 0, which a 26.2+
  client reads as "none". Rift renumbers that entity, but a reference to it
  inside another entity's data (a firework's user, a fishing hook's catch, a
  guardian's target) or as a leash holder is lost.

Each pair's step lists what it converts, emulates and drops in its module
documentation under `crates/rift-translation/src/steps/`.

## How it works

`crates/rift-translation` is a library without I/O. A `Translator` chains one
step per pair of adjacent protocols (41 pairs between 1.12 and 26.3); each pair
has an `up` step for a newer client on an older backend and a `down` step for
the reverse. A step routes packets by name: a packet both protocols share
passes through with only its ID changed, and a step may rewrite, split, drop,
answer or synthesise packets, which is what the 1.20.2 configuration phase,
the 1.13 flattening or the 1.20.5 item components need.

Shared codecs under `src/codec/` know the structures that travel in many
packets and versions — item stacks before and after data components, entity
data, particles, chunks and light, text components as JSON and NBT, tags,
command trees, synchronised registries — in every protocol, and convert them
between the two protocols of whichever step calls them. Registry IDs are
mapped by entry name through tables built from each version's own registries,
with the per-pair rename and stand-in rules.

In Rift, a backend on another protocol gets a translator inside its
connection. The session then sees a backend that speaks the player's protocol,
so login, forwarding, switching, the tab list, proxy commands and everything
plugins do work the same with and without translation. Packets Rift neither
translates nor rewrites are still forwarded in the frame they arrived in.

A packet a step has to change is parsed completely. One it cannot parse fails
the connection rather than reaching the peer corrupted.

Cost grows with the distance between the two versions, because every step in
between handles each packet. A release build replays a whole recorded join
session through all 41 steps in about two seconds of CPU, and through one step
in about 0.15 seconds including start-up. A 1.21.1 or older client on a
1.21.2+ backend costs more at join: its recipe list is resent whenever the
book grows.

## Where the data comes from

Rift is BSD-licensed and ships no ViaVersion code or mapping files. Per-version
packet tables, registries and block states are generated by
[`scripts/translation_crate.py`](../scripts/translation_crate.py) from the
pinned, checksum-verified official servers in
[`tests/servers.json`](../tests/servers.json) (Mojang's data generator
reports) and, for versions that predate those reports, from the MIT-licensed
[PrismarineJS minecraft-data](https://github.com/PrismarineJS/minecraft-data);
packet order for versions before 1.21 is read from ViaVersion's packet-type
enums, which is the protocol's own numbering under Mojang's names.
[`data/SOURCES.md`](../crates/rift-translation/data/SOURCES.md) lists the
source of every table. Entity data layouts, vanilla tags and the registries
servers synchronise are extracted from the same server jars by
`scripts/translation_entities.py`, `translation_tags.py` and
`translation_registries.py`. ViaVersion and ViaBackwards served as references
for what changed between versions; the implementation was written
independently. Every script has a `--check` mode that fails when the committed
files are stale.

## Verification

- **Unit tests** in the crate (over 750) cover every codec and step, with
  recorded packets from the official server of each version where a hand-built
  one would prove less.
- **Recorded sessions.** [`tests/translation_corpus.py`](../tests/translation_corpus.py)
  records a scripted session from the official server of every protocol. The
  crate's `replay` example feeds one through any chain of steps; every pair,
  and the whole range in both directions, replays without a rejected packet.
- **Real servers.** [`tests/translation_matrix.py`](../tests/translation_matrix.py)
  starts a real server, connects a reference client directly and a client of
  another version through the translator, and runs 33 scenario steps (blocks,
  items, entities, chat, commands, containers, scoreboards, titles, respawn,
  a dimension change and more). Every packet the translated client receives
  is decoded strictly with the pinned minecraft-data schema of its own version
  — an independent description of the protocol, with documented corrections
  where the official server disagrees with it — and compared with what the
  reference client saw. Every adjacent pair passes in both directions on the
  official server, as do Paper, Spigot and Pumpkin fixtures.
- **Real clients.** [`tests/real_client.py`](../tests/real_client.py) runs the
  unmodified vanilla client of a version headlessly through the translator and
  fails on any packet, registry or network error it logs
  ([details](../tests/REAL_CLIENT.md)). Real clients of every anchor version
  were run against older and newer servers of their neighbouring versions.
- **Through Rift.** `tests/translation_matrix.py --via rift` and
  `--via rift-detect` run the same cells through the `rift` binary, with the
  backend's version declared and detected;
  [`tests/translation_minecraft.py`](../tests/translation_minecraft.py) and
  [`tests/translation_network.py`](../tests/translation_network.py) switch a
  player between backends of different versions, and
  [`tests/translation_real_client.py`](../tests/translation_real_client.py)
  runs real vanilla clients behind Rift while the server's console plays a
  scenario.

```sh
python3 scripts/translation_crate.py --check
cargo test -p rift-translation
cargo run -p rift-translation --example replay -- --backend 340 --client 777 --summary
python3 tests/translation_matrix.py --accept-eula --server vanilla-1.16.5 --client 1.12.2,1.20.1,26.3 --via rift
python3 tests/real_client.py --accept-eula join --version 1.20.1 --address 127.0.0.1:25565
```

These checks use offline-mode servers and offline test accounts. They do not
replace playing with a signed-in graphical client on your own network, and
they cover the vanilla protocol: a plugin or mod that sends its own packet
layouts through plugin channels is passed through as it is.

## Adding a version

1. Add the release to `tests/servers.json` and `version::PROTOCOLS`, and
   generate its data with the scripts above.
2. Create the pair's module under `src/steps/` with an `up` and a `down` step
   built on `RouteBuilder::defaults()`; the module documentation of
   `src/steps/mod.rs` describes how. `Routes::problems()` lists every packet
   that still needs a rule.
3. Teach the shared codecs the new protocol's layouts where they changed, and
   write the pair's rename and stand-in rules
   (`cargo run -p rift-translation --example mapping_report -- <older> <newer>`
   lists what is unmapped).
4. Record the new server's session, then run the replay, the matrix and a real
   client for the new pair in both directions.
