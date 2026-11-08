# Permissions

Rift keeps one permission store for the whole network: users, groups,
inheritance with weights, negated nodes, wildcards, temporary nodes, contexts,
prefixes, suffixes, meta and tracks, the model LuckPerms users know. The proxy
owns the data and resolves it; backends will hold replicas and answer
`hasPermission` from memory (see the [protocol](permissions-protocol.md)).

On the proxy today the store answers extension commands, `ctx.has_permission`,
the Lua `rift.permissions` module (extensions API v3), the `RiftInGroup`
[BungeeCord request](bungeecord.md#rift-requests), the [dashboard](#dashboard)
and the [HTTP API](http.md#permissions-api).

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
  `server`, `world`, `dimension` and `gamemode` are filled by Rift and the
  backend adapters; any other key is custom (`region=spawn`). No contexts means
  global.
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

- `server`, the backend they are on. The proxy does not track `world`,
  `dimension` or `gamemode` yet; nodes that require them do not apply on the
  proxy until it does.
- Custom contexts reported by the player's backend, for keys listed in
  `permissions.contexts.external`. A report replaces the backend's earlier one
  and is accepted only while the player is on that backend.
- Custom contexts from Lua [context providers](#context-providers).

Reported and provided contexts are cleared on every server switch. Offline
players have no contexts, so only global nodes apply to them.

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
and tells subscribers, which is how backends will learn about it. Compiled
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
`name` and `server`. Keys must be custom (not `server`, `world`, `dimension` or
`gamemode`); at most 16 providers.

## LuckPerms import and export

The authority reads the JSON that `/lp export` writes, gzipped or plain:

- `group.x`, `prefix.N.text`, `suffix.N.text`, `meta.key.value`, `weight.N`
  and `displayname.x` become typed nodes; group weights and display names come
  from their global nodes.
- `{a,b}` and `(a|b)` shorthands expand into separate nodes.
- LuckPerms context sets mean "any listed value"; Rift writes one node per
  combination, and drops `server=global` and `world=global`.
- Regex nodes (`R=…`), invalid names, nodes past a limit and expiries already in
  the past are skipped, and the report lists each with the reason.

Everything lands with source `import` in one revision. Importing is additive:
nodes replace what is in their slots and nothing else is deleted. With replace,
every earlier `import` node is deleted first, in the same revision; nodes from
other sources are never touched. Export writes the same format, so `/lp import`
can read it back; a context key with several values keeps its first value
there, since LuckPerms would read the list as "any".

Over HTTP, `POST /api/perms/import` takes the file (gzipped or not, with
`?replace=true` for replace) and `GET /api/perms/export` returns it; see the
[HTTP API](http.md#permissions-api).

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
