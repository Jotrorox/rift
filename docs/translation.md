# Protocol translation

Rift can connect a player to a backend that runs a different Minecraft version,
in the way Velocity does with ViaVersion and ViaBackwards. Each backend may
declare the version it runs; Rift then rewrites packets between the player's
protocol and the backend's in both directions, on initial login and across
`/server` switches between backends of different versions.

Translation is built one adjacent version step at a time. This release ships
the first step:

| Client | Backend | Direction |
| --- | --- | --- |
| 1.21.7, 1.21.8 (772) | 1.21.6 (771) | Newer client, older backend |
| 1.21.6 (771) | 1.21.7, 1.21.8 (772) | Older client, newer backend |

Every other client/backend pair still needs the same protocol. Rift refuses an
unsupported pair before connecting instead of letting the backend reject the
player.

## Configuration

Declare a backend's version with the table form of a backend entry:

```lua
local config = require("rift.config")
config.listeners.public = "0.0.0.0:25565"
config.backends.lobby = "127.0.0.1:25566"                      -- Runs the players' version.
config.backends.survival = { address = "127.0.0.1:25567", version = "1.21.6" }
config.routes.public = "lobby"
config.network = { initial = { "lobby" }, hubs = { "lobby" } }
```

`version` takes a release name from the [release matrix](network-protocol.md#release-matrix).
A backend without `version` is assumed to run each player's own version, as before.
Initial-server selection, fallbacks, hub recovery and `/server` skip backends that
cannot serve a player's version; with no compatible candidate left, the player sees
"No server on this network supports your Minecraft version." Managed servers and
service-group instances do not declare versions yet and always run the players' version.

Authentication, Velocity modern forwarding, BungeeCord messaging, the tab list,
boss bars and Rift's own messages work unchanged: Rift speaks the player's
protocol to the player and the backend's protocol to the backend.

## What is translated

1.21.6 and 1.21.7/1.21.8 share packet IDs and layouts. They differ in two built-in
registries: 1.21.7 inserted the *Lava Chicken* music disc and its sound event,
renumbering 159 items and 788 sound events. Rift rewrites every network ID from
those registries, wherever the protocol carries one:

- item stacks in inventories, cursors, equipment, entity metadata, particles,
  advancement icons, villager trades and recipe displays, including stacks nested
  inside data components (containers, bundles, charged projectiles, use remainders
  and exact component predicates);
- item references in data components (pot decorations, repair items),
  recipe property sets, stonecutter inputs, recipe book requirements, item
  statistics and item tags;
- sound events in sound and explosion packets and in components (consumables,
  equipment, shields, instruments, jukebox songs, break sounds);
- creative-mode inventory items and the hashed item stacks clients send with
  container clicks.

An entry the receiving version lacks becomes a stand-in: an older player sees
the Lava Chicken disc as the Creator disc, and an older backend receives it the
same way from a creative client. Data-driven registries sent during configuration
refer to built-in entries by name; a name the older client lacks would fail its
whole registry load, so Rift sends such sound events inline and names stand-in items.

Packets that cannot carry a changed ID pass through untouched, including chunks.
A packet that can carry one is parsed completely: a value Rift does not
understand, or trailing data, closes the connection rather than delivering a
corrupted packet.

## How the data and layouts are derived

[`scripts/translation_data.py`](../scripts/translation_data.py) runs the pinned,
checksum-verified official servers from [`tests/servers.json`](../tests/servers.json)
with Mojang's data generator and writes `src/protocol/translation/data/`: the
layout-selecting registries and packet names of each version, and each step's
registry differences. It refuses a reordered registry or a missing stand-in, and
`--check` fails when the committed files are stale. The wire layouts follow the
official servers' stream codecs.

## Verification

- Rust unit tests cover the registry tables, every rewritten structure, length
  prefixes that change size, registry data rewriting, malformed and truncated
  input, a translated session and a switch back to an untranslated backend, and
  version-aware backend selection.
- [`tests/translation_minecraft.py`](../tests/translation_minecraft.py) runs a
  real vanilla server with two simultaneous clients: one direct in the server's
  protocol, one through Rift in the other. A scripted scenario produces every
  packet kind above. Each client decodes all of its packets with the pinned
  [PrismarineJS minecraft-data](https://github.com/PrismarineJS/minecraft-data)
  schema for its own protocol (an independent decoder, with documented corrections
  checked against the official server), replaces IDs with names from its version's
  data reports, and the sessions must match packet for packet. The server itself
  checks serverbound translation: creative items arrive under the intended names,
  and translated hashed clicks match without a resync (a deliberately wrong claim
  is resynced as a control). Its `network` mode switches a 1.21.8 client between
  a native 1.21.8 lobby and a translated 1.21.6 survival server with the full
  network acceptance checks.

```sh
python3 scripts/translation_data.py --check
python3 tests/translation_minecraft.py --accept-eula --binary target/release/rift
```

These checks use offline test clients; they do not replace playing with a
signed-in graphical client.

## Adding a step

1. Add the pair to `STEPS` in `scripts/translation_data.py`, with stand-ins for
   entries only one side has, and generate the data.
2. If the generated diff changes packet IDs or layouts, or registries beyond items
   and sound events, write that step's rewriting explicitly; the current step
   type asserts that it has none.
3. Register the step in `src/protocol/translation/mod.rs`, extend the unit tests,
   and add the pair to `tests/translation_minecraft.py` and the CI matrix.
