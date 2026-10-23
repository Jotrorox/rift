# Authenticated Lua extensions, API versions 1 and 2

[`examples/extensions.lua`](../examples/extensions.lua) is a runnable authenticated
network with a FIFO survival queue, a staff-only server, durable visit counters,
recurring jobs and optional HTTP permission refresh.
It uses the existing Paper/Velocity forwarding setup in
[`examples/online.lua`](../examples/online.lua). Run `rift --check
examples/extensions.lua`, then start with `RIFT_FORWARDING_SECRET` set. Clients
must use Java 1.19.3–26.3, which supports both switching and online authentication.

Set `rift.config.extensions = { api_version = 2, ... }` for the current API
(or `api_version = 1` for the original contract), or use
`rift.on(event, callback)` and `rift.command(name, definition)` from a script or
[folder-based plugin](lua.md). Existing returned configuration tables still work.
Unknown versions, fields, invalid registrations and missing online authentication
reject the configuration. The existing `on_route`, `on_http` and `on_message`
interfaces remain available. `on_route` still runs before the handshake and has
**no authenticated identity**; identity-based policy belongs in these new hooks.

## Callbacks and decisions

Each callback receives one owned context table. Return `nil` to continue. A
non-nil result must have exactly one of the fields permitted below. Strings must
be nonempty UTF-8, at most 1024 bytes. Invalid results and unknown server names
are errors; Rift never treats an error as permission to proceed.

| Callback | When it runs | Additional context | Permitted result |
| --- | --- | --- | --- |
| `login(ctx)` | After Mojang authentication, before any backend DNS/connect | `default_server` | `{ deny = "reason" }` |
| `initial_server(ctx)` | After an allowed login | `default_server` | `{ server = "configured-name" }` or `{ deny = "reason" }` |
| `join(ctx)` | Once, after the first usable Join Game reaches the client | `server` | `nil` |
| `before_transfer(ctx)` | Before each eligible target connection attempt | `server`, `target`, `reason` | `{ deny = "reason" }` |
| `after_transfer(ctx)` | When an admitted transfer attempt completes or fails | `server` (source), `target`, `reason`, `success` | `nil` |
| `disconnect(ctx)` | Session teardown, including an authenticated login that never joined | Last committed `server`, or nil; `reason = "session_closed"` | `nil` |

Common player context fields are `api_version` (1 or 2), `authenticated = true`, `uuid`
(lowercase 32-digit hex, no hyphens), canonical Mojang `name`, `connection_id`
(decimal string), `listener`, normalized `hostname`, `peer_ip`, and `protocol`.
`ctx.has_permission("node")` returns a boolean. Call it with a dot, not a colon.
`default_server` is the route/legacy `on_route` selection, or nil for an unmatched
hostname; it is a proposal, not an established attachment. It does not expand
`network.initial`. `server` is nil until the first join, then the last committed
server. Context mutations never alter Rust state.

An explicit initial selection is a single configured destination; it still
passes the existing network access, health, draining and capacity checks. It
does not acquire implicit fallbacks. Returning nil preserves ordinary initial
candidate policy. The hook may resolve an otherwise unmatched hostname; if it
returns nil for that hostname, login is rejected. Status requests retain the
ordinary routing/status behavior and never run authenticated callbacks.

## Ordering, cancellation and failure

For a new player the order is:

1. Legacy `on_route`, handshake, maintenance/rate admission and login parsing.
2. Mojang identity verification and encryption. A claimed Login Start UUID is
   never passed to an extension as an authenticated identity.
3. `login`, then `initial_server`, access checks and duplicate-name reservation.
4. Initial backend capacity reservation, connect, login, configuration and Join Game.
5. Presence/administration registration, then `join`. Registration at backend
   Login Success may be visible before Join Game; the join hook waits for the world.
   The player appears in `rift.players` before `join` runs, so the hook can message them.

Hooks execute serially within a session. Different sessions can run concurrently;
there is no global event order. A directly configured callback runs first,
followed by `rift.on` handlers in registration order. The first non-nil result
ends that event chain; observer callbacks must return nil. All handlers share
one execution budget.

Every transfer entry point uses `before_transfer`: `/server`, `/hub`, extension
commands, queue admissions, outage recovery and administration (including its
HTTP/messaging entry points), BungeeCord `Connect`/`ConnectOther` requests and
`rift.players.transfer`. Static access/health checks may eliminate a target
before this hook. `reason` is `command`, `queue`, `recovery`, `admin`, `bungeecord`
or `extension`. A denial
prevents that target's connection; recovery may try another eligible candidate.
A backend can still reject an allowed player.

An admitted attempt has passed both the hook and host capacity reservation. It
gets one `after_transfer` invocation after either a usable destination Join Game
(`success = true`) or failure (`false`). Denials, exhausted capacity and callback
errors do not start an attempt and have no after event. While a successful
transfer is being committed, presence and capacity move to the new server before
the after hook runs. The source remains in `ctx.server` for that event. There is
no second `join` event on a transfer.

Login/initial callback errors, overload or deadlines reject login. Transfer
callback failures prevent that target attempt; command callback failures produce
a system message. Observer callbacks cannot cancel committed actions: errors are
logged and processing continues. A target preflight failure preserves the old
world when the protocol permits; failure after transition starts closes the
session, as with built-in transfers.

On every normal return, error, task cancellation or forced shutdown, Rust
synchronously releases queue tickets and capacity leases. Teardown tries to run
a pending failed `after_transfer` before `disconnect` using a bounded worker.
Teardown observations are **best effort**: an in-flight cancelled callback,
worker overload, shutdown or process failure can omit them. The teardown batch
shares one 50 ms deadline, including worker scheduling. They must never be responsible for essential cleanup or
serve as an exactly-once durable event log. `session_closed` deliberately does
not promise a detailed transport failure diagnosis.

## Deadlines and state ownership

Each player callback has a 50 ms wall deadline including blocking-worker scheduling and
configuration evaluation, 100,000 Lua instructions, an 8 MiB VM limit and a
256 KiB combined entry/module/plugin source limit. At most four extension invocations execute or wait for workers
per process, shared across reload generations. Saturation fails immediately.
Workers retain permits until execution actually stops, including after caller
cancellation. A cancelled callback may finish and publish within its remaining
budget; its returned action is discarded. A per-session worker permit prevents
later callbacks from overtaking it. No Lua VM or socket crosses between invocations. Host APIs serialize their own
state access; callbacks never receive a Rust lock or mutable registry.
This extension worker limit is separate from the existing route/HTTP/message
worker limits.

Every invocation evaluates the source in a fresh restricted VM. Globals,
upvalues and context edits disappear when it finishes. Top-level configuration
code must therefore be deterministic and free of runtime side effects. Local
modules and plugins load from the saved snapshot. Direct I/O, dynamic/native module
loading, coroutine scheduling and arbitrary native calls are unavailable. API v2
exposes the bounded host services below, including host-scheduled jobs.
`rift.publish` becomes available only after source evaluation and uses
the existing bounded, nonblocking broker API. Publications already delivered
before a callback fails are not rolled back.

Rust owns session identity, committed server, pending transfer, permissions,
capacity leases and FIFO tickets. API v1 retains its original permission
snapshot and admission state contract. API v2 also owns shared storage, live
permission overrides, permission group memberships and job scheduling. Each store/permission mutation and HTTP
request takes effect independently; later callback failure does not roll it back.

## Permissions and commands

```lua
rift.config.extensions = {
    api_version = 1,
    permissions = {
        ["*"] = { ["network.queue"] = true },
        ["0123456789abcdef0123456789abcdef"] = { ["network.staff"] = true },
    },
    commands = {
        staff = {
            permission = "network.staff",
            run = function(ctx) return { server = "staff" } end,
        },
    },
}
```

Configured permissions are granted by authenticated UUID. `*` grants nodes to
all authenticated players. An absent grant denies. API v1 accepts exact nodes
and grants (`true`) only. API v2 also accepts wildcards, denials (`false`),
permission groups and live grant/deny overrides, described below.

Register up to 64 lowercase command names using letters, digits, `_` or `-`
(maximum 64 bytes). `server` and `hub` are reserved. Each registration must have
an explicit permission and `run` function. The host checks permission **before**
invoking it, even for manually crafted command packets. Command names are
advertised to all players; visibility is not an authorization check. A registered
name replaces the same backend command root in the advertised tree.

Command context adds `command` and trimmed `args` (the remaining unsigned text).
Commands may return nil, `{ message = "text" }`, `{ server = "name" }`,
`{ queue = "name" }` or `{ leave_queue = true }`. Server actions still pass
`before_transfer` and all host admission checks. Signed backend arguments stay
with their original backend; proxy arguments use unsigned Brigadier strings.
Protect destinations in both `initial_server` and `before_transfer`: a command
permission alone does not protect a server against `/server` or admin routing.

## API v2 host services

These APIs are available only in authenticated extension callbacks and scheduled
jobs with `api_version = 2`. They are unavailable in legacy route/HTTP/message
callbacks, except that `rift.players` also works in `on_message` and `on_http`
when extensions use API v2. Configuration loading, `rift check` and candidate reload validation
never execute jobs, HTTP requests or storage operations. Capture API references
in initialization freely (`local store = require("rift.store")`); calling them
there is an error. `require("rift.http")`, `require("rift.permissions")` and
`require("rift.players")` work in the same way. All methods use a dot, not a colon.

### Persistent state

```lua
rift.config.extensions = {
    api_version = 2,
    storage = { path = "extensions.state" },
    join = function(ctx)
        local visits = rift.store.increment("visits", ctx.uuid, 1)
        rift.store.set("last_server", ctx.uuid, ctx.server)
        rift.publish("visits", ctx.uuid .. " " .. tostring(visits))
    end,
}
```

Storage is shared across sessions, jobs and reload generations. With `storage`,
it survives process restarts in a versioned binary snapshot. Relative paths are
resolved against the configuration file's directory; the parent directory must
already exist and be writable. Startup loads or creates the snapshot and rejects
corrupt or oversized files. Without `storage`, the same API uses memory and resets
on restart. Each file belongs to one Rift process; it is not a distributed database.
Namespaces organize data; they do not isolate mutually untrusted plugins.
Namespaces beginning with `rift.` are reserved for host-owned data such as group
memberships; `rift.store` methods reject them.

| Method | Result |
| --- | --- |
| `rift.store.get(namespace, key)` | Stored string, or nil when absent |
| `rift.store.set(namespace, key, value)` | Store a binary-safe string |
| `rift.store.delete(namespace, key)` | Boolean: whether an entry was removed |
| `rift.store.increment(namespace, key, delta)` | Atomically add an integer and return the result; missing values start at zero |

Increment stores a decimal string. The existing value, delta and result must be
integers in −(2^53−1) through 2^53−1, ensuring exact LuaJIT numbers; invalid values
or overflow fail without mutation.
A read followed by a write is not atomic; use increment for concurrent counters.
Namespaces and keys are UTF-8 strings of 1–128 bytes. Values have a 64 KiB limit;
the entire store, including snapshot overhead, has a 1 MiB/4096-entry limit.
Exceeding a limit fails without changing committed state. Durable mutations write
and sync a temporary sibling file before atomic replacement; memory changes only
after replacement succeeds. Unix directory syncing is best effort. Filesystem
calls cannot be interrupted mid-operation; an expired worker retains its permit
until it stops, and a committed mutation can outlive a timed-out callback.

### Scheduled work

```lua
rift.config.extensions.jobs = {
    heartbeat = {
        every_ms = 60000,
        run = function(ctx)
            rift.publish("jobs." .. ctx.job, "alive")
        end,
    },
}
```

Register up to 32 named jobs using lowercase letters, digits, `_`, `-` or `.`
(1–64 bytes). Each requires `every_ms` (an integer from 100 to 86400000) and a
`run` function. Its context contains only `api_version = 2` and `job`; it has no
player identity. Return nil. The first run waits a full interval, and each next
run waits a full interval after completion. Jobs never overlap by name, including
across reloads. Missed intervals are skipped, never queued for catch-up.

Jobs share the four-worker extension limit, 100,000 instructions and 8 MiB VM
limit with player callbacks. Their wall deadline is five seconds including worker
scheduling and HTTP requests; player callbacks still have only 50 ms. Exhausted
capacity skips a tick. Errors are logged and the job retries at its next interval.
Use jobs for external I/O and keep player policy checks local.

Only the committed configuration schedules jobs. Successful reload replaces the
schedule and job source; failed reload leaves both intact. Shutdown stops new
jobs before draining players. An already running callback may finish within its
remaining budget, keeping its permits and excluding the same job name in a new
generation. Schedules are process-local and do not survive downtime or promise
exactly-once execution; use idempotent external operations.

### HTTP integrations

```lua
rift.config.extensions.integrations = {
    directory = {
        url = "https://directory.example.com/permissions/",
        timeout_ms = 1000,
        headers = { ["Accept"] = "text/plain" },
    },
}
-- Inside a job:
local response = rift.http.request("directory", {
    method = "GET", path = "0123456789abcdef0123456789abcdef",
})
if response.status == 200 then
    rift.permissions.set("0123456789abcdef0123456789abcdef",
        "network.staff", response.body == "allow")
end
```

Up to 16 named integrations authorize only their configured HTTP(S) origin and
base path. Base URLs must have no embedded credentials, query or fragment; paths
are treated as directories. Requests use a relative `path` (empty by default),
`method` (GET by default), optional string `body`, and optional `headers` table.
Absolute URLs/paths, traversal and encoded separators are rejected. Request
headers override configured headers. Redirects are returned without following
and environment HTTP proxies are disabled. Configured services may be private
or loopback addresses; configuration authors choose the allowed endpoints.

The result is `{ status = number, body = string, headers = table }`. HTTP error
statuses are ordinary responses; transport errors, timeout, malformed options
and oversized responses fail the callback. Header names in responses are
lowercase. Bodies are limited to 64 KiB, headers to 32 entries/8 KiB. Configured
`timeout_ms` defaults to 1000 and must be 1–5000; the enclosing callback's
remaining deadline always takes precedence. There are no automatic retries.

### Permission groups and wildcards

```lua
rift.config.extensions = {
    api_version = 2,
    storage = { path = "extensions.state" },
    permissions = {
        ["0123456789abcdef0123456789abcdef"] = { ["ranks.manage"] = true },
    },
    groups = {
        default = { permissions = { ["network.queue"] = true, ["chat.color"] = false } },
        staff = {
            inherits = { "default" },
            permissions = { ["network.*"] = true, ["chat.color"] = true },
        },
        admin = {
            inherits = { "staff" },
            permissions = { ["*"] = true, ["network.shutdown"] = false },
        },
    },
    commands = {
        rank = {
            permission = "ranks.manage",
            run = function(ctx)
                -- /rank <uuid>: add a player to staff.
                rift.permissions.add_group(ctx.args, "staff")
                return { message = "Added " .. ctx.args .. " to staff" }
            end,
        },
    },
}
```

In API v2, permission tables in `permissions` and in groups map patterns to
`true` (grant) or `false` (deny). A pattern is an exact node, `prefix.*` or `*`.
`network.*` matches `network.staff` and `network.staff.kick`, but not `network`.
`*` matches every node. Within one table, the most specific matching pattern
wins: the exact node, then the longest `prefix.*`, then `*`. Checks always name
exact nodes. `ctx.has_permission("network.*")` is false, and
`rift.permissions.has` rejects it.

Define up to 64 groups. Each has an optional `permissions` table and an optional
`inherits` list of up to 16 other groups. Group names follow permission node
rules. Unknown parents and inheritance cycles fail configuration loading.
Everyone belongs to `default` when it is defined. Other memberships are stored
per player. When several of a player's groups match a node:

- A group overrides every group it inherits from, directly or transitively.
- Among the remaining matches, any denial wins.

`vip` granting `chat.color` therefore loses to `default` denying it unless `vip`
inherits `default`. Most groups should inherit `default`.

| Method | Result |
| --- | --- |
| `rift.permissions.add_group(uuid, group)` | Boolean: whether the membership was added |
| `rift.permissions.remove_group(uuid, group)` | Boolean: whether a membership was removed |
| `rift.permissions.groups(uuid)` | Sorted list of the player's stored groups, without `default` |

Membership methods take a lowercase 32-digit player UUID; `*` is rejected. Adding
requires a currently configured group other than `default`. A player can belong
to at most 16 groups. Memberships live in the extension store under the reserved
`rift.groups` namespace. With `storage` they survive restarts; without it they
reset. They count toward the store's size and entry limits, and they follow its
commit rules: a membership changes only after the snapshot is replaced. A reload
that removes a group leaves its memberships dormant: they grant nothing, can be
removed, and apply again if the group returns. Configuration has no static member
lists. Grant the first manager through `permissions`, as shown above.

### Live permissions

V2 `ctx.has_permission(node)` and command authorization consult current host
permissions each time. `rift.permissions.has(uuid, node)` checks any UUID.
`rift.permissions.set(uuid, pattern, true)` grants and `false` revokes, including
configured and group grants. The pattern may be a wildcard: setting `network.*`
to false revokes every `network.` node. Set nil to remove the override and return
to the configured policy. UUIDs use lowercase 32-digit hex, or `*` for a default
override.

Rift checks layers in this order and uses the first one with a matching pattern:

1. UUID overrides
2. `*` overrides
3. Configured UUID permissions
4. Groups
5. Configured `*` permissions

Inside a layer, the most specific pattern wins, so a broader pattern in an earlier
layer beats an exact node in a later one. At most 4096 `(UUID, pattern)` overrides
can exist. Overrides survive reconnects and reloads within this process but reset
on restart. Persist application policy in the store or group memberships, or
refresh it from a service when needed.

A committed reload updates configured v2 grants and groups for existing players
while their callback source and command registrations stay pinned. Overrides continue to
apply until explicitly reset. A candidate or rejected reload changes no live
grants. Commands check permission before invoking Lua, including after worker
scheduling; transfers and queue admission see updated permissions through their
existing `before_transfer` policy. Revocation does not eject someone already on
a server or undo an action whose authorization check has completed. The extension
example optionally refreshes a staff grant from HTTP and revokes it before each
request so a failed refresh leaves that grant denied.

### Online players

```lua
rift.config.extensions.jobs.announcements = {
    every_ms = 300000,
    run = function()
        rift.players.broadcast("Vote for the network at example.com/vote")
    end,
}
rift.config.extensions.commands.alert = {
    permission = "network.staff",
    run = function(ctx)
        local sent = 0
        for _, player in ipairs(rift.players.list()) do
            if rift.permissions.has(player.uuid, "network.staff")
                and rift.players.message(player.uuid, "[Staff] " .. ctx.name .. ": " .. ctx.args)
            then
                sent = sent + 1
            end
        end
        return { message = "Alert sent to " .. sent .. " staff members." }
    end,
}
```

`rift.players` lists the players who are in a world on this Rift process and
queues actions for them. It works in callbacks, jobs, and (with API v2)
`on_message` and `on_http`, so a messaging handler can relay cross-server chat or
an HTTP endpoint can kick a player. A `player` argument is a lowercase UUID
(32 hex digits, no hyphens) or a case-insensitive name. A `server` argument
names a configured backend or service group; a group matches all its instances.

| Method | Result |
| --- | --- |
| `rift.players.list(server)` | Array of player tables; `server` is optional |
| `rift.players.get(player)` | Player table, or nil when the player is offline |
| `rift.players.message(player, text)` | Boolean: a chat message was queued |
| `rift.players.action_bar(player, text)` | Boolean: action-bar text was queued |
| `rift.players.title(player, title)` | Boolean: a title was queued; `title` is text or an options table |
| `rift.players.tab_list(player, options)` | Boolean: a tab list header and footer were queued; omit `options` to restore the configured ones |
| `rift.players.boss_bar(player, name, options)` | Boolean: a boss bar was queued; omit `options` to remove the bar called `name` |
| `rift.players.kick(player, reason)` | Boolean: a disconnect with this reason was queued |
| `rift.players.transfer(player, server)` | Boolean: a transfer request was queued |
| `rift.players.broadcast(text, server)` | Number of players a chat message was queued for; `server` is optional |

Player tables contain `uuid`, `name`, `server` (the last committed backend),
`connection_id` (a decimal string, matching `ctx.connection_id`) and `protocol`.
Players appear after their first usable Join Game and disappear when the session
closes. Pending logins and players on other Rift processes are not listed.

Title options are `title` and `subtitle` (at least one), plus `fade_in_ms`,
`stay_ms` and `fade_out_ms` (integers from 0 to 60000; defaults 500, 3500 and
1000; rounded down to 50 ms ticks). A title without a subtitle clears any earlier
subtitle. Text, titles and reasons are 1–1024 bytes of UTF-8, shown as plain text.

Tab list options are `header` and `footer`, each a string or chat component
table of up to 4096 bytes, with the same placeholders as the `tab_list` setting
(see [operations](operations.md#tab-list-header-footer-and-network-wide-players)).
An omitted field keeps the configured text. The player keeps this header and
footer across server switches until the next call or disconnect, and
`rift.players.tab_list(player)` returns them to the configured ones:

```lua
rift.config.extensions = {
    api_version = 2,
    join = function(ctx)
        if rift.permissions.has(ctx.uuid, "network.vip") then
            rift.players.tab_list(ctx.uuid, { header = { text = "VIP Lounge", color = "light_purple" } })
        end
    end,
}
```

Boss bars are named by the extension (1–64 lowercase letters, digits, `.`, `_`
or `-`), so a later call with the same name changes that bar. Options are
`text` (required: a string or chat component table of up to 4096 bytes),
`progress` (0 to 1; default 1), `color` (`pink`, `blue`, `red`, `green`,
`yellow`, `purple` or `white`; default `pink`), `style` (`progress`,
`notched_6`, `notched_10`, `notched_12` or `notched_20`; default `progress`) and
the booleans `darken_screen`, `play_music` and `create_fog`. Rift sends only the
fields that changed. A player keeps their bars across server switches until they
are removed or the player disconnects, and can have at most 8; showing a ninth
fails the callback. Boss bars need Minecraft 1.9 or newer, so the call returns
false for 1.8.9 players. They have their own UUIDs and never touch the
backend's boss bars.

```lua
rift.config.extensions = {
    api_version = 2,
    join = function(ctx)
        rift.players.boss_bar(ctx.uuid, "event", {
            text = { text = "Build contest: 40 of 50 entries", color = "gold" },
            progress = 0.8,
            color = "yellow",
            style = "notched_10",
        })
    end,
    commands = {
        hidebar = {
            permission = "network.player",
            run = function(ctx) rift.players.boss_bar(ctx.uuid, "event") end,
        },
    },
}
```

Actions are delivered asynchronously. A call returns true once the action is
queued, or false when the player is offline or already has 32 undelivered
actions; it never waits for the client. The player's own session writes queued
actions in order while it is in a usable world, so they wait while a transfer is
in progress. An action other than a kick, tab list or boss bar that meets a
backend-initiated reconfiguration is dropped; tab lists and boss bars apply
after the next join. Undelivered actions are lost on disconnect. Each
callback can make at most 256 action calls; a broadcast counts once.

A transfer takes the same path as `/server`: static access, health and draining
checks, `before_transfer` with reason `extension`, capacity and queue
reservation, then `after_transfer`. A denied or unavailable target leaves the
player where they are without a message, and the current server is never a
target. A service group selects one of its instances. A kick closes the
connection with the given reason and runs `disconnect` as usual. Invalid
arguments, unknown servers and exceeded budgets fail the callback.

## Queue behavior

`queues = { survival = 100 }` sets a process-local capacity of 100 for that
backend. It counts initial attachments and transfers in progress as well as
joined sessions. All entry paths, including administrator transfers, reserve
capacity atomically before opening the backend. During preflight the player
holds the old and target slot; failure releases the target, successful world
entry releases the old slot. It does not measure players connected directly to
Paper or through another Rift process.

Queues may also target service groups. For a group, the configured capacity is
per registered instance: `queues = { games = 20 }` allows 40 group slots with
two instances. FIFO order and leases apply across concrete instance names, so
direct connections cannot bypass the group queue. Adding or removing instances
updates capacity without changing tickets; transfers within a group retain the
player's existing group slot. A group with no instances can collect tickets,
which its [scaling policy](managed-servers.md#automatic-scaling) uses to start
capacity. Queue policies still require authenticated extensions.

A queue command keeps the client playing on its current server. Each session can
hold one ticket. Repeating the same request returns its current position;
switching queues removes the old ticket only when the new request succeeds.
Each queue allows 1024 tickets and a five-minute wait. `/queue leave` in the
example cancels the ticket. Disconnects and successful transfers remove it.

The session checks its ticket once per second while it is in a usable world.
The FIFO head may enter when capacity is free; new direct entries cannot jump
queued players. One head completes entry before the next head is considered.
Admission rechecks permissions through `before_transfer`, static access and
health. Exhausted capacity retains the ticket. Other transfer failures remove
the ticket and tell the player; a partially completed protocol transition can
require disconnect. A stopped/reconfiguring client is bounded by the existing
session phase deadline.

Try the example with capacity 1 and two accounts: both initially enter lobby;
`/queue` moves the first account to survival; the second waits in lobby. Returning
the first account with `/hub` frees its slot and admits the second. `/server
survival` cannot bypass that queue. `/staff`, `/server staff` and the staff
hostname deny an account without `network.staff`.

## Reload behavior

A validated reload is atomic for new accepted connections. Existing connections
keep their source, command registrations and callback semantics for their entire
lifetime, including future transfers. V1 keeps its UUID grants until reconnect.
V2 uses current configured grants and runtime overrides, as described above.
Administrator transfers to newly added backends still run the player's pinned
transfer policy. Failed reloads leave the active generation intact.

Host leases, queue order, storage, permission overrides and worker limits survive
reload. Enabling/disabling extensions, changing API version, storage path or queue
destinations/capacities requires restart. Restart clears in-memory state and job
schedules but preserves a configured storage snapshot. There is no automatic
state migration or durable replay of callbacks.
