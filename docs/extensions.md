# Authenticated Lua extensions, API version 1

[`examples/extensions.lua`](../examples/extensions.lua) is a runnable authenticated
network with a FIFO survival queue, a staff-only server and registered commands.
It uses the existing Paper/Velocity forwarding setup in
[`examples/online.lua`](../examples/online.lua). Run `rift --check
examples/extensions.lua`, then start with `RIFT_FORWARDING_SECRET` set. Clients
must use Java 1.19.3–26.3, which supports both switching and online authentication.

Set `rift.config.extensions = { api_version = 1, ... }`, or use
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

Common context fields are `api_version = 1`, `authenticated = true`, `uuid`
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

Hooks execute serially within a session. Different sessions can run concurrently;
there is no global event order. A directly configured callback runs first,
followed by `rift.on` handlers in registration order. The first non-nil result
ends that event chain; observer callbacks must return nil. All handlers share
one execution budget.

Every transfer entry point uses `before_transfer`: `/server`, `/hub`, extension
commands, queue admissions, outage recovery and administration (including its
HTTP/messaging entry points), and BungeeCord `Connect`/`ConnectOther` requests.
Static access/health checks may eliminate a target
before this hook. `reason` is `command`, `queue`, `recovery`, `admin` or `bungeecord`. A denial
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

Each callback has a 50 ms wall deadline including blocking-worker scheduling and
configuration evaluation, 100,000 Lua instructions, an 8 MiB VM limit and a
256 KiB combined entry/module/plugin source limit. At most four extension jobs execute or wait for workers
per process, shared across reload generations. Saturation fails immediately.
Workers retain permits until execution actually stops, including after caller
cancellation. A cancelled callback may finish and publish within its remaining
budget; its returned action is discarded. A per-session worker permit prevents
later callbacks from overtaking it. No Lua VM, socket, mutable registry or lock crosses into a callback.
This extension worker limit is separate from the existing route/HTTP/message
worker limits.

Every invocation evaluates the source in a fresh restricted VM. Globals,
upvalues and context edits disappear when it finishes. Top-level configuration
code must therefore be deterministic and free of runtime side effects. Local
modules and plugins load from the saved snapshot. I/O, dynamic/native module
loading, coroutine scheduling, arbitrary native calls and timers are unavailable.
`rift.publish` becomes available only after source evaluation and uses
the existing bounded, nonblocking broker API. Publications already delivered
before a callback fails are not rolled back.

Rust owns session identity, committed server, pending transfer, permissions
snapshot, capacity leases and FIFO tickets. The only shared mutable extension
state in v1 is this host-owned admission state. Use the existing messaging API
for external observations; v1 does not add arbitrary Lua persistence or remote
permission lookups.

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

Permissions are exact nodes, granted by authenticated UUID. `*` grants nodes to
all authenticated players; there are no wildcard permission nodes, inheritance
or deny precedence. An absent grant denies. Values must be `true`.

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

## Queue behavior

`queues = { survival = 100 }` sets a process-local capacity of 100 for that
backend. It counts initial attachments and transfers in progress as well as
joined sessions. All entry paths, including administrator transfers, reserve
capacity atomically before opening the backend. During preflight the player
holds the old and target slot; failure releases the target, successful world
entry releases the old slot. It does not measure players connected directly to
Paper or through another Rift process.

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
keep their source, command registrations, UUID grants and callback semantics for
their entire lifetime, including future transfers. Permission changes therefore
apply on reconnect; reload is not an immediate permission-revocation mechanism.
Administrator transfers to newly added backends still run the player's pinned
transfer policy. Failed reloads leave the active generation intact.

Host leases, queue order and worker limits survive reload. Enabling/disabling
extensions or changing queue destinations/capacities requires restart, avoiding
conflicting capacity policies between live generations. Restart clears all
in-memory state. There is no reload callback or arbitrary state migration in v1.
