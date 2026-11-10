# Plugins: events, commands and policy

This page describes the events, commands, permissions, queues and host
services that plugins use, as the configuration script sees them: player
callbacks with a verified identity, permission-checked commands, FIFO queues,
durable state, scheduled jobs and bus decisions, in API versions 1 to 3.
Persistent [Lua](plugins-lua.md) and WebAssembly plugins receive the same
events through the [plugin contract](plugins.md), with the same validation;
[the plugin overview](plugins.md) explains the execution modes, the dispatch
order and the trust model.

[`examples/extensions.lua`](../examples/extensions.lua) is a runnable authenticated
network with a FIFO survival queue, a staff-only server, durable visit counters,
recurring jobs, permission groups and optional HTTP permission refresh.
It uses the existing Paper/Velocity forwarding setup in
[`examples/online.lua`](../examples/online.lua). Run `rift --check
examples/extensions.lua`, then start with `RIFT_FORWARDING_SECRET` set. Clients
may use any supported version, 1.8.9–26.3; Velocity forwarding needs 1.13 or
newer, so use BungeeGuard forwarding for older ones (see the README).

Set `rift.config.plugins = { api_version = 3, ... }` for the current API
(`api_version = 2` is the same without `rift.permissions`, and `api_version = 1`
is the original contract), or use
`rift.on(event, callback)` and `rift.command(name, definition)` from a script or
[folder-based plugin](lua.md). `rift.config.plugins` is the preferred name;
`rift.config.extensions` (and an `extensions` field in a returned table) is
accepted as an alias throughout 0.x, and setting both is an error. Diagnostics
and the field names on this page call the table `extensions`. Existing returned
configuration tables still work.
Unknown versions, fields, invalid registrations and missing online authentication
reject the configuration. The existing `on_route`, `on_http` and `on_message`
interfaces remain available. `on_route` still runs before the handshake and has
**no authenticated identity**; identity-based policy belongs in these new hooks.

## Callbacks and decisions

Each callback receives one owned context table. Return `nil` to continue. A
non-nil result must have exactly one of the fields permitted below. Strings must
be nonempty UTF-8, at most 1024 bytes. A deny reason may instead be a chat
component table (see [rich text](#rich-text)). Invalid results and unknown
server names are errors; Rift never treats an error as permission to proceed.

| Callback | When it runs | Additional context | Permitted result |
| --- | --- | --- | --- |
| `login(ctx)` | After Mojang authentication, before any backend DNS/connect | `default_server` | `{ deny = "reason" }` |
| `initial_server(ctx)` | After an allowed login | `default_server` | `{ server = "configured-name" }` or `{ deny = "reason" }` |
| `join(ctx)` | Once, after the first usable Join Game reaches the client | `server` | `nil` |
| `before_transfer(ctx)` | Before each eligible target connection attempt | `server`, `target`, `reason` | `{ deny = "reason" }` |
| `after_transfer(ctx)` | When an admitted transfer attempt completes or fails | `server` (source), `target`, `reason`, `success` | `nil` |
| `disconnect(ctx)` | Session teardown, including an authenticated login that never joined | Last committed `server`, or nil; `reason = "session_closed"` | `nil` |
| `dialog(ctx)` | A click on a plugin action of a [dialog](dialogs.md) the script showed (API v2+) | `server`, `action`, `values`, `payload` | `nil` |

Common player context fields are `api_version` (1, 2 or 3), `authenticated = true`, `uuid`
(lowercase 32-digit hex, no hyphens), canonical Mojang `name`, `connection_id`
(decimal string), `listener`, normalized `hostname`, `peer_ip`, and `protocol`.
`ctx.has_permission("node")` returns a boolean from the [permission
store](permissions.md), in the contexts Rift knows for the player; a node nobody
set is `false`. Call it with a dot, not a colon.
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
3. `login`, then `initial_server` (each Lua first, then [persistent plugins](plugins.md#dispatch-order), then its [bus decision](#decisions-answered-over-the-message-bus)),
   access checks and duplicate-name reservation.
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
256 KiB combined entry/module/plugin source limit. At most four player callbacks execute or wait for workers
per process, shared across reload generations. A callback waits up to 25 ms for
one of them, which counts toward its deadline only after it gets a worker; if
none frees up, it fails. Scheduled jobs have separate workers and never hold these.
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
capacity leases and FIFO tickets. API v1 retains its original admission state
contract. API v2 also owns shared storage and job scheduling, and API v3 adds
writes to the permission store. Each store/permission mutation and HTTP request
takes effect independently; later callback failure does not roll it back.

## Permissions and commands

```lua
rift.config.plugins = {
    api_version = 3,
    groups = {
        default = { permissions = { ["network.queue"] = true } },
        staff = { inherits = { "default" }, permissions = { ["network.staff"] = true } },
    },
    commands = {
        staff = {
            permission = "network.staff",
            run = function(ctx) return { server = "staff" } end,
        },
    },
}
```

Command permissions are checked against the [permission store](permissions.md)
for the authenticated UUID, in the contexts Rift knows for the player. A node
nobody set denies. Grant nodes with groups (configured as above, or at runtime),
with user nodes from `rift.permissions` in API v3, or in the dashboard. The
`extensions.permissions` grant table of earlier versions is rejected; see
[upgrading](permissions.md#upgrading-from-extensions-api-v2). A command
permission is a lowercase node without wildcards.

Register up to 64 lowercase command names using letters, digits, `_` or `-`
(maximum 64 bytes). `server` and `hub` are reserved. Each registration must have
an explicit permission and `run` function. The host checks permission **before**
invoking it, even for manually crafted command packets. A registered name
replaces the same backend command root in the advertised tree. Each player's tree
only lists the commands they have permission for. Rift rebuilds it whenever a
backend sends its command tree (after every server switch, or when a plugin
resends commands), so permission changes show in the tree from then on. Hiding a
command is not an authorization check; the permission check above is.

Command context adds `command` and trimmed `args` (the remaining unsigned text).

### Tab completion

A command can complete its arguments with `suggest`:

```lua
rift.command("ban", {
    permission = "relaxoban.ban",
    suggest = function(ctx)
        if ctx.position == 1 then return rift.players.names(ctx.word) end
        if ctx.position == 2 then
            return { { text = "1d", tooltip = "One day" }, "7d", "30d" }
        end
    end,
    run = function(ctx) ... end,
})
```

`suggest = "players"` completes every argument from the names of players on
this Rift process, and `suggest = "servers"` from the destinations the player
may join with `/server`. Neither runs Lua. A `suggest` function gets the command
context with `args` untrimmed, plus `word` (the argument being typed, possibly
empty) and `position` (its 1-based number). It returns nil or an array of up to
1024 entries. Each entry is a string or `{ text = ..., tooltip = ... }`, both
1–256 bytes without control characters, with a plain-text tooltip. It runs
under the usual 50 ms deadline, like every callback.

Rift keeps entries that start with `word` (ignoring ASCII case), drops
duplicates and sends at most 100. The client replaces `word` with the chosen
entry. Players without the command's permission get no suggestions, and the
function never runs for them. A failing function is logged and suggests
nothing. Each session gets 10 answered requests per second; the client asks on
every keystroke, and further requests get an empty answer. `/server <name>`
completes from the destinations the player may join, without extensions.
Completion needs Minecraft 1.19.3 or newer and network mode. Commands without
`suggest` show no suggestions.
Commands may return nil, `{ message = "text" }` (or a chat component table, see
[rich text](#rich-text)), `{ server = "name" }`, `{ queue = "name" }` or
`{ leave_queue = true }`. Server actions still pass
`before_transfer` and all host admission checks. Signed backend arguments stay
with their original backend; proxy arguments use unsigned Brigadier strings.
Protect destinations in both `initial_server` and `before_transfer`: a command
permission alone does not protect a server against `/server` or admin routing.

## Decisions answered over the message bus

A service with its own database, such as a ban system, can decide `login`,
`initial_server` and `before_transfer` without running in Lua. Rust sends a
request on the [message bus](messaging.md) and waits asynchronously, so a slow
responder holds no Lua worker and no extension slot. Decisions require
`api_version = 2` or later.

```lua
rift.config.plugins = {
    api_version = 2,
    decisions = {
        login = {
            subject = "relaxoban.login", timeout_ms = 2000,
            on_timeout = "deny", on_no_responders = "deny",
            deny_message = "Ban status unavailable, please retry",
            max_in_flight = 256,
        },
        before_transfer = { subject = "network.transfer", timeout_ms = 300, on_timeout = "allow" },
    },
}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `subject` | required | Literal subject the request is sent to, outside `rift.*` and `_INBOX.*` |
| `timeout_ms` | `1000` | How long to wait for a reply, 1–10000 |
| `on_timeout` | `"deny"` | `"allow"` or `"deny"` when no reply arrives in time |
| `on_no_responders` | `"deny"` | Applied at once when nobody subscribes to `subject` |
| `deny_message` | `"Unable to check your access, please try again later."` | Shown to the player when a policy denies |
| `max_in_flight` | `256` | Open requests for this hook, 1–65536. When it is full, `on_timeout` applies at once |

Decisions fail closed by default. `on_no_responders` takes effect immediately,
without waiting for the timeout, because the broker knows nobody is subscribed.
The `max_in_flight` limit protects the proxy and the responder against login
floods. Decisions can change on reload; open requests keep counting against the
limit while it stays the same.

**Order.** The Lua hook (a direct callback or `rift.on` handlers) runs first,
because it is cheap and local, then [persistent plugins](plugins.md#dispatch-order)
in `rift.plugin()` order. The bus is asked only when all of them continue. Any
deny wins. A hook without Lua code or plugins goes straight to the bus.

**Request.** A JSON object with `hook`, `uuid` (lowercase 32-digit hex),
`name`, `connection_id` (decimal string), `peer_ip`, `hostname`, `protocol`,
`default_server`, `server`, `target` and `reason`. Fields a hook does not have
are `null`, as in the Lua context.

**Reply.** One of these JSON objects, with no other fields:

| Reply | Allowed for |
| --- | --- |
| `{"action":"continue"}` | every decided hook |
| `{"action":"deny","reason":"…"}` | every decided hook |
| `{"action":"server","server":"lobby-2"}` | `initial_server` only |

A `reason` is a string of plain text or a JSON text component, an object or a
list, such as `{"action":"deny","reason":{"text":"Banned","color":"red"}}`
(see [rich text](#rich-text)). Replies are validated exactly like Lua results:
strings must contain 1–1024 bytes, a server must be configured, and an invalid reply is an error, so login
is rejected and a transfer target is skipped. A reply that arrives after the
timeout is dropped. A timed-out request may still have been processed, so
**decisions must not have side effects**.

**Responders.** Subscribe with a queue group so several instances share the
load and none is a single point of failure. A remote principal needs
`subscribe` on the decision subject and `publish` on `_INBOX.>` to reply:

```lua
principals = {
    {
        name = "relaxoban",
        token_env = "RIFT_RELAXOBAN_TOKEN",
        subscribe = { "relaxoban.login" },
        publish = { "_INBOX.>" },
    },
}
```

> **Do not run a login decider only on backends that Rift starts on demand.**
> With no responder the first login is denied, so the lobby never starts and no
> responder ever appears. Run the decider independently of the backends, for
> example as a small standalone service using a messaging client.

[`rift.events.players`](messaging.md#rift-control-subjects) reports joins, server
switches and quits, so services that only follow players need no Lua at all.

## Host services (API v2 and v3)

These APIs are available only in authenticated extension callbacks and scheduled
jobs with `api_version = 2` or `3`. They are unavailable in legacy route/HTTP/message
callbacks, except that `rift.players` also works in `on_message` and `on_http`
when extensions use API v2 or v3. Configuration loading, `rift check` and candidate reload validation
never execute jobs, HTTP requests or storage operations. Capture API references
in initialization freely (`local store = require("rift.store")`); calling them
there is an error. `require("rift.http")` and `require("rift.players")` work in
the same way. All methods use a dot, not a colon. API v3 adds
`rift.permissions`, described in [permissions](permissions.md#lua-api).

### Persistent state

```lua
rift.config.plugins = {
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
it survives process restarts in a bundled-SQLite database file. Relative paths are
resolved against the configuration file's directory; the parent directory must
already exist and be writable. Startup loads or creates the database and rejects
corrupt files, SQLite databases that are not Rift extension stores, and entries
that break the limits below. Without `storage`, the same API uses memory and
resets on restart. Each file belongs to one Rift process: Rift holds an exclusive
lock on it while running, and a second process fails at startup. It is not a
distributed database. While Rift runs, recent commits can be in a `-wal` file
next to the database; copy both, or stop Rift first, to back it up.

Stores written by earlier Rift versions (versioned binary snapshots) are
converted at startup. The original snapshot is kept as `<file>.v1-backup`; to go
back to an earlier Rift version, restore it in place of the database.
Namespaces organize data; they do not isolate mutually untrusted plugins.
Namespaces beginning with `rift.` are reserved for host-owned data, such as the
group memberships of API v2 that an upgrade copies into the permission store,
and namespaces beginning with `plugin.` hold persistent plugins' private data;
`rift.store` methods reject both.

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
Namespaces and keys are UTF-8 strings of 1–128 bytes. Values have a 64 KiB limit.
Plugin data, everything in `rift.store` namespaces, is limited to 4096 entries
and 1 MiB, counting each entry as its namespace, key and value plus 8 bytes.
Host-owned data has a separate limit of 65536 entries and 8 MiB. Exceeding a
limit fails without changing committed state.

Each mutation commits only the entries it changes, in one SQLite transaction
that is synced to disk (WAL mode, `synchronous=FULL`) before the call returns.
Memory changes only after the commit succeeds, so a failed write leaves both
unchanged. Writes are serialized; reads never wait for a write to sync.
Filesystem calls cannot be interrupted mid-operation; an expired worker retains
its permit until it stops, and a committed mutation can outlive a timed-out
callback.

### Scheduled work

```lua
rift.config.plugins.jobs = {
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
`run` function. Its context contains only `api_version` and `job`; it has no
player identity. Return nil. The first run waits a full interval, and each next
run waits a full interval after completion. Jobs never overlap by name, including
across reloads. Missed intervals are skipped, never queued for catch-up.

Jobs run on two workers of their own, so a job waiting seconds on an HTTP call
cannot make logins or commands fail. They share the 100,000-instruction and
8 MiB VM limits with player callbacks. Their wall deadline is five seconds including worker
scheduling and HTTP requests; player callbacks still have only 50 ms. When both
job workers are busy, the tick is skipped. Errors are logged and the job retries at its next interval.
Use jobs for external I/O and keep player policy checks local.

Only the committed configuration schedules jobs. Successful reload replaces the
schedule and job source; failed reload leaves both intact. Shutdown stops new
jobs before draining players. An already running callback may finish within its
remaining budget, keeping its permits and excluding the same job name in a new
generation. Schedules are process-local and do not survive downtime or promise
exactly-once execution; use idempotent external operations.

### HTTP integrations

```lua
rift.config.plugins.integrations = {
    directory = {
        url = "https://directory.example.com/permissions/",
        timeout_ms = 1000,
        headers = { ["Accept"] = "text/plain" },
        -- Header values read from the environment, never written in rift.lua.
        headers_env = { ["Authorization"] = "DIRECTORY_API_TOKEN" },
    },
}
-- Inside a job:
local response = rift.http.request("directory", {
    method = "GET", path = "0123456789abcdef0123456789abcdef",
})
if response.status == 200 then
    -- API v3; the write commits before the call returns.
    rift.permissions.user("0123456789abcdef0123456789abcdef")
        :set("network.staff", response.body == "allow")
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

Keep credentials out of `headers`: its values are part of `rift.lua`, so they
end up in deployment history and in the web editor. `headers_env` maps a header
name to an environment variable name instead, such as
`DIRECTORY_API_TOKEN`. Rift reads the variable when it sends each request, so
the value never appears in the configuration, deployment history or
diagnostics; errors name the variable only. A header may appear in `headers` or
`headers_env`, not both, and both count toward the 32-header limit. Startup and
reloads fail while a referenced variable is unset or is not a valid header
value, and `rift check --strict` reports it before you start. Set the variable
in the service environment (for example an `EnvironmentFile=` or a Compose
secret), not on the command line.

The result is `{ status = number, body = string, headers = table }`. HTTP error
statuses are ordinary responses; transport errors, timeout, malformed options
and oversized responses fail the callback. Header names in responses are
lowercase. Bodies are limited to 64 KiB, headers to 32 entries/8 KiB. Configured
`timeout_ms` defaults to 1000 and must be 1–5000; the enclosing callback's
remaining deadline always takes precedence. There are no automatic retries.

### Permissions

Permission groups, memberships, live grants and their Lua API (`rift.permissions`
with `api_version = 3`) are described in [permissions](permissions.md). The
`extensions.groups` table is mirrored into the permission store on every
committed reload; [groups as configuration](permissions.md#groups-as-configuration)
lists its fields. Persistent plugins reach the same store through the
contract's `permissions` interface, described in
[permissions](permissions.md#persistent-plugins). Backend plugins can ask
whether a player is in a group with the `RiftInGroup` request on the
[BungeeCord channel](bungeecord.md#rift-requests).

### Online players

```lua
rift.config.plugins.jobs.announcements = {
    every_ms = 300000,
    run = function()
        rift.players.broadcast("Vote for the network at example.com/vote")
    end,
}
rift.config.plugins.commands.alert = {
    permission = "network.staff",
    run = function(ctx)
        local sent = 0
        for _, player in ipairs(rift.players.list()) do
            if rift.permissions.check(player.uuid, "network.staff") == true
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
queues actions for them. It works in callbacks, jobs, and (with API v2 or v3)
`on_message` and `on_http`, so a messaging handler can relay cross-server chat or
an HTTP endpoint can kick a player. A `player` argument is a lowercase UUID
(32 hex digits, no hyphens) or a case-insensitive name. A `server` argument
names a configured backend or service group; a group matches all its instances.

| Method | Result |
| --- | --- |
| `rift.players.list(server)` | Array of player tables; `server` is optional |
| `rift.players.get(player)` | Player table, or nil when the player is offline |
| `rift.players.names(prefix)` | Sorted array of player names; `prefix` (optional) keeps names starting with it, ignoring ASCII case |
| `rift.players.message(player, text)` | Boolean: a chat message was queued; `text` is a string or a chat component table |
| `rift.players.action_bar(player, text)` | Boolean: action-bar text was queued; `text` is a string or a chat component table |
| `rift.players.title(player, title)` | Boolean: a title was queued; `title` is text or an options table |
| `rift.players.tab_list(player, options)` | Boolean: a tab list header and footer were queued; omit `options` to restore the configured ones |
| `rift.players.boss_bar(player, name, options)` | Boolean: a boss bar was queued; omit `options` to remove the bar called `name` |
| `rift.players.dialog(player, dialog)` | Boolean: a [dialog](dialogs.md) was queued; false for players older than 1.21.6 |
| `rift.players.clear_dialog(player)` | Boolean: closing Rift's dialog was queued |
| `rift.players.kick(player, reason)` | Boolean: a disconnect with this reason (a string or a chat component table) was queued |
| `rift.players.transfer(player, server)` | Boolean: a transfer request was queued |
| `rift.players.broadcast(text, server)` | Number of players a chat message (a string or a chat component table) was queued for; `server` is optional |

Player tables contain `uuid`, `name`, `server` (the last committed backend),
`connection_id` (a decimal string, matching `ctx.connection_id`) and `protocol`.
Players appear after their first usable Join Game and disappear when the session
closes. Pending logins and players on other Rift processes are not listed.

Title options are `title` and `subtitle` (at least one), plus `fade_in_ms`,
`stay_ms` and `fade_out_ms` (integers from 0 to 60000; defaults 500, 3500 and
1000; rounded down to 50 ms ticks). A title without a subtitle clears any earlier
subtitle. Titles are 1–1024 bytes of UTF-8, shown as plain text. Messages,
action bars and kick reasons are the same plain text, or chat component tables
as described under [rich text](#rich-text).

Tab list options are `header` and `footer`, each a string or chat component
table of up to 4096 bytes, with the same placeholders as the `tab_list` setting
(see [operations](operations.md#tab-list-header-footer-and-network-wide-players)).
An omitted field keeps the configured text. The player keeps this header and
footer across server switches until the next call or disconnect, and
`rift.players.tab_list(player)` returns them to the configured ones:

```lua
rift.config.plugins = {
    api_version = 3,
    join = function(ctx)
        if rift.permissions.check(ctx.uuid, "network.vip") == true then
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
rift.config.plugins = {
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
backend-initiated reconfiguration is dropped (a dialog too: configuration
closes every screen); tab lists and boss bars apply after the next join. Undelivered actions are lost on disconnect. Each
callback can make at most 256 action calls; a broadcast counts once.

A transfer takes the same path as `/server`: static access, health and draining
checks, `before_transfer` with reason `extension`, capacity and queue
reservation, then `after_transfer`. A denied or unavailable target leaves the
player where they are without a message, and the current server is never a
target. A service group selects one of its instances. A kick closes the
connection with the given reason and runs `disconnect` as usual. Invalid
arguments, unknown servers and exceeded budgets fail the callback.

[Dialogs](dialogs.md) are screens with text, inputs and buttons (Minecraft
1.21.6 and newer), described as tables in the vanilla dialog format. A button
whose `custom` or `dynamic/custom` action has an `id` without a namespace
calls the `dialog` callback with `ctx.action` (that `id`), `ctx.values` (the
player's inputs) and `ctx.payload` (the action's `payload` or `additions`,
which Rift keeps instead of sending them to the client):

```lua
rift.config.extensions = {
    api_version = 2,
    commands = {
        report = {
            permission = "network.player",
            run = function(ctx)
                rift.players.dialog(ctx.uuid, {
                    type = "multi_action",
                    title = "Report a player",
                    inputs = {
                        { type = "text", key = "player", label = "Player" },
                        { type = "text", key = "reason", label = "Reason", max_length = 256 },
                    },
                    actions = {
                        { label = "Send", action = { type = "dynamic/custom", id = "report" } },
                    },
                })
            end,
        },
    },
    dialog = function(ctx)
        if ctx.action == "report" then
            rift.publish("staff.reports", ctx.name .. " reported " .. ctx.values.player
                .. ": " .. ctx.values.reason)
        end
    end,
}
```

### Rich text

Chat messages, action bars, broadcasts, kick reasons, deny reasons and command
replies are plain text by default: a string of 1–1024 bytes, shown exactly as
written. For colours, line breaks, hover text and click events, give a
[JSON text component](https://minecraft.wiki/w/Text_component_format) instead:

- in Lua (configuration callbacks and [Lua plugins](plugins-lua.md)), a chat
  component table where the string would go, as for tab lists:
  `{ deny = { text = "Banned", color = "red" } }`,
  `rift.players.message(player, { text = "Hi", bold = true })`;
- in [bus replies](#decisions-answered-over-the-message-bus), an object or a
  list as the `reason`;
- in WebAssembly plugins, the `-json` functions of the plugin contract
  (`players.message-json`, `action-bar-json`, `kick-json`, `broadcast-json`)
  and the `deny-json` and `message-json` answers of its optional `decisions`
  export (since 0.1.2), which take the component as JSON text. The SDKs
  export `decisions` for you.

Rift never reads JSON out of a plain string, so a plugin that relays what a
player typed cannot be made to send click events. A component may be at most
32 KiB of compact JSON (a Lua table at most 1024 values in 16 levels); it must
not be empty, must not contain `null` and must have an NBT form, as every
component does. An invalid component fails the call or the decision like any
other invalid argument.

Minecraft 1.21.5 renamed `clickEvent` and `hoverEvent` to `click_event` and
`hover_event` and changed their fields (`clickEvent.value` became `url`,
`command`, `page` or `value`; `hoverEvent.contents` became `value` for text and
plain fields for items and entities). Write either form: Rift rewrites the
events of every message, action bar, kick and denial into the form each
player's client reads, and when a component carries both forms for one event
it keeps the matching one. An event the client cannot express, such as
`show_dialog` before 1.21.5, is dropped; the text stays.

A party invitation with buttons, and a ban screen:

```lua
rift.players.message(target.uuid, {
    { text = inviter .. " invited you to their party. " },
    {
        text = "[Accept]", color = "green", bold = true,
        click_event = { action = "run_command", command = "/party accept " .. inviter },
        hover_event = { action = "show_text", value = "Join " .. inviter .. "'s party" },
    },
    { text = " " },
    {
        text = "[Decline]", color = "red", bold = true,
        click_event = { action = "run_command", command = "/party decline " .. inviter },
    },
})

rift.config.plugins = {
    api_version = 3,
    login = function(ctx)
        local ban = rift.store.get("bans", ctx.uuid)
        if ban then
            return { deny = {
                text = "You are banned from this network\n\n", color = "red", bold = true,
                extra = {
                    { text = "Reason: ", color = "gray", bold = false },
                    { text = ban, color = "white", bold = false },
                },
            } }
        end
    end,
}
```

Logs and admin replies show a component's words without formatting.

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
capacity. Queue policies still require `rift.config.plugins` with online authentication.

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
schedules but preserves a configured storage database. There is no automatic
state migration or durable replay of callbacks.
