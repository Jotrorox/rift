# Permissions

Rift keeps one permission store for the whole network: users, groups,
inheritance with weights, negated nodes, wildcards, temporary nodes, contexts,
prefixes, suffixes, meta and tracks, the model LuckPerms users know. The proxy
owns the data and resolves it; backends hold replicas and answer
`hasPermission` from memory (see the [protocol](permissions-protocol.md)).

On the proxy the store answers extension commands, `ctx.has_permission`,
the Lua `rift.permissions` module (extensions API v3), the `RiftInGroup`
[BungeeCord request](bungeecord.md#rift-requests), the [dashboard](#dashboard),
the [HTTP API](http.md#permissions-api) and the [`/perms` commands](#commands);
backends replicate it over the [bus](#backends).

## Model

**Users** are keyed by UUID (32 lowercase hex digits; the dashed form is
accepted on input). A user has nodes, an optional explicit primary group and
the name they last joined with.

**Groups** have a name (1 to 64 bytes of `[a-z0-9_.-]`), a weight (higher wins,
default 0), an optional display name and nodes. The `default` group always
exists, may be empty and cannot be deleted. Every user inherits it implicitly
and last; a negated `inherit` node is the only way to opt a user out.

**Nodes** are typed:

| Kind | Payload | Meaning |
| --- | --- | --- |
| `permission` | pattern, `true`/`false` | Grant or deny `a.b`, `a.*` or `*` |
| `inherit` | group, `true`/`false` | Inherit a group; `false` blocks it on every path |
| `prefix`, `suffix` | priority, text | Chat decoration; the highest priority wins |
| `meta` | key, value | Arbitrary key/value data such as `homes = "5"` |
| `weight` | integer | Groups only; the same as the group's weight |
| `display_name` | text | Groups only |

Patterns are dot-separated segments of `[a-z0-9_:-]`, at most 256 bytes, with
`*` only as a whole trailing segment. Input is lowercased as Bukkit does, and
Pumpkin's `plugin:node` fits as written. Every node also has:

- `contexts`: `key=value` pairs that must all be active for the node to apply.
  `server`, `group`, `world`, `dimension` and `gamemode` are filled by Rift and
  the backend adapters; any other key is custom (`region=spawn`). No contexts
  means global. `server` is the backend's name, which for an instance of a
  [service group](managed-servers.md#service-groups-and-dynamic-instances) is
  the instance (`lobby-2`), so a grant meant for every lobby uses
  `group=lobby` instead: `group` is the service group of the backend, and is
  absent on backends outside groups.
- an optional expiry (Unix seconds). A node expiring at `t` applies while the
  time is before `t`, exactly, at every check.
- a source: `store` (edited at runtime), `config` (mirrored from
  `extensions.groups`), `import` (LuckPerms import) or `plugin:<name>`.

One node exists per subject, kind, key and contexts; setting a node replaces
the one in that slot.

**Tracks** are ordered lists of groups (`default → vip → mod → admin`).
Promoting moves a user's `inherit` node to the next group on the track in the
contexts given; demoting moves it back. Leaving or arriving at `default` touches
no node, because that edge is implicit.

**Registered permissions** are a catalogue of nodes backends declare, with a
description and a default (`true`, `false`, `op`, `op:<level>`, `not_op`). They
never change how a node with an explicit value resolves.

## Resolution

Resolution has two stages, specified exactly in the
[protocol](permissions-protocol.md#compiled-subject) and held to the shared
fixture corpus.

1. **Compile** (proxy only): walk inheritance from the user, keeping each
   edge's contexts, and flatten everything into ranked entries. The user's own
   nodes rank 0; groups rank 1, 2, 3, … by weight descending, then by depth
   (a direct parent before a grandparent), then by name, with `default` last.
2. **Check** (anywhere): among the entries whose contexts are active, the most
   specific pattern wins (exact node, then the longest `prefix.*`, then `*`);
   on a tie, more context pairs win, then the lower rank, then `false`.

A check answers `true`, `false` or undefined. Commands and
`ctx.has_permission` treat undefined as `false`; `rift.permissions.check`
returns `nil` for it, so a script can tell "denied" from "never set".

### Contexts on the proxy

Proxy-side checks use the contexts Rift knows for an online player:

- `server`, the backend they are on, and `group`, its service group when it
  is an instance of one. Both follow the player on every switch.
- `dimension` and `gamemode`, read from the play packets Rift relays from that
  backend on every supported version (1.8.9–26.3): Join Game and Respawn name
  both, and the game-mode Game Event updates `gamemode` in place. `dimension`
  is the world's namespaced key, so the vanilla worlds are
  `minecraft:overworld`, `minecraft:the_nether` and `minecraft:the_end`
  exactly as the backend adapters report them (before 1.16 the packets carry
  only those three, as dimension IDs). `gamemode` is `survival`, `creative`,
  `adventure` or `spectator`. A packet Rift cannot read is relayed unchanged
  and leaves the values as they were; a Join Game it cannot read clears them.
- Not `world`: a Bukkit world name (`world_nether`) never crosses the wire, so
  nodes that require `world` apply on the backends, whose adapters fill it,
  but not in proxy-side checks.
- Custom contexts reported by the player's backend, for keys listed in
  `permissions.contexts.external`. A report replaces the backend's earlier one
  and is accepted only while the player is on that backend, or while the proxy
  forwards their connection to it.
- Custom contexts from Lua [context providers](#context-providers).

Reported and provided contexts are cleared on every server switch, and the
proxy's own restart from the new `server`; the new backend's Join Game fills
`dimension` and `gamemode` again. Offline players have no contexts, so only
global nodes apply to them.

A backend computes the reserved contexts from its own player state, which is
more precise: it knows `world`, and a custom Paper world with a vanilla
environment reports that environment's dimension (`minecraft:the_nether`)
where the proxy sees the world's own key. Grants scoped to the three vanilla
dimensions and to game modes resolve the same on both sides. The `/perms`
entries in a player's command tree follow a context change with the backend's
next command tree, as they do for any other permission change.

## Storage and revisions

Everything lives in the `instance_database` file (`rift.sqlite3` beside the
configuration by default), in tables prefixed `perm_`, next to the instance
registry and on the same connection. Each part records its schema version in
`rift_schema`. Startup runs `PRAGMA integrity_check` and refuses to start on a
damaged file rather than run with partial policy. Without a configuration file
(a bare `rift <listen> <backend>`), the store is in memory.

Every write is one transaction that also appends one row to `perm_change`. The
row's number is the new **revision**, stored, so it keeps increasing across
restarts. A write that changes nothing creates no row. Each row lists its
effects as before/after pairs, so the log is both the audit trail and, later,
the replication stream for a follower proxy.

After each commit the proxy recompiles the online players the change affects
and tells subscribers, which is how backends learn about it. Compiled
subjects are cached per user and dropped when one of their nodes or any group
they could inherit changes.

Expired nodes stop applying the moment they expire. Every
`permissions.sweep_interval_ms` the proxy deletes them in one revision, with
actor `system`, so subscribers and `on_change` handlers see the removal.

Rows record who made them: `uuid:<player>`, `operator:<name>`,
`lua:<callback>` (for example `lua:command.rank` or `lua:join`),
`backend:<name>`, `cli`, `import` or `system`.

## Configuration

```lua
rift.config.permissions = {
    -- Defaults shown. They keep a compiled subject under 64 KiB.
    limits = {
        groups = 1024,           -- groups in the store
        group_nodes = 256,       -- nodes per group
        user_nodes = 512,        -- nodes per user
        parents = 16,            -- inherit nodes per user or group
        contexts = 32,           -- context pairs per node
        tracks = 64,
        compiled_entries = 4096, -- entries of one compiled subject
    },
    -- Custom context keys backends may report for their players.
    contexts = { external = { "region" } },
    sweep_interval_ms = 60000,   -- 1000 to 86400000
}
```

Every field is optional and can change on reload. A write past a limit fails
without changing anything. A user whose inheritance compiles to more than
`compiled_entries` entries resolves everything as undefined and the proxy logs
why.

### Groups as configuration

Extensions with `api_version = 2` or `3` can keep groups in Lua:

```lua
rift.config.extensions = {
    api_version = 3,
    groups = {
        default = { permissions = { ["network.queue"] = true, ["chat.color"] = false } },
        staff = {
            inherits = { "default" },
            weight = 10,
            prefix = "&9[Staff] ",                 -- priority = weight
            suffix = { priority = 5, text = " ✦" },
            meta = { homes = 5 },
            permissions = { ["network.*"] = true, ["chat.color"] = true },
        },
    },
}
```

`permissions` maps patterns to `true` or `false`; `inherits` lists parent
groups, which need not be defined here and may be missing (a missing parent
grants nothing until it exists). `prefix` and `suffix` take text, using the
group's weight as priority, or `{ priority = n, text = "..." }`. `meta` values
are strings or integers.

On every committed reload Rift makes the store's `config` nodes equal this
table, in one revision when anything differs and none when nothing does. It
creates missing groups and never touches other nodes, so the dashboard and Lua
can add to configured groups, and the change log shows which side wrote what.
Removing a group from the table removes its configured nodes; the group itself
and its runtime nodes stay. The table must fit `permissions.limits`, which
`rift check` verifies.

## Lua API

`rift.permissions` (also `require("rift.permissions")`) needs
`extensions.api_version = 3`. Calls are synchronous, never leave the process
and work only in extension callbacks and jobs; writes have committed when they
return. Calling them while the configuration loads is an error, except for the
two registrations at the end of this section. Methods of the objects that
`user`, `group` and `track` return are called with a colon.

```lua
local perms = require("rift.permissions")

perms.check(uuid, "essentials.fly", { server = "survival" }) -- true, false or nil
perms.check(uuid, "essentials.fly")      -- in the player's current contexts
perms.compiled(uuid)                     -- the compiled subject, as JSON-shaped tables
perms.meta(uuid, ctx)                    -- { prefix, suffix, meta = { homes = "5" }, primary_group }
perms.groups()                           -- { { name, weight, display_name }, ... }

local user = perms.user(uuid)
user:set("essentials.fly", true, { expires_in = 3600, contexts = { server = "survival" }, reason = "event" })
user:unset("essentials.fly", { server = "survival" })
user:add_parent("vip", { expires_in = 86400 * 30 })
user:remove_parent("vip")
user:set_prefix(100, "&6★ ")             -- also set_suffix, set_meta(key, value)
user:unset_meta("homes")                 -- also unset_prefix(priority), unset_suffix(priority)
user:set_primary_group("vip")            -- nil clears it
user:clear()                             -- every node; clear(contexts) only those
user:nodes()                             -- the user's own nodes

local vip = perms.group("vip")
vip:create({ weight = 50, display_name = "VIP" })
vip:exists()
vip:set_weight(50)
vip:set_display_name("VIP")
vip:set_prefix(100, "&6[VIP] ")
vip:set("kit.vip", true)                 -- and every node method users have
vip:delete()

local ranks = perms.track("ranks")
ranks:set({ "default", "vip", "mod" })
ranks:promote(uuid)                      -- ranks:promote(uuid, { server = "survival" })
ranks:demote(uuid)
ranks:groups()
```

Contexts are tables of keys to a string, or to a list of strings for a custom
key with several values. Write options are `contexts`, `expires_in` (seconds
from now), `expires_at` (Unix seconds) and `reason` (up to 256 bytes, recorded in
the change log). The second argument of an `unset` method is either options or
a bare contexts table. Writes return `{ revision = n, changed = true|false }`;
`changed` is false when the store already held exactly that.

A callback may write 64 times. Invalid arguments, unknown groups or tracks,
limits and impossible track moves raise errors; `rift.try` catches them. A
write that fails changes nothing.

### Change events

```lua
rift.permissions.on_change(function(event)
    -- { revision, actor, reason, subject, kind, key, before, after }
    rift.publish("audit.permissions", event.subject .. " " .. event.kind .. " " .. event.key)
end)
```

Register handlers while the configuration loads (equivalently, set
`extensions.on_permission_change`). Rift calls them for each committed change,
once per effect, on the job workers with the job deadline: `subject` is
`user:<uuid>`, `group:<name>` or `track:<name>`, `kind` is a node kind or
`group`, `track` or `primary_group`, and `before` and `after` are the node,
group attributes, track list or primary group on each side (nil when created or
removed). At most 64 effects of one change are delivered. Changes the handlers
make themselves are not delivered back to them. Events are ephemeral: a change
committed during a reload may be missed.

### Context providers

```lua
rift.permissions.contexts.provide("region", function(player)
    return regions.of(player.server)    -- a string, a list of strings, or nil
end)
```

Providers (equivalently, `extensions.context_providers`) compute custom
contexts when a player reaches a server: after the first join and after every
switch, before the `join` and `after_transfer` callbacks. `player` has `uuid`,
`name` and `server`. Keys must be custom (not `server`, `group`, `world`, `dimension`
or `gamemode`); at most 16 providers.

## Visibility

Rift does not know what "vanished" means; it knows who may see whom. A player
whose meta key `rift.visible-to` is set is shown only to viewers who hold the
permission node in its value. A vanish plugin sets the key and Rift does the
rest:

```sh
/perms user Staff meta set rift.visible-to vanish.see          # hidden everywhere
/perms user Staff meta settemp rift.visible-to vanish.see 30m  # for 30 minutes
/perms user Staff meta set rift.visible-to vanish.see server=lobby-1
/perms group mod meta set rift.visible-to vanish.mod           # a vanish level
/perms group admin permission set vanish.see                   # who sees them
```

The rule is meta, not a permission node, so staff holding `*` are not all
hidden; and because it is meta, it is durable, known at login, scoped by
contexts, expirable, audited in the change log and replicated to backends like
every other node, so timed vanish, per-server vanish and vanish levels need no
code. A value that is not a valid node still hides the player: nobody holds
it.

Every reader of "who is online" names who is looking:

| Viewer | Sees | Used by |
| --- | --- | --- |
| player | unrestricted players, themselves, and players whose node they hold (in the viewer's current contexts) | the [network tab list](operations.md#tab-list-header-footer-and-network-wide-players), the `{online}` and `{server_online}` placeholders, player-name completion of extension and plugin commands |
| public | unrestricted players only | the server-list player count, the status server's counts and its badge |
| backend | unrestricted players only | BungeeCord `PlayerCount`, `PlayerList` and the `RiftServers` counts |
| system | everyone | capacity, queues, scaling and idle shutdown, Prometheus metrics, `/api/status`, kick and transfer by name, `GetPlayerServer`, `rift.players.*` and the plugin API |

System readers must see hidden players: if idle shutdown counted only visible
players, Rift could stop the server a hidden administrator stands on. Plugins
see everyone too, with the rule beside each player (`visible_to`; see
[the plugin API](plugins.md#host-api)), and decide for themselves.

Rift resolves the rule onto the player when they log in (before any tab list
or count includes them), when they join a world (before the join event is
published), on every server switch (contexts change, so per-server rules
apply), and whenever their permissions change; each change is published on
`rift.events.players` as a `visibility` event. Sessions rebuild their tab list
when any online player's permissions change, so a viewer who gains or loses
the node sees the difference at once. An expiring rule or `see` node takes
effect when the sweeper deletes it, within `sweep_interval_ms`. A dimension or
game-mode change does not resolve the rule again; scope rules by `server` or
`group` rather than by `dimension` or `gamemode`.

Rift hides players from what it builds itself. A backend lists its own players
in their tab list and in its own player count; a Paper plugin reads the same
key through RiftPerms and hides the player locally (see
[RiftPerms](permissions-java.md#visibility-on-paper)). The example
[`vanish-lua`](../examples/plugins/vanish-lua/init.lua) plugin implements
`/vanish`, `/vanish here` and `/vanish <minutes>` in 40 lines.

## Persistent plugins

Persistent [plugins](plugins.md), in Lua or WebAssembly, use the store through
the `permissions` interface of the plugin contract (`rift:plugin@0.1`). Its
calls keep their signatures and map onto the store like this:

| Contract call | In the permission store |
| --- | --- |
| `has(uuid, node)` | the node in the player's current proxy contexts, like `ctx.has_permission`; undefined is false |
| `set(uuid, node, some(value))` | sets the user's global permission node; `uuid` `*` sets it on `default` |
| `set(uuid, node, none)` | unsets that global node |
| `add-group(uuid, group)` | sets a global `inherit` node; returns whether anything changed |
| `remove-group(uuid, group)` | unsets the global `inherit` node; returns whether anything changed |
| `groups(uuid)` | the groups the player is in, in their current contexts: direct, inherited and `default`, in rank order |
| `list-groups()` | every group as a JSON object keyed by name: `name`, `weight`, `display_name` and `nodes` |
| `meta(uuid, key)` (0.1.3) | one meta value in the player's current contexts, or `none` |
| `set-meta(uuid, key, value, contexts-json, expires-at)` (0.1.3) | sets the user's meta node in exactly those contexts (`none`: global), expiring at Unix seconds if given; a `none` value removes it; returns whether anything changed |

Writes are committed when the call returns and persist like any other store
change. They are recorded with actor `lua:plugin:<name>`, whatever the
plugin's engine, and the nodes carry source `plugin:<name>`. Permission and
group nodes with contexts or expiry, tracks and group edits are not part of
the contract; use the dashboard, `/perms` or `rift.lua` with API v3 for them.
Persistent Lua plugins also have `rift.permissions.check(uuid, node, contexts?)`
(true, false or nil), `rift.permissions.meta(uuid, contexts?)`, as in the Lua
API above (without contexts they use the player's current ones), and
`rift.permissions.set_meta(uuid, key, value, options?)`, whose `value` `nil`
removes the node and whose options are `contexts`, `expires_in` and
`expires_at` as for API v3 writes.

## LuckPerms import and export

The authority reads the JSON that `/lp export` writes, gzipped or plain:

- `group.x`, `prefix.N.text`, `suffix.N.text`, `meta.key.value`, `weight.N`
  and `displayname.x` become typed nodes; group weights and display names come
  from their global nodes.
- `{a,b}` and `(a|b)` shorthands expand into separate nodes.
- LuckPerms context sets mean "any listed value"; Rift writes one node per
  combination, and drops `server=global` and `world=global`.
- `server=<name>` naming one of the configuration's service groups becomes
  `group=<name>`: LuckPerms knew one server per proxy entry, while Rift's
  instances of that group are `<name>-1`, `<name>-2`, …, where only `group`
  matches. Names that are not service groups stay `server`. Export writes
  `group=<name>` back as `server=<name>`, so LuckPerms reads it the old way.
- Regex nodes (`R=…`), invalid names, nodes past a limit and expiries already in
  the past are skipped, and the report lists each with the reason.

Everything lands with source `import` in one revision. Importing is additive:
nodes replace what is in their slots and nothing else is deleted. With replace,
every earlier `import` node is deleted first, in the same revision; nodes from
other sources are never touched. Export writes the same format, so `/lp import`
can read it back; a context key with several values keeps its first value
there, since LuckPerms would read the list as "any".

Run it with `/perms import <file> [--replace]` and `/perms export <file>`
([Commands](#commands)). Over HTTP, `POST /api/perms/import` takes the file (gzipped or not, with
`?replace=true` for replace) and `GET /api/perms/export` returns it; see the
[HTTP API](http.md#permissions-api).

## Commands

Every surface speaks the same LuckPerms-style grammar and produces the same
audit rows:

- in game, the proxy answers `/perms` and its alias `/rp` itself, for
  [network](operations.md) sessions (an extension command of the same name
  takes precedence). The command tree shows them to players who hold any
  `rift.perms.` node;
- a backend adapter forwards its own `/perms` on
  [`rift.perms.command`](permissions-protocol.md#commands) and shows the reply;
- `rift admin perms <command>` runs it from the proxy host with console
  rights (admin permission `perms`, see [Command line](#command-line)).

In game, lines are coloured by the theme styles `heading`, `good`, `bad`,
`muted` and `plain` (gold, green, red, gray and none by default), which
operators restyle under `text.theme` ([styled text](text.md#theme)).

| Command | Effect |
| --- | --- |
| `user <player> info` | Online server, primary group, groups and contexts in effect, prefix, suffix and every own node |
| `user <player> permission set <node> [true\|false] [context...] [--duration <time>]` | Set a node (default `true`); `--expiry <unix>` sets an absolute end |
| `user <player> permission settemp <node> <true\|false> <time> [context...]` | Set a temporary node |
| `user <player> permission unset\|unsettemp <node> [context...]` | Remove the node in exactly these contexts (`unsettemp` only a temporary one) |
| `user <player> permission info` / `check <node> [context...]` | List permission nodes / show the node and what it resolves to |
| `user <player> parent add\|remove\|set <group> [context...]` | Inherit, stop inheriting, or make the group the only parent in these contexts |
| `user <player> parent addtemp <group> <time> [context...]`, `removetemp` | Temporary membership |
| `user <player> parent switchprimarygroup <group>`, `parent info` | Explicit primary group; list parents |
| `user <player> meta set\|unset <key> [<value>] [context...]`, `settemp` | Meta values |
| `user <player> meta setprefix\|setsuffix <priority> <text> [context...]` | Also `addprefix`, `settempprefix <priority> <text> <time>`, … |
| `user <player> meta removeprefix\|removesuffix <priority> [context...]`, `meta info`, `meta clear [context...]` | |
| `user <player> promote\|demote <track> [context...]` | Move along a track |
| `user <player> clear [context...]` | Remove every node, or those in exactly these contexts |
| `group <group> create [weight] [display name]`, `delete`, `info`, `listmembers` | Also `creategroup` and `deletegroup` |
| `group <group> setweight <weight>`, `setdisplayname [name]` | No name clears the display name |
| `group <group> permission\|parent\|meta …`, `group <group> clear` | As for users; `group default clear` empties `default` |
| `track <track> create [group...]`, `delete`, `info`, `clear` | Also `createtrack` and `deletetrack` |
| `track <track> append <group>`, `insert <group> <position>`, `remove <group>` | Positions start at 1 |
| `check <player> <node> [context...]` | The proxy's answer, in the player's current contexts unless given |
| `trace <player> <node> [context...]` | The answer, the winning entry (pattern, subject, rank, contexts, expiry) and why every other match lost |
| `listgroups`, `listtracks` | Groups by weight; tracks |
| `search <node>` | Who sets the node, and who holds a wildcard covering it; `%` makes it a `LIKE` pattern |
| `import <file> [--replace]`, `export <file>` | [LuckPerms JSON](#luckperms-import-and-export) (`.gz` is compressed); export never overwrites |
| `bulk [users\|groups\|all] [filters] <actions> [--dry-run]` | Patch matching nodes in one revision, see below |
| `log [--since <revision>] [--actor <actor>] [--subject <player\|group:name>] [--limit <n>]` | The change log |
| `compiled <player>` | The compiled-subject JSON a backend holds |
| `registered` | Permissions backends registered, with owner, default and description |
| `sync` | Ask every backend to resync, then list them with their revisions |
| `backends` | Backends the bus has heard from: server, revision at `hello`, last seen, client version |
| `doctor` | Problems, see [Doctor](#doctor) |

Players are names (anyone who has joined once) or UUIDs. Contexts are
`key=value` arguments (`server=survival world=world_nether`); `global` and
`server=global` mean no context. Times are durations (`90s`, `30m`, `12h`,
`7d`, `2w`, `1mo`, `1y`, combined like `1d12h`) or Unix seconds. Text with
spaces goes in double quotes: `meta setprefix 100 "&6[VIP] "`.

`bulk` filters are `key <like>` (SQL `LIKE`, `%` and `_`), `kind <kind>`,
`value <value>`, `context <k=v>` (repeat for several pairs; `context global`
selects global nodes), `source <source>` and `subject user:<player>|group:<name>`.
Actions are `delete`, `set-value <true|false>`, `set-context <k=v|global>`
(repeatable), `set-expiry <time>`, `permanent` and `set-source <source>`.
For example `bulk users key essentials.% kind permission set-value false --dry-run`.

**Authorization.** Players need `rift.perms.<verb>`, mirroring the grammar:
`rift.perms.user.permission.set`, `rift.perms.user.parent.addtemp`,
`rift.perms.group.meta.setprefix`, `rift.perms.track.append`,
`rift.perms.trace`, `rift.perms.bulk` and so on (`settemp`, `unsettemp`,
`addtemp`, `removetemp`, `settempprefix` are verbs of their own);
`rift.perms.*` grants everything. A player cannot grant a permission or group
they do not hold themselves, in the node's contexts, unless they hold
`rift.perms.escalate`; denying and removing are always allowed. In game,
`import` and `export` take a bare file name in the proxy's working directory.
Backend consoles may run commands only when their principal has
`control = true`; `rift admin perms` and the dashboard act as the console.

### Command line

```sh
rift admin perms user Alice parent add vip
rift admin perms trace Alice essentials.fly server=survival
rift admin perms log --since 400 --actor cli
rift admin perms export ./luckperms.json.gz
rift admin perms compiled Alice
rift admin perms doctor
```

The text is printed as it would appear in game; a failed command exits
non-zero. `import` and `export` paths are resolved where the CLI runs and read
or written by the proxy process.

### Doctor

`/perms doctor` lists:

- dangling parents: `inherit` nodes, or track entries, naming groups that do
  not exist (they grant nothing until the group is created again);
- unreachable groups: groups nobody inherits and no track lists;
- expired nodes the sweeper has not deleted yet (they already grant nothing);
- backends whose last `hello` reported an older revision than the proxy's,
  with no `hello` since (run `/perms sync`), and principals that never said
  `hello`;
- extension commands whose `permission` is neither registered by a backend nor
  granted anywhere (commands with an empty permission need none and are
  skipped).

`rift check --strict` warns about the last kind as well, reading the store
read-only and `extensions.groups`.

## Backends

Backends keep replicas through the [permission protocol](permissions-protocol.md)
on the [messaging bus](messaging.md). Give each one a principal with the
`backend` preset; its name must be `server.<backend>`:

```lua
rift.config.messaging = {
    listen = "0.0.0.0:7443",
    certificate = "messaging-cert.pem",
    private_key = "messaging-key.pem",
    principals = {
        { name = "server.survival-1", token_env = "RIFT_SURVIVAL_1_TOKEN", preset = "backend" },
        { name = "server.lobby", token_env = "RIFT_LOBBY_TOKEN", preset = "backend" },
    },
}
```

The proxy publishes each player's compiled subject before it forwards their
connection to a backend, so the replica usually holds it before the player
logs in, and again after every change that affects them. This needs the
backend to know the player by the proxy's UUID; Pumpkin does only with
[Velocity modern forwarding](permissions-pumpkin.md#velocity-modern-forwarding),
and otherwise fetches the snapshot at login. With a principal that may
receive these snapshots, the proxy reads the login name before connecting
even without network features. Every committed change is also appended to the
retained stream `rift.perms.changes`, file-backed beside the instance database
(`rift.perms-changes/`).

### `rift dev`

`rift dev` starts a proxy for plugin development in one command: the
permission store in memory, the bus on `127.0.0.1:7443` with a self-signed
certificate written to `./rift-dev/ca.pem`, one backend `dev` at
`127.0.0.1:25566` whose principal `server.dev` has the `backend` preset and
`control = true`, the admin service on `127.0.0.1:9091`, and the
[dashboard](#dashboard) and [HTTP API](http.md#permissions-api) on
`http://127.0.0.1:8080/`. It prints the token (`RIFT_DEV_TOKEN`, generated
unless set), a ready plugin configuration, the `rift admin perms` command line
and the dashboard URL:

```sh
rift dev --server paper --backend 127.0.0.1:25566
```

The admin service and the dashboard accept the admin token: `RIFT_ADMIN_TOKEN`
if set, otherwise the `RIFT_DEV_TOKEN` value. Enter it under API access in the
dashboard, or send it as `Authorization: Bearer <token>`. Groups edited there
reach the connected backends like any other change, and
`GET /api/perms/backends` lists them.

Options: `--listen`, `--backend`, `--server`, `--bus`, `--admin`, `--web`
(a loopback address, or `off`), `--dir`. Nothing is saved; run the backend in
offline mode.

## Dashboard

The [web dashboard](http.md) has four permission views under **Permissions**.
They call the [HTTP API](http.md#permissions-api) for everything, so an edit
there is validated, audited and pushed to backends exactly like `/perms` or a
Lua write, and the change log records it as `operator:<name>`.

- **Groups**: every group as a tree by inheritance (each group under its
  heaviest parent) with its weight, parents, node count and tracks. Create and
  delete groups, change weight and display name, and edit a group's nodes.
  Tracks are edited below the tree: add groups and put them in order with the
  Up and Down buttons, lowest first.
- **Players**: search by name, UUID prefix or group. A player's page shows
  their status, effective primary group and current contexts, lets you set the
  primary group, add and remove parent groups (with contexts and an expiry),
  promote and demote on a track, and edit their own nodes. *What backends
  see* shows the compiled subject with each entry's source group and rank, the
  resolved prefix, suffix and meta for any contexts you enter, and checks one
  node with the winning entry and why the others lost.
- **Nodes**: the registered node catalogue grouped by the backend and plugin
  that registered it, with defaults and descriptions. Choose a node to list who
  sets it directly; wildcards are not expanded there, so use a player's check
  for the full answer.
- **Activity**: the change log, newest first, filterable by actor, subject and
  revision. Open a revision to compare every node before and after.

The node editor takes a kind (permission, parent group, prefix, suffix or
meta), its key and value, contexts as `key=value` pairs separated by commas,
and an expiry (a duration from now or a date). Editing a node replaces it, so
changing its contexts moves it. Nodes from `extensions.groups` show the source
`config` and come back on the next reload if you change them here; edit those
in Lua instead.

Operators with the `perms` permission see the views without the editing
controls; `config` adds them. Weight and display-name nodes are edited through
the group's fields.

## Upgrading from extensions API v2

API v2's five fixed layers (overrides, `*` overrides, configured grants,
groups, configured `*` grants) are replaced by the weighted model above. Rift
is pre-0.1, so there is no compatibility layer.

On the first start with the permission store, Rift copies, once and in one
revision with actor `system`:

- memberships from the extension store namespace `rift.groups` as global
  `inherit` nodes, and
- the grants of `extensions.permissions`: UUID entries as user nodes, `*`
  entries as nodes of `default`.

It marks the store so this never repeats, and deletes nothing from the extension
store, so downgrading remains possible. `extensions.permissions` itself is
rejected by `rift check` and on load: after the first start copied it, Rift
refuses to start until the field is deleted. Runtime overrides were never
persisted and are not carried over.

| API v2 | API v3 |
| --- | --- |
| `rift.permissions.has(uuid, node)` | `rift.permissions.check(uuid, node) == true` |
| `rift.permissions.set(uuid, node, value)` | `rift.permissions.user(uuid):set(node, value)`; `nil` is `:unset(node)` |
| `rift.permissions.set("*", node, value)` | `rift.permissions.group("default"):set(node, value)` |
| `rift.permissions.add_group(uuid, group)` | `rift.permissions.user(uuid):add_parent(group)` |
| `rift.permissions.remove_group(uuid, group)` | `rift.permissions.user(uuid):remove_parent(group)` |
| `rift.permissions.groups(uuid)` | the `inherit` entries of `rift.permissions.user(uuid):nodes()` |
| `extensions.permissions` | user nodes (`user(uuid):set` in a callback, or the dashboard) or `extensions.groups` |

API v1 and v2 extensions keep working without `rift.permissions`: their
commands and `ctx.has_permission` are answered by the store. Group inheritance
no longer has to avoid cycles or name defined groups, and where API v2 let any
unrelated denial win, weights now decide; give the group that should win a
higher weight.
