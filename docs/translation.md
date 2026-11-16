# Protocol translation

Rift connects players to backends that run a different Minecraft version than
their client, as Velocity does with ViaVersion and ViaBackwards, but with no
plugin on the proxy or any backend. Any release from **1.12 to 26.3** can join
a backend on any other, on first login and across `/server` switches.

Translation is on by default and needs no configuration: Rift asks each
backend which version it runs.

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

## Configuration

| Setting | Default | Meaning |
| --- | --- | --- |
| `translation` | `true` | `false` turns translation off: Rift speaks each player's own protocol to every backend, which accepts or refuses the player itself. |
| `translation.enabled` | `true` | The same switch in table form. |
| `translation.detect` | `true` | Ask backends without a declared `version` which version they run. With `false`, only backends that declare a `version` are translated. |
| `translation.clients` | `"all"` | The client versions translated for backends and service groups that do not set their own; see [which versions a backend takes](#which-versions-a-backend-takes). |
| `backends.<name>.translation` | none | The client versions translated for this backend. |
| `service_groups.<name>.translation` | none | The client versions translated for every instance of this group. |
| `backends.<name>.version` | none | The release the backend runs, by name from the [release matrix](network-protocol.md#release-matrix). Takes precedence over detection. |

Without a configuration file, `rift <listen> <backend> --translation off` (or
the same after `--route`/`--default` options) turns translation off. Otherwise
it is on, with detection.

## Version detection

For a backend without `version`, Rift sends a status request with the player's
own protocol in the handshake, as the player's client would, and reads the
protocol the answer names. Managed servers and service-group instances are
detected the same way.

| The backend | Rift |
| --- | --- |
| Answers with the player's version, natively or through its own ViaVersion | Leaves it alone. |
| Does not answer, or names a version outside 1.12–26.3 | Speaks the player's own protocol to it. |

Rift keeps answers for 30 seconds per backend and client version, failures
for 5. It waits up to 2 seconds for the answer while a player logs in, so
declare the `version` of a backend that accepts connections but never answers
status requests.

## Which versions a backend takes

The proxy, a backend or a [service group](service-groups.md) can each say
which client versions are translated for it. Players on the version a backend
runs can always join it; the setting decides who else can.

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

- A backend's or group's own `translation` replaces `translation.clients`.
- A service group's applies to all its instances and may be changed while
  they run.
- `rift check` rejects a pattern that names no known version or lies outside
  1.12–26.3.

In the [web dashboard](http.md), every service group under *Groups &
templates* has a **Minecraft versions** section: follow the proxy setting, all
versions, same version or newer, same version or older, same version only, or
chosen versions (one switch per release family and a field for other
patterns). A line underneath lists the versions the choice covers before you
save.

Saving writes `translation` into the group's definition in the Lua script.
*Service groups* shows each group's setting in the *Player versions* column.

## Where Rift sends players

- Initial-server selection, fallbacks, hub recovery and `/server` skip a
  backend that cannot serve the player's version. For a backend without a
  declared `version`, Rift finds out when the player connects and moves on to
  the next fallback then.
- `/server` to a backend that leaves the player's version out reports "This
  server does not accept your Minecraft version."
- With no candidate left, the player sees "No server on this network supports
  your Minecraft version."
- The server list shows a backend that leaves the player's version out on the
  version it really runs. When Rift translates, a relayed server-list response
  names the player's protocol, so clients do not show the server as
  incompatible.

## Forwarding and authentication

Online authentication, BungeeCord and BungeeGuard forwarding work for every
pair. Velocity modern forwarding needs a backend on 1.13 or newer; the
player's version does not matter, because Rift answers the backend's request
in the backend's own protocol. A 1.19–1.19.2 player's chat key is not
forwarded to a backend on another version.

## What players get

Everything the two versions have in common is converted: chunks, light and
block changes, block entities, items in every container and menu, entities and
their data, particles and sounds, chat and every other text, titles, boss
bars, scoreboards and teams, the tab list, commands and tab completion,
statistics, recipes and the recipe book, advancements, villager trades, maps,
world borders, resource packs, plugin channels and the registries servers send.

Where a version lacks something, the player gets the closest thing it has.

**Stand-ins.** A block, item, entity, biome, sound or particle the client does
not know appears as the nearest one it has, keeping shape and state where it
can (copper stairs as stone stairs, a crossbow as a bow, an axolotl as a
tropical fish, cherry wood as birch). Items carry the name of what they really
are ("Mace"), and what the client sends back from the creative inventory or a
container is the original item. Particles and sounds without a counterpart
are left out.

**Features one side lacks** are answered or emulated where a player would
notice, and dropped otherwise:

| Case | Result |
| --- | --- |
| 1.20.2+ client, older backend | Rift builds the configuration phase from what the backend sends at join. |
| Older client, 1.20.2+ backend | Rift answers the configuration phase for the client. |
| 1.21.1 client, 1.21.2+ backend | The recipe book is rebuilt from what the server shows the client. |
| 1.12 client, 1.21.2+ backend | The recipe book is unlocked by matching what a recipe makes. It may show a few more 1.12 recipes than the server unlocked, and no smelting recipes. |
| 1.12 client | Completes commands through the server, because it has no command tree. |
| 1.16 or older client, 1.17+ backend | The client shows only Y 0–255; everything below 0 and above 255 is cut off. |
| 26.2+ client, 1.12 or 1.13 backend | These servers number entities from 0, which the client reads as "none". Rift renumbers that entity, but a reference to it inside another entity's data (a firework's user, a fishing hook's catch, a guardian's target) or as a leash holder is lost. |
| Transfer to another server, client before 1.20.5 | A disconnect naming the target. |
| Dialogs, waypoints and server links | Not shown to clients that have none. |
| Translation keys a client lacks | Shown as their raw key. |

The stand-in choices live in `crates/rift-translation/data/rules/`. Each
pair's module documentation under `crates/rift-translation/src/steps/` lists
what it converts, emulates and drops.

## Chat signing

Signatures cannot cross a change of the signed format. Across 1.18.2, 1.19,
1.19.1 and 1.19.3, chat reaches players as unsigned system chat and backends
receive unsigned messages. From 1.19.3 on, both sides share one format and
signatures are passed on as they are.

A backend that receives unsigned chat must not require signed chat. See
[signed chat](authentication.md) for what vanilla, Spigot and Paper need; a
Paper backend needs `enforce-secure-profile=false`. Two cases matter here:

- A 1.19 backend must keep its default `enforce-secure-profile=false`, or it
  refuses translated players.
- Pumpkin 0.2.0 announces secure-chat enforcement whatever its configuration.
  It works as a single backend, but Rift refuses it as a backend of an
  offline-mode [network](network-protocol.md), with or without translation.

`authentication.signed_chat = false` makes every backend's chat unsigned,
translated or not.

## See also

- [How translation works](translation-internals.md): design, cost, data
  sources and tests
- [Release matrix](network-protocol.md#release-matrix): every supported
  release and its protocol
