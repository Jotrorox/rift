# Hooks in `rift.lua`

`rift.lua` can handle the player events of the [plugin contract](plugins.md)
itself: decide logins and transfers, register commands, run queues and hand
decisions to a service on the message bus. This is **script mode**: the code
runs in the sandboxed configuration VM, with 50 ms per callback and no state of
its own. For more, write a [persistent plugin](plugins.md).

```lua
rift.config.plugins = {
    login = function(ctx)
        if ctx.name == "ExampleBlockedAccount" then
            return { deny = "This account cannot join this network." }
        end
    end,
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

[`examples/extensions.lua`](../examples/extensions.lua) is a runnable
authenticated network with a survival queue, a staff-only server, durable visit
counters, recurring jobs, permission groups and optional HTTP permission
refresh. It uses the Paper/Velocity forwarding setup of
[`examples/online.lua`](../examples/online.lua). Run
`rift check examples/extensions.lua`, then start with `RIFT_FORWARDING_SECRET`
set. Clients may use any supported version, 1.8.9–26.3; Velocity forwarding
needs 1.13 or newer, so use BungeeGuard forwarding for older ones
([authentication](authentication.md)).

## Set up

Set `rift.config.plugins = { ... }`, or call `rift.on(event, callback)` and
`rift.command(name, definition)` from the script or a
[folder-based plugin](lua.md). Besides the
[callbacks](#callbacks-and-decisions), the table takes `commands` and `groups`
([permissions and commands](#permissions-and-commands)), `decisions`
([bus responders](#decisions-answered-over-the-message-bus)), `queues`
([queues](#queues)), and `storage`, `jobs` and `integrations`
([host services](host-services.md)).

- Unknown fields, invalid registrations and missing online authentication
  reject the configuration.
- `rift.config.plugins` is the preferred name. `rift.config.extensions` (and an
  `extensions` field in a returned table) is accepted as an alias throughout
  0.x, and setting both is an error. Diagnostics and the field names on this
  page call the table `extensions`.
- There is one script API: every script has everything on this page.
  `api_version` is no longer needed; 1, 2 and 3 are still accepted and ignored,
  and `ctx.api_version` is always 3.
- Configurations that return a table still work, and `on_route`, `on_http` and
  `on_message` remain available. `on_route` runs before the handshake and has
  **no authenticated identity**; identity-based policy belongs in the callbacks
  on this page.

## Callbacks and decisions

Each callback receives one context table. Return `nil` to continue, or a table
with exactly one of the fields the callback permits.
[Plugin events](plugin-events.md#all-events) says when each event fires.

| Callback | Extra context | May return |
| --- | --- | --- |
| `login(ctx)` | `default_server` | `{ deny = "reason" }` |
| `initial_server(ctx)` | `default_server` | `{ server = "configured-name" }` or `{ deny = "reason" }` |
| `join(ctx)` | `server` | `nil` |
| `before_transfer(ctx)` | `server`, `target`, `reason` | `{ deny = "reason" }` |
| `after_transfer(ctx)` | `server` (source), `target`, `reason`, `success` | `nil` |
| `disconnect(ctx)` | Last committed `server`, or nil; `reason = "session_closed"` | `nil` |
| `dialog(ctx)` | `server`, `action`, `values`, `payload` ([dialogs](dialogs.md)) | `nil` |
| `chat(ctx)` | `server`, `kind` (`"message"` or `"command"`), `text`; for a command also `command` and `args` | `{ deny = "reason" }` drops it and shows the player the reason. With signed chat, the first deny costs that player their chat signing; see [chat](plugin-events.md#chat) |
| `kick(ctx)` | `server` (the backend that kicked), `reason` (its kick text without formatting). The player is still connected to Rift, on a [network](operations.md#run-a-maintenance-window) session | `{ server = "name" }` moves the player there (a group picks an instance), `{ deny = "reason" }` disconnects them with that text; `nil` applies `network.on_kick` |

- Strings must be nonempty UTF-8, at most 1024 bytes. A deny reason may instead
  be a chat component table ([rich text](players-api.md#rich-text)).
- Invalid results and unknown server names are errors. Rift never treats an
  error as permission to proceed.
- `rift.on` registers every callback except `chat`, which is set as a field
  ([registering callbacks](lua.md#registering-callbacks-and-commands)).
- Status requests keep the ordinary routing and status behavior and never run
  authenticated callbacks.

### Context fields

Every player callback has these fields. Changing `ctx` never alters Rift's
state.

| Field | Content |
| --- | --- |
| `authenticated` | `true` |
| `uuid` | Lowercase 32-digit hex, no hyphens |
| `name` | The canonical Mojang name |
| `connection_id` | A decimal string |
| `listener`, `hostname`, `peer_ip`, `protocol` | The connection; `hostname` is normalized |
| `language` | What `rift.players.set_language` stored for the player, else the client's locale once Rift knows it, else nil ([languages](plugin-events.md#the-player-directory-and-languages)) |
| `default_server` | The route or legacy `on_route` selection, or nil for an unmatched hostname. A proposal, not an established attachment; it does not expand `network.initial` |
| `server` | nil until the first join, then the last committed server |
| `ctx.has_permission("node")` | A boolean from the [permission store](permissions.md), in the contexts Rift knows for the player; a node nobody set is `false`. Call it with a dot, not a colon |

### Choosing the initial server

- `{ server = ... }` is a single configured destination. It still passes the
  network access, health, draining and capacity checks, and gets no implicit
  fallbacks.
- Returning nil keeps the ordinary initial candidate policy.
- The callback may resolve an otherwise unmatched hostname; if it returns nil
  for that hostname, login is rejected.

## Event order

For a new player the order is:

1. Legacy `on_route`, handshake, maintenance/rate admission and login parsing.
2. Mojang identity verification and encryption. A claimed Login Start UUID is
   never passed to a callback as an authenticated identity. With
   `maintenance_permission` set, maintenance is decided here instead of in
   step 1.
3. `login`, then `initial_server` (each Lua first, then persistent plugins,
   then its bus decision: [dispatch order](plugin-events.md#dispatch-order)),
   access checks and duplicate-name reservation.
4. Initial backend capacity reservation, connect, login, configuration and
   Join Game.
5. Presence/administration registration, then `join`. Registration at backend
   Login Success may be visible before Join Game; `join` waits for the world.
   The player appears in `rift.players` before `join` runs, so the callback
   can message them.

Callbacks run serially within a session. Different sessions can run
concurrently; there is no global event order.

Within one event, a directly configured callback runs first, followed by
`rift.on` handlers in registration order. The first non-nil result ends the
chain; observer callbacks must return nil. All handlers share one execution
budget.

## Transfers

Every transfer entry point runs `before_transfer`, with one of these reasons:

| `reason` | Entry point |
| --- | --- |
| `command` | `/server`, `/hub`, script commands |
| `queue` | Queue admissions |
| `recovery` | Outage recovery |
| `kick` | Backend kicks |
| `admin` | Administration, including its HTTP and messaging entry points |
| `bungeecord` | BungeeCord `Connect`/`ConnectOther` requests |
| `extension` | `rift.players.transfer` |

Static access and health checks may eliminate a target before the callback
runs. A denial prevents that target's connection; recovery may try another
eligible candidate. A backend can still reject an allowed player.

An admitted attempt has passed both the callback and host capacity reservation.
It gets one `after_transfer` call, after either a usable destination Join Game
(`success = true`) or failure (`false`).

- Denials, exhausted capacity and callback errors do not start an attempt and
  have no after event.
- While a successful transfer is committed, presence and capacity move to the
  new server before `after_transfer` runs. The source remains in `ctx.server`.
- There is no second `join` event on a transfer.

## When a callback fails

| Callback | An error, overload or missed deadline |
| --- | --- |
| `login`, `initial_server` | Rejects the login |
| `before_transfer` | Prevents that target attempt |
| A command | Produces a system message |
| Observers | Is logged; processing continues. Observers cannot cancel committed actions |

A target preflight failure preserves the old world when the protocol permits.
A failure after the transition starts closes the session, as with built-in
transfers.

On every normal return, error, task cancellation or forced shutdown, Rift
itself releases queue tickets and capacity leases. Teardown tries to run a
pending failed `after_transfer` before `disconnect`; the batch shares one 50 ms
deadline, including worker scheduling.

> [!WARNING]
> Teardown observations are **best effort**: a cancelled callback in flight,
> worker overload, shutdown or a process failure can omit them. Never make them
> responsible for essential cleanup or use them as an exactly-once durable
> event log.

`session_closed` deliberately does not promise a detailed transport failure
diagnosis.

## Limits and state

| Limit | Value |
| --- | --- |
| Wall deadline of a player callback | 50 ms, including blocking-worker scheduling and configuration evaluation |
| Lua instructions | 100,000 |
| VM memory | 8 MiB |
| Combined entry, module and plugin source | 256 KiB |
| Player callbacks executing or waiting for workers | 4 per process, shared across reload generations |
| Wait for a free worker | Up to 25 ms, counted toward the deadline only once the callback gets a worker; if none frees up, the callback fails |

Scheduled jobs have separate workers, and so do the route, HTTP and message
callbacks. A cancelled callback may finish and publish within its remaining
budget; its returned action is discarded, and it keeps its worker until it
stops. Later callbacks of the same session never overtake it.

**Nothing persists in Lua.** Every invocation evaluates the source in a fresh
restricted VM. Globals, upvalues and context edits disappear when it finishes,
so top-level configuration code must be deterministic and free of runtime side
effects. Local modules and plugins load from the saved snapshot.

Direct I/O, dynamic or native module loading, coroutine scheduling and
arbitrary native calls are unavailable. The bounded
[host services](host-services.md), including host-scheduled jobs, take their
place.

**Rift owns the state.** Each store or permission mutation and each HTTP
request takes effect independently; a later callback failure does not roll it
back. `rift.publish` becomes available only after source evaluation, and
publications already delivered before a callback fails are not rolled back
either.

## Permissions and commands

Register up to 64 lowercase command names using letters, digits, `_` or `-`
(maximum 64 bytes), as in the example at the top; `server` and `hub` are
reserved. Each registration must have an explicit `permission` (which may be
empty) and a `run` function.

**Permissions.** Rift checks a command's permission against the
[permission store](permissions.md) for the authenticated UUID, in the contexts
it knows for the player, **before** it invokes `run`, even for manually crafted
command packets. A node nobody set denies.

- Grant nodes with groups (configured in `groups`, or at runtime), with user
  nodes from `rift.permissions`, or in the dashboard.
- A command permission is a lowercase node without wildcards, or the empty
  string (`permission = ""`) for a command every player may see and run, such
  as a version command.
- An empty permission is not a node: nothing is looked up or registered for it,
  and `/perms doctor` and `rift check` skip the command.
- The `extensions.permissions` grant table of earlier versions is rejected; see
  [upgrading](permissions.md#upgrading-from-extensions-api-v2).

**The command tree.** A registered name replaces the same backend command root
in the advertised tree. Each player's tree lists only the commands they have
permission for and those with an empty permission. Rift rebuilds it whenever a
backend sends its command tree (after every server switch, or when a plugin
resends commands), so permission changes show from then on. Hiding a command is
not an authorization check; the permission check above is.

**Running.** Command context adds `command` and trimmed `args` (the remaining
unsigned text). `run` may return nil, `{ message = "text" }` (or a chat
component table, see [rich text](players-api.md#rich-text)),
`{ server = "name" }`, `{ queue = "name" }` or `{ leave_queue = true }`.

- Server actions still pass `before_transfer` and all host admission checks.
- Signed backend arguments stay with their original backend; proxy arguments
  use unsigned Brigadier strings.
- Aliases and console commands are for persistent plugins
  ([commands](plugin-events.md#commands)).

> [!WARNING]
> A command permission alone does not protect a server against `/server` or
> admin routing. Protect destinations in both `initial_server` and
> `before_transfer`.

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

- `suggest = "players"` completes every argument from the names of players on
  this Rift process, and `suggest = "servers"` from the destinations the player
  may join with `/server`. Neither runs Lua.
- A `suggest` function gets the command context with `args` untrimmed, plus
  `word` (the argument being typed, possibly empty) and `position` (its 1-based
  number). It returns nil or an array of up to 1024 entries, under the usual
  50 ms deadline.
- Each entry is a string or `{ text = ..., tooltip = ... }`, both 1–256 bytes
  without control characters. The tooltip may also be a chat component table,
  including `{ mini = ... }` ([styled text](text.md)).
- Rift keeps entries that start with `word` (ignoring ASCII case), drops
  duplicates and sends at most 100. The client replaces `word` with the chosen
  entry.
- Players without the command's (non-empty) permission get no suggestions, and
  the function never runs for them. A failing function is logged and suggests
  nothing.
- Each session gets 10 answered requests per second; the client asks on every
  keystroke, and further requests get an empty answer.
- Completion needs Minecraft 1.19.3 or newer and network mode. Commands without
  `suggest` show no suggestions.

`/server <name>` completes from the destinations the player may join, without
any of this.

## Decisions answered over the message bus

A service with its own database, such as a ban system, can answer `login`,
`initial_server`, `before_transfer` and `kick` over the
[message bus](messaging.md) instead of a Lua callback. See
[remote decisions](remote-decisions.md) for the configuration, the request and
reply format, and what happens on a timeout.

## Queues

`queues = { survival = 100 }` caps a backend at 100 players and makes the
rest wait in line, with rank priority and a bypass permission. See
[server queues](queues.md).

## Reloads

A validated reload is atomic for newly accepted connections. A failed reload
leaves the active generation intact.

- **Existing connections** keep their source, command registrations and
  callback semantics for their entire lifetime, including future transfers.
  Administrator transfers to newly added backends still run the player's
  pinned transfer policy.
- **Permission checks** read the current [permission store](permissions.md).
- **Host leases, queue order, storage, permission overrides and worker limits**
  survive a reload.
- **A restart is needed** to enable or disable `rift.config.plugins`, and to
  change the storage path or quotas or queue destinations or capacities. It
  clears in-memory state and job schedules but preserves a configured storage
  database.

There is no automatic state migration or durable replay of callbacks.
Persistent plugins reload by [their own rules](plugin-operations.md#reloads).
