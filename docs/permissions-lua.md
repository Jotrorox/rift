# Permissions from Lua and plugins

`rift.permissions` lets `rift.lua` read and write the
[permission store](permissions.md), react to changes and supply custom
contexts. Persistent plugins get a smaller interface, described
[at the end](#persistent-plugins).

```lua
local perms = require("rift.permissions")

if perms.check(uuid, "essentials.fly") ~= true then
    perms.user(uuid):add_parent("vip", { expires_in = 86400 * 30 })
end
```

## Calling rules

`rift.permissions` (also `require("rift.permissions")`) is available to every
configuration script.

- Calls are synchronous and never leave the process; writes have committed
  when they return.
- Calls work only in extension callbacks and jobs. While the configuration
  loads they are an error, except for the registrations
  [`on_change`](#change-events) and [`contexts.provide`](#context-providers).
- Methods of the objects that `user`, `group` and `track` return are called
  with a colon.
- A callback may write 64 times.
- Invalid arguments, unknown groups or tracks, limits and impossible track
  moves raise errors; `rift.try` catches them. A write that fails changes
  nothing.

## Functions

| Function | Result |
| --- | --- |
| `perms.check(uuid, node, contexts?)` | `true`, `false` or `nil`; without contexts, in the player's current ones |
| `perms.compiled(uuid)` | The [compiled subject](permissions-protocol.md#compiled-subject) |
| `perms.meta(uuid, contexts?)` | Prefix, suffix, meta and primary group |
| `perms.groups()` | Every group |
| `perms.user(uuid)`, `perms.group(name)`, `perms.track(name)` | An object with the methods below |

| Object | Methods |
| --- | --- |
| User and group | `:set`, `:unset`, `:add_parent`, `:remove_parent`, `:set_prefix`, `:set_suffix`, `:unset_prefix`, `:unset_suffix`, `:set_meta`, `:unset_meta`, `:clear`, `:nodes` |
| User only | `:set_primary_group` |
| Group only | `:create`, `:exists`, `:set_weight`, `:set_display_name`, `:delete` |
| Track | `:set`, `:promote`, `:demote`, `:groups` |

The arguments, by example:

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

## Write options and results

| Option | Meaning |
| --- | --- |
| `contexts` | A table of keys to a string, or to a list of strings for a custom key with several values |
| `expires_in` | Seconds from now |
| `expires_at` | Unix seconds |
| `reason` | Up to 256 bytes, recorded in the change log |

The second argument of an `unset` method is either options or a bare contexts
table. Writes return `{ revision = n, changed = true|false }`; `changed` is
false when the store already held exactly that.

## Change events

```lua
rift.permissions.on_change(function(event)
    -- { revision, actor, reason, subject, kind, key, before, after }
    rift.publish("audit.permissions", event.subject .. " " .. event.kind .. " " .. event.key)
end)
```

Register handlers while the configuration loads; setting
`extensions.on_permission_change` is equivalent. Rift calls them for each
committed change, once per effect, on the job workers with the job deadline.

| Field | Value |
| --- | --- |
| `subject` | `user:<uuid>`, `group:<name>` or `track:<name>` |
| `kind` | A node kind, or `group`, `track` or `primary_group` |
| `before`, `after` | The node, group attributes, track list or primary group on each side; `nil` when created or removed |

- At most 64 effects of one change are delivered.
- Changes the handlers make themselves are not delivered back to them.
- Events are ephemeral: a change committed during a reload may be missed.

## Context providers

```lua
rift.permissions.contexts.provide("region", function(player)
    return regions.of(player.server)    -- a string, a list of strings, or nil
end)
```

A provider computes a custom [context](permissions.md#contexts-on-the-proxy)
when a player reaches a server: after the first join and after every switch,
before the `join` and `after_transfer` callbacks. Setting
`extensions.context_providers` is equivalent.

- `player` has `uuid`, `name` and `server`.
- Keys must be custom: not `server`, `group`, `world`, `dimension` or
  `gamemode`.
- At most 16 providers.

## Persistent plugins

Persistent [plugins](plugins.md), in Lua or WebAssembly, use the store through
the `permissions` interface of the plugin contract (`rift:plugin@0.3`):

| Contract call | In the permission store |
| --- | --- |
| `has(uuid, node)` | the node in the player's current proxy contexts, like `ctx.has_permission`; undefined is false |
| `set(uuid, node, some(value))` | sets the user's global permission node; `uuid` `*` sets it on `default` |
| `set(uuid, node, none)` | unsets that global node |
| `add-group(uuid, group, expires-at?)` | sets a global `inherit` node, a membership until `expires-at` (Unix seconds) when given; adding again replaces the expiry; returns whether anything changed |
| `remove-group(uuid, group)` | unsets the global `inherit` node; returns whether anything changed |
| `groups(uuid)` | the groups the player is in, in their current contexts: direct, inherited and `default`, in rank order |
| `list-groups()` | every group as a JSON object keyed by name: `name`, `weight`, `display_name` and `nodes` |
| `meta(uuid, key)` | one meta value in the player's current contexts, or `none` |
| `set-meta(uuid, key, value, contexts-json, expires-at)` | sets the user's meta node in exactly those contexts (`none`: global), expiring at Unix seconds if given; a `none` value removes it; returns whether anything changed |

Writes are committed when the call returns and persist like any other store
change. Rift records them with actor `lua:plugin:<name>`, whatever the
plugin's engine, and the nodes carry source `plugin:<name>`.

Permission nodes with contexts or expiry, group nodes with contexts, tracks
and group edits are not part of the contract; use the dashboard, `/perms` or
`rift.lua` for them.

Persistent Lua plugins also have these functions:

| Function | Behavior |
| --- | --- |
| `rift.permissions.check(uuid, node, contexts?)`, `rift.permissions.meta(uuid, contexts?)` | As [above](#functions); without contexts they use the player's current ones |
| `rift.permissions.set_meta(uuid, key, value, options?)` | A `nil` value removes the node. Options are `contexts`, `expires_in` and `expires_at`, as for [writes](#write-options-and-results) |
| `rift.permissions.add_group(uuid, group, { expires_in?, expires_at? })` | The membership ends after that many seconds or at that Unix time |
