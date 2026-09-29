# Rift

A Minecraft Java Edition proxy with client-owned sessions. Route multiple
hostnames through one port or select one backend. Rift handles packet framing,
compression, and handshake, status, login, configuration and play states. Client
and backend connections have independent protocol state; a backend is an
attachment within a player's session. LuaJIT is embedded through `mlua`; the binary needs no Java or
separate Lua installation. Building requires a C toolchain (MSVC on Windows).
Windows x86-64 builds use AWS-LC's bundled assembly objects if NASM is absent;
when NASM is installed, AWS-LC assembles them from source.

Download a native archive from [Releases](https://github.com/Jotrorox/rift/releases)
and follow the [operator guide](docs/operations.md) for installation, the supplied
systemd/container examples, administration, reloads, shutdown and upgrades.
Archives include the examples and operating docs. Rift is released under the
[BSD-2-Clause License](LICENSE).

## Run

```sh
cargo build --release --locked
./target/release/rift init rift.lua
# Edit rift.lua for your network, then validate it before starting.
./target/release/rift check rift.lua
./target/release/rift --config rift.lua
```

The generated defaults accept players on port `25565` and connect to the server
on `25566`. `rift init [path]` writes a commented configuration and refuses to
overwrite existing files. For an immediate run without a file, use
`rift 0.0.0.0:25565 127.0.0.1:25566`.
With no arguments, Rift loads `./rift.lua` if present, otherwise it uses those
same addresses. Explicit CLI addresses or routing options override the file.
The listener must be an IP literal with a port. Backends accept IP literals or DNS hostnames with ports;
IPv6 works too: `rift '[::]:25565' '[::1]:25566'`.
Use `--help` for usage and `--version` (or `-V`) for the package version.
Use `--license` to print the project license and [third-party notices](THIRD_PARTY_NOTICES).
Both files are embedded at compile time, so the command works with just the binary.
Ctrl-C stops accepting connections and lets active sessions
drain for up to 30 seconds by default; a second Ctrl-C closes them immediately.

### Hostname routing

```sh
./target/release/rift 0.0.0.0:25565 \
  --route survival.example.com=127.0.0.1:25566 \
  --route creative.example.com=localhost:25567 \
  --route '*.games.example.com=mc-backend.internal:25565' \
  --default 127.0.0.1:25566
```

Point the public DNS records for both player-facing domains at Rift. Players
enter `survival.example.com` or `creative.example.com`, using the same port, and
reach different servers. Backend DNS is resolved for each connection; all
returned addresses are tried in order within one deadline (five seconds by default) covering
resolution, connection and handshake forwarding. DNS uses the system resolver
and explicit ports; Rift does not look up backend SRV records.

Routing prefers an exact hostname, then the longest matching wildcard suffix,
then the optional default. `*.games.example.com` matches one or more subdomain
labels, but not `games.example.com` itself. Names are case-insensitive and one
trailing dot is ignored. Quote wildcard arguments to avoid shell expansion.
`--route '*=host:port'` is an alternative to `--default host:port`. Duplicate
patterns or defaults are rejected. Without a default, unmatched clients receive a missing-route message.

Every listener requires a Java Edition handshake within five seconds, including
direct routes and Lua-selected backends. The initial packet body is limited to
2 KiB and the address to 1020 UTF-8 bytes and 255 UTF-16 code units. Invalid or
late handshakes close without contacting a backend. The hostname before any
NUL-delimited mod metadata selects the route; all handshake fields, including
metadata, are preserved. Packet lengths are encoded canonically.

Login supports protocol 47 (1.8), 761–775 (1.19.3 through 26.1), and the pinned
Pumpkin protocol 777. Unknown login versions receive an explanatory disconnect;
status discovery works with any protocol number, including -1. Transfer
handshakes require protocol 766 or newer. Legacy pre-1.7 pings and arbitrary TCP
streams are unsupported. There is no protocol translation or Bedrock/UDP support.

Rift answers server-list requests and ping payloads itself, even without a
backend. By default it advertises `Rift`, the client's protocol, the number of
sessions that have reached play, and the configured connection capacity. Enable
`status_cache` below to use backend status documents. Missing routes and backend
connection failures produce readable disconnect messages during login.

Do not mix a positional backend with routing options.

For a backend on the same machine, set these in `server.properties`:

```properties
server-ip=127.0.0.1
server-port=25566
online-mode=false
prevent-proxy-connections=false
```

Rift can authenticate licensed Java accounts and forward their existing UUID,
name, skin properties and player IP to Paper using Velocity modern forwarding.
Enable both settings in the returned Lua configuration table:

```lua
authentication = { online_mode = true, timeout_ms = 10000 },
forwarding = { mode = "velocity", secret_env = "RIFT_FORWARDING_SECRET" },
```

Set `RIFT_FORWARDING_SECRET` in Rift's process environment to your existing
Velocity forwarding secret, then configure every Paper backend:

```yaml
# config/paper-global.yml
proxies:
  velocity:
    enabled: true
    online-mode: true
    secret: "the same secret as RIFT_FORWARDING_SECRET"
```

Keep `server.properties`'s `online-mode=false` and `spigot.yml`'s
`settings.bungeecord=false`. Restart Paper after changing these settings. This
matches [Paper's Velocity forwarding setup](https://docs.papermc.io/velocity/player-information-forwarding/).
Backend ports must remain private. See [`examples/online.lua`](examples/online.lua)
for a two-Paper network; switching supports Minecraft **1.21.8 and 1.21.11**.

Rift owns client encryption and verifies each login with Mojang's HTTPS session
service before contacting a backend. It ignores the client's claimed UUID and
retains the verified profile across backend replacements. A wrong secret, missing
forwarding exchange, or mismatched backend identity rejects login; a rejected
replacement leaves the current server attached. Verification failures and service
timeouts never fall back to offline authentication. Inventories remain in each
backend's existing UUID-based player data; Rift does not synchronize inventories
between different worlds.

Authentication and Velocity forwarding must be enabled together. They support
Rift's modern login versions (761–775 and 777); protocol 47 requires offline
configuration. The timeout covers the encryption exchange and session lookup,
accepts 1–60,000 milliseconds and defaults to 10,000. The secret variable defaults
to `RIFT_FORWARDING_SECRET`; its value must contain 1–1024 bytes without control
characters. Use a randomly generated secret for new networks. `rift check`
validates variable names without requiring the secret; startup and reload resolve
the secret before applying the configuration. Established sessions retain their
original authentication and forwarding settings.

For backward compatibility, omitted authentication/forwarding settings use
**unauthenticated offline identities**. In that mode keep Paper forwarding disabled;
backends see Rift's IP. Use that configuration only where unauthenticated identities
are acceptable. Backends requesting their own encryption are rejected in both modes.

## Authenticated extensions

Lua extension API v1 adds authenticated login decisions, initial server selection,
join/disconnect and transfer events, plus commands with UUID permission checks.
[`examples/extensions.lua`](examples/extensions.lua) demonstrates a FIFO survival
queue and a staff-only server. Read the [extension API contract](docs/extensions.md)
for callback ordering, cancellation, deadlines, state ownership and reload behavior.
Extensions require online authentication and a switchable client protocol (Java
1.21.8 or 1.21.11); the legacy pre-handshake `on_route` hook remains available.

## Lua configuration

Generate a starter with `rift init`, adapt [`examples/rift.lua`](examples/rift.lua),
or select a file explicitly:

```sh
./target/release/rift --config examples/rift.lua
```

The file is a Lua script returning a table. For example, two listeners can route to
different servers:

```lua
return {
    listeners = {
        public = "0.0.0.0:25565",
        creative = "0.0.0.0:25567",
    },
    backends = {
        lobby = "127.0.0.1:25566",
        creative = "127.0.0.1:25568",
    },
    routes = {
        public = "lobby",
        creative = "creative",
    },
    limits = {
        max_connections = 1024,
        connect_timeout_ms = 5000,
        buffer_size = 32 * 1024,
    },
}
```

`listeners`, `backends`, and `routes` are required, nonempty tables with string
names. Listeners require IP literals with ports; backends also accept DNS
hostnames with ports. Listener port `0` asks the OS to choose an available port.
Each listener needs a route. A string value selects one backend for each session. A table enables Minecraft hostname routing:

```lua
routes = {
    public = {
        ["survival.example.com"] = "lobby",
        ["creative.example.com"] = "creative",
        ["*.games.example.com"] = "creative",
        ["*"] = "lobby", -- Optional default.
    },
    creative = "creative", -- Existing single-backend listener.
},
```

Every route target must name a configured backend. Matching and handshake
limits are the same as for CLI routes. Multiple listeners may share backends.

`limits` and each of its fields are optional and default to the values above.
`max_connections` is shared across all listeners and must be a positive integer
within Tokio's semaphore capacity. `connect_timeout_ms` accepts 1–86,400,000
milliseconds; `buffer_size` accepts 1–16,777,216 bytes and controls socket read
granularity (capped at the maximum wire frame size). All three
reject fractions, strings, and nonfinite numbers.

The default capacity is 1024 connections. Packet buffers grow as data arrives;
wire frames are limited to 2,097,151 bytes and decompressed packets to 8 MiB.
Compression thresholds, declared lengths, zlib checksums and string limits are
validated before forwarding. `buffer_size` does not cap complete packet buffers;
capacity planning must account for packet sizes as well as sockets and runtime.
The earlier raw-relay measurements in the [local pilot](docs/pilot.md) predate
this session layer.

Lua evaluates during startup and produces a typed Rust `Config`. Send SIGHUP on
Unix or Ctrl-Break on Windows to validate and reload the selected file. An optional `on_route` function also runs for each connection,
as described below. Treat configuration scripts as trusted local code. Unknown
fields, invalid types or addresses, missing references, duplicate listener
addresses, and direct proxy loops fail startup with a contextual error. Syntax
and execution errors include the filename and Lua diagnostics. A missing explicit
`--config` file is an error; only a missing implicit `./rift.lua` uses the defaults.
All listeners bind before Rift begins accepting connections.

## Web administration and status

Rift includes an optional Axum API and bundled admin website, a separate status
website/JSON endpoint, and Prometheus metrics. Edit and validate the full Lua
configuration in the browser; saves apply live with conflict detection and
atomic file replacement. Extend the website and API with sandboxed `on_http`
Lua handlers under `/ext/*`.

```sh
./target/release/rift --config examples/admin.lua
# Admin: http://127.0.0.1:8080   Status: http://127.0.0.1:9090
```

All HTTP servers are disabled by default and independently configurable.
The admin API supports bearer tokens, required for non-loopback binds. HTML,
CSS and JavaScript are embedded at compile time; no frontend build is needed.
See the [HTTP guide](docs/http.md) for configuration, API examples, Lua extension
contracts and live enable/disable behavior.

The `web` HTTP dashboard/API uses its own bearer token and is separate from the
loopback JSON-line `admin` endpoint used by `rift admin`. Both can run together
on different ports. Operational `admin.permissions` apply to the JSON-line
endpoint; a web credential grants the enabled HTTP API capabilities, including
configuration editing. The separate `status` HTTP service is unauthenticated
and read-only.

## Operating a network

[`examples/network.lua`](examples/network.lua) enables all operational features
for a primary server and a fallback lobby. Rate limits, health probes, backend status caching and the metrics listener
are disabled unless their corresponding fields are present.

### Validate and reload

```sh
./target/release/rift check ./rift.lua
kill -HUP <rift-pid>
```

`rift check [path]` (default `./rift.lua`; also spelled `--check path`) evaluates
the script and validates its configuration without binding sockets or contacting
backends. It checks syntax, top-level execution budgets,
types, routes and backend references. Hook results remain validated per invocation,
since they can depend on connection metadata.

A reload reads and validates the complete file on a blocking worker, reserves
changed HTTP/metrics sockets, then swaps one immutable configuration snapshot
for all listeners. HTTP saves, HTTP/CLI reloads and signal reloads share a
serialized transaction path. An invalid candidate is logged and counted; the
previous configuration stays active. A failed HTTP save also retains the
previous file; a rejected reload does not undo an external editor's disk changes.
Save files by atomic replacement before signaling. CLI-only invocations have no
file to reload. An implicitly loaded `./rift.lua` can be reloaded too.

Routes, backends, fallback lists, network settings, scripts, limits, health checks,
status-cache settings, maintenance/draining policy, HTTP services and the shutdown deadline
can change live. **Gameplay listener names/addresses and the operational `admin`
endpoint's bind, token-variable and permissions require a restart**; changing
them rejects the entire reload. The `web`, `status` and standalone `metrics`
servers can be enabled, disabled or moved on reload, including changes to
`web.token`. An unchanged configured port `0` retains its assigned port. All new
service sockets bind successfully before a change is applied.

Already accepted connections finish using their original routing snapshot.
Established sessions retain their sockets, buffers and original snapshot, so reloads
do not disconnect players. The player registry is shared across snapshots. Lowering
`max_connections` preserves current sessions and rejects new ones until the total
falls below the new limit. The Lua worker limit remains shared across generations.
Every successful reload invalidates status caches and starts fresh health checks.
Route changes affect new connections while existing sessions continue. Runtime
maintenance/draining overrides survive reload; a later admin command can change
them, and a restart restores the configured policy.

On Windows, Ctrl-Break reloads and Ctrl-C drains connections. On Unix, SIGINT or
SIGTERM drains; SIGHUP reloads. The startup log advertises readiness after signal
handlers are installed.

### Connection admission

```lua
rate_limit = {
    per_ip_per_second = 20,
    per_ip_burst = 40,
    global_per_second = 200,
    global_burst = 400,
    max_ips = 65536,
},
```

These are the defaults when `rate_limit = {}` is present. Token buckets refill
continuously and apply across every listener, before Lua, handshake parsing or
backend connection attempts. Excess connections close immediately. Every attempt
that passes the global bucket spends a global token, including per-IP denials.
IPv4 and its IPv4-mapped IPv6 form share one bucket. Established traffic is never
rate limited. Users behind the same NAT share their IP allowance.

The IP table is bounded by `max_ips`. When full, only completely replenished
buckets can be evicted; new IPs otherwise close. Cleanup runs at most once per
second. Unchanged rate settings preserve balances on reload; changed settings
start fresh buckets. All five values accept integers in 1–1,000,000.

`login_rate_limit` accepts the same fields and defaults, using separate buckets
after handshake parsing for login/transfer admission. Status requests do not
spend login tokens. `maintenance = true` rejects new logins while existing players
and server-list status remain available. `draining = { "survival" }` prevents new
attachments to named backends, including fallback and explicit transfers, while
preserving existing sessions. All three options are disabled by default.

### Administrator controls

```lua
admin = {
    listen = "127.0.0.1:9091",
    token_env = "RIFT_ADMIN_TOKEN",
    permissions = { "status", "maintenance", "drain", "transfer", "reload", "shutdown" },
},
```

This operational JSON-line endpoint is opt-in and loopback-only. Set the named environment variable
to a random secret of 32–1024 bytes without control characters before starting; `rift check` validates
the configuration without requiring the runtime secret. Each operation requires
both the token and its explicit permission. There is one credential/permission
set; `{ "status" }` makes it read-only. The endpoint uses JSON lines over TCP.
Its bind, token-variable and permission changes require restart. The separate
`web` HTTP server and its bearer credential can change live.

With the same token in the CLI's `RIFT_ADMIN_TOKEN` environment variable:

```sh
rift admin status
rift admin drain survival on
rift admin transfer 42 lobby
rift admin maintenance on
rift admin reload
rift admin drain survival off
rift admin maintenance off
rift admin shutdown
```

Use a `connection_id` from `status` for transfers; status also lists backend health
and draining state. The default endpoint is `127.0.0.1:9091`; select another using
`rift admin --address 127.0.0.1:9092 status`. Transfer requires a settled online
player on Minecraft 1.21.8 (protocol 772) or 1.21.11 (protocol 774), a configured eligible target and
matching UUID/name at backend login. Both the session's retained network access
rules and the current administrator-request configuration must permit the target. Unsupported clients and invalid targets stay attached to their original
backend. Once transfer has altered client protocol state, failure disconnects
the session. The operation is capped at 30 seconds and reports success when play
resumes. See the [operator guide](docs/operations.md) for secret handling,
maintenance windows, reload confirmation and service/container procedures.

### Backend outages and fallback

```lua
fallbacks = {
    survival = { "lobby", "maintenance" },
},
health_check = {
    interval_ms = 5000,
    timeout_ms = 1000,
    unhealthy_threshold = 2,
    healthy_threshold = 1,
},
```

Fallback lists contain 1–16 distinct configured backend names, excluding their
own primary. They are flat and ordered: a fallback's own list is not followed.
Direct routes, hostname routes and successful Lua backend selections all use the
selected primary's list. Unmatched routes, hook rejection and hook errors close
without attempting fallback. With no list, only the selected backend is tried.

Health checking is optional and uses a TCP connect followed by close, including
DNS resolution and proxy-loop checks. It checks reachability, not Minecraft
application readiness. There are at most 16 concurrent probes, each bounded by
`timeout_ms`; the next cycle waits `interval_ms` after all probes finish. Initially
backends are eligible while the first probes run. Consecutive failures mark a
backend down, and consecutive successes restore it. Connection attempts also
feed those thresholds. Down backends are skipped until probes recover them.
With probing disabled, each new connection tries its configured candidates again.
The durations accept 1–86,400,000 ms and thresholds accept 1–1,000.

All candidate connections share `limits.connect_timeout_ms`. Each attempt gets
a share of the remaining deadline, reserving time for later candidates even if
a primary silently drops packets. Resolution and all returned addresses are
included in that attempt. If no candidate is reachable, the client receives an unavailable-server message.
Without a `network` configuration, fallback applies to the initial TCP connection.
An explicit backend disconnect (including bans and whitelist rejections) is always
honored; it never triggers automatic fallback.

### A lobby and survival network

Use [`examples/online.lua`](examples/online.lua) for an authenticated Paper network,
or [`examples/two-server.lua`](examples/two-server.lua) for offline fixtures, with **Minecraft 1.21.8
(protocol 772) or 1.21.11 (protocol 774)** clients and backends of the same version. Other supported protocols retain ordinary
routing, initial fallback and access checks, but cannot switch within a session.
Both backends need `online-mode=false`. The online example uses Velocity modern
forwarding as described above. The offline example requires forwarding disabled
and `enforce-secure-profile=false`.

```lua
return {
    listeners = { public = "0.0.0.0:25565" },
    backends = {
        lobby = "127.0.0.1:25566",
        survival = "127.0.0.1:25567",
    },
    routes = { public = "lobby" },
    network = {
        initial = { "lobby" },
        hubs = { "lobby" },
        access = {
            survival = { allow = { "Alice", "Bob" }, deny = { "Bob" } },
        },
    },
}
```

Players join the lobby, use `/server` to list accessible servers and their current
server, `/server survival` to switch, and `/hub` to return. These commands are local
to Rift. Entry and hub lists are ordered, contain up to 16 distinct backend names,
and skip unavailable or inaccessible candidates. `initial` replaces the default
direct route's entry list; hostname routes and explicit Lua backend selections
retain their selected primary and its `fallbacks`. Denial of that selected primary
ends login instead of trying a different server.

Access rules match player names without case. Omitted rules allow access; an
explicit `allow = {}` permits nobody; `deny` overrides `allow`. Rules apply to
initial connections, server listings, commands and recovery. With online mode
enabled, names come from Mojang's verified profile. With security settings omitted,
they are **unauthenticated offline identities**. The backend also checks its own
bans and whitelist on every login.

On backend EOF or socket failure, a player in the world tries `hubs` in order,
then that backend's fallback list, excluding the failed server and duplicates.
A rejected login, configuration disconnect or play kick is not an outage and
never selects another fallback. If a requested switch fails before transition,
the player stays on the old server and receives a message. If recovery exhausts
its candidates, or a client transition fails, Rift disconnects with a reason.

A replacement logs in before the client leaves its current world. Rift verifies
the same UUID/name, re-enters configuration, forwards the replacement's registries
and Join Game, and retains the frontend socket and compression settings. The
client creates a fresh world, scoreboard and player list, resets its signed
chat chain and creates a new chat session through this sequence. Rift does not
replay old chat sessions or strip message signatures. Cached client settings and
brand are sent to the new backend, and the old resource-pack stack is cleared.
The proxy also advertises its commands in the client command tree. See the
[protocol contract and source notes](docs/network-protocol.md) for the state sequence.
Rift answers Velocity forwarding requests from its retained authenticated profile
during replacement. Other replacement login plugin requests receive an unsupported
response; login cookies and backend encryption during replacement are unsupported.
Connection and transition deadlines bound stalled switches.

The player registry tracks UUID, name and current backend across listeners and
reloads. Names are reserved before initial backend contact to prevent duplicate logins
from evicting an existing player. Duplicate UUIDs are also rejected. Switches update presence after
world entry, and disconnect/cancellation removes the registration. Existing
sessions retain their original network policy until reconnecting.

Administrators can also request an online player switch with
`rift admin transfer <connection-id> <backend>`. The operational admin
endpoint requires the explicit `transfer` permission; see
[operator controls](docs/operations.md) for setup and authentication.

### Server-list status cache

```lua
status_cache = {
    ttl_ms = 1000,
    max_entries = 1024,
    max_response_bytes = 65536,
},
```

When enabled, backend status caching applies to direct, hostname and Lua-selected
routes. Rift owns the client status exchange and queries the backend independently.
Without this option, status is answered locally without a backend connection.
Login and transfer sessions never use the cache. Disable caching if backend status
depends on information outside the handshake.

Rift caches only complete, valid JSON-object responses. Keys include the listener, selected backend,
and complete handshake fields, separating hostnames, protocol versions, ports
and mod metadata. Concurrent misses for the same key share a fill while the
bounded fill index has capacity. Responses expire after their fixed TTL; failed
fills do not populate the cache. A valid entry can answer through a brief outage,
but stale entries are never served after expiration. If a backend connection, read or timeout
fails, Rift returns a local unavailable-server status. Malformed upstream responses
are rejected and never cached. Each client's ping payload
is echoed independently and is never cached.

Both the cache and fill index are capped by `max_entries` (1–65,536). Cached
response and key bytes have an additional 64 MiB ceiling per generation; at
capacity, responses are forwarded without insertion. Individual response packet
bodies are bounded by `max_response_bytes` (1–1,048,576). TTL accepts
1–86,400,000 ms. Status request and ping phases each have a five-second deadline;
the fill, including waiting for a concurrent fill, connecting and reading the
backend response, shares `connect_timeout_ms`. Successful reloads clear the cache.

### Metrics and shutdown

```lua
metrics = "127.0.0.1:9090",
shutdown_timeout_ms = 30000,
```

The optional HTTP listener serves Prometheus text at `GET /metrics`. Bind it to
loopback or a trusted monitoring interface: it has no authentication. Scrapes
use Axum with bounded HTTP requests, independent of gameplay admission. Other
paths return 404. The separate status server can also expose `/metrics`; see
the [HTTP guide](docs/http.md).

Metrics include `rift_connections_active`, `rift_players_online`, accepted/completed/rejected connection
counters, connection errors, backend connect failures, fallback selections,
cache hits/misses, successful/failed reloads, health probes, forced shutdowns,
and client bytes read/written. `rift_backend_players_online{backend="name"}`
counts players attached to each backend. `rift_backend_up{backend="name"}` reports each
configured backend's eligibility (initially 1; always 1 when checks are disabled).
This is health status, separate from administrative draining.
`rift_backend_draining{backend="name"}` and `rift_maintenance_mode` report the
effective administrative state, including runtime overrides.
Counters survive reloads. Bytes update during live sessions and include failed
sessions; no peer-address or hostname labels create unbounded metric cardinality.

`rift_login_duration_seconds` is a histogram from TCP acceptance through first
settled play, including routing, handshake and login/configuration. Interpret its
buckets/sum/count with errors: failed logins do not contribute successful latency.
`rift_logins_rate_limited_total` and `rift_connections_access_rejected_total`
track login admission and maintenance rejection. Transfer results use
`rift_player_transfers_total` and `rift_player_transfer_failures_total`.
`rift_process_resident_memory_bytes` reports Linux process RSS;
`rift_process_resident_memory_available` indicates whether sampling is supported
and succeeded. When unavailable, the byte gauge is omitted.

Shutdown closes gameplay listeners and stops health probes, then drains all
accepted sessions, including pending hooks and handshakes. The metrics listener
stays available during the drain. The default deadline is 30 seconds; set
`shutdown_timeout_ms` to an integer in 1–86,400,000 ms. On expiry, or a second
shutdown signal, remaining sessions close and the process exits. TCP half-close
behavior is preserved throughout a normal drain.

### Connection failure events

Connection failures and rejections emit one JSON object per line to stderr,
without requiring the metrics listener. Startup, reload and shutdown messages
remain plain text. For example:

```json
{"event":"connection_failed","timestamp_unix_ms":1790598574204,"connection_id":42,"listener":"public","peer":"192.0.2.10:51234","backend":"survival","backend_address":"127.0.0.1:25566","stage":"connect","failure":"connect_error","duration_ms":1.234,"error_kind":"ConnectionRefused","message":"Connection refused (os error 111)"}
```

`connection_id` correlates events within one process. `duration_ms` is elapsed
monotonic time since TCP accept, including hook execution and all fallback
attempts; `timestamp_unix_ms` is the event's wall-clock time. `backend` is the
configured name of the selected or most recently attempted backend, with its
configured target in `backend_address`. Both are `null` before selection, such
as when a handshake or hook fails. After fallback, subsequent failures identify
the backup actually used.

| Stage | Failure | Meaning |
| --- | --- | --- |
| `handshake` | `handshake_error` | Invalid, truncated or timed-out client handshake |
| `route` | `no_route` | No configured hostname route matched |
| `on_route` | `lua_overload` | All Lua worker slots are occupied |
| `on_route` | `script_timeout` | The asynchronous hook deadline expired |
| `on_route` | `script_error` | Lua execution/budget error or malformed decision |
| `on_route` | `unknown_backend` | The hook returned an unconfigured backend name |
| `dns` | `dns_error` | Backend resolution failed, returned no addresses or timed out |
| `connect` | `connect_error` | TCP connection or loop protection failed, or connect timed out |
| `connect` | `no_eligible_backend` | No primary or fallback was eligible |
| `admission` | `rate_limited`, `capacity_exhausted` | Admission closed the connection |
| `login_admission` | `login_rate_limited`, `maintenance` | Login was denied before contacting a backend |
| `transfer` | `transfer_failed` | Replacement backend login/configuration failed; the session closed |
| `on_route` | `route_rejected` | The hook explicitly rejected the connection |

Other I/O failures use `failure="io_error"` with `stage` set to `client_setup`,
`backend_setup`, `handshake_write`, `login`, `configuration`, `play`, `status_request`, `status_cache_wait`,
`status_upstream` or `status_response`. `error_kind="TimedOut"` distinguishes I/O
deadlines from other errors at the same stage. Lua diagnostics retain the script
filename and hook context. Messages are capped at 2,048 characters and JSON
escapes embedded newlines and control characters; handshake payloads are not logged.

`backend_status_failed` records a failed status read or timeout when a local
status response is served instead. `backend_attempt_failed` records each failed backend attempt, including attempts
recovered by a fallback. Only `connection_failed` means the session ended in an
error. Policy/admission closures use `connection_rejected`; hook rejection
reasons appear in `message`. These events preserve the existing error/rejection
counters and do not log successful sessions or individual traffic packets.
`admin_command` records completed or rejected administrator operations with
`permission`, `outcome` and a bounded diagnostic message; credentials are omitted.
For example, filter a combined stderr log with
`jq -R 'fromjson? | select(.event == "connection_failed")' rift.log`.

## Routing hook

Add `on_route` to the returned configuration table. Rift calls it once after TCP
accept and before connecting to any backend or reading client bytes:

```lua
on_route = function(connection)
    if connection.peer_ip == "192.0.2.10" then
        return { reject = true, reason = "Access denied" }
    end
    if connection.listener == "creative" then
        return { backend = "creative" }
    end
    return nil -- Use routes[connection.listener].
end,
```

[`examples/routing.lua`](examples/routing.lua) is a complete configuration. Every
listener still needs a configured route. Omitting `on_route` keeps that policy and
does not invoke Lua during connection handling.

`connection` is a fresh table containing:

| Field | Value |
| --- | --- |
| `listener` | Configured listener name |
| `peer_addr`, `local_addr` | Actual accepted socket endpoints as IP:port strings; IPv6 uses brackets |
| `peer_ip`, `local_ip` | IP strings without ports or brackets |
| `peer_port`, `local_port` | Integer ports; local port reflects the OS-selected port when configured as `0` |
| `default_backend` | Direct backend name, or the hostname table's `"*"` default; `nil` if absent |

There is no Minecraft hostname, player identity, or packet data at this stage.
Changing this table does not change the socket or configured route. A backend
selection bypasses hostname matching; it still enters the Minecraft session layer. Returning
`nil` preserves hostname parsing, matching, handshake deadlines and forwarding;
it does not jump directly to the hostname table's `"*"` fallback. Script-selected
backends retain DNS resolution, connection deadlines and proxy-loop checks.

Return `nil` to continue the configured direct or hostname routing policy,
`{ backend = "name" }` to select a configured backend, or `{ reject = true, reason = "optional explanation" }` to close TCP.
Rejection reasons must be UTF-8 strings of at most 1,024 bytes; they are available
in the Rust result and structured rejection event but are not sent to the client. Rift sends no Minecraft
disconnect packet. Unknown fields, conflicting choices, wrong types and unknown
backend names are errors. Arbitrary backend addresses are not accepted.

Startup syntax errors, invalid configuration, or a non-function `on_route` fail
startup. During routing, script errors, invalid decisions, exceeded budgets and
worker overload **fail closed**: only that connection closes, with an error on
stderr as a [structured event](#connection-failure-events). Lua errors include the script filename and hook context. There is no
automatic fallback after an error; a successful `nil` result explicitly selects
the configured policy. Displayed script diagnostics are capped at 2,048 characters,
and unknown backend names at 256 characters. Backend connection failures retain the
usual connection timeout.

Each invocation creates a fresh Lua state and re-evaluates the active configuration
source snapshot, including its top-level code, before calling the hook. Globals and
closure upvalues are private to that invocation and never persist across clients.
The file is read again only on an explicit reload. Keep top-level initialization small and deterministic.
Independent calls can run concurrently, and their completion order is unspecified.

The initial limits are fixed in the implementation:

| Resource | Limit |
| --- | --- |
| Script source | 256 KiB |
| Lua allocator per state | 8 MiB |
| Lua instructions per evaluation, including initialization and hook | 100,000, checked every 1,000 instructions |
| Routing deadline, including blocking-worker scheduling and initialization | 50 ms |
| Admitted script jobs across all listeners | 4; excess calls close immediately |

Lua runs through Tokio's blocking pool, with admission acquired before scheduling.
There is no application waiting queue. Timed-out or cancelled calls retain their
worker permit until execution actually stops, so they cannot accumulate unlimited
background jobs. Existing relays never acquire a script permit or access a Lua
state. Pending routing also counts against `max_connections`.

Startup evaluation uses the same instruction, memory and elapsed-time limits.
LuaJIT compilation is disabled so instruction hooks remain effective. Deadlines
are checked at hooks and when native operations return; they are not hard
real-time preemption. The async caller also has a deadline. This is an in-process
environment for trusted scripts, not process isolation for hostile code.

Both startup and routing expose basic Lua operations (`assert`, `error`, `ipairs`,
`next`, `pairs`, `select`, `tonumber`, `tostring`, `type`, `unpack`), `math`,
`string.byte/char/len/lower/rep/reverse/sub/upper`, and
`table.concat/insert/remove/maxn`, plus `_G` and `_VERSION`. File/process I/O,
module and code loading, FFI, JIT controls, debug access, coroutines,
`pcall`/`xpcall`, metatable manipulation and finalizers are unavailable. Native
pattern matching and sorting are also omitted to avoid work outside instruction
hooks. `table.insert` runs in Lua under the instruction budget and requires a
position in `1..=#table+1`. A script cannot catch a budget error or replace the
execution hook.

The library exposes owned Rust `ConnectionInfo`, `RouteDecision` and `RouteError`
types in [`src/hooks.rs`](src/hooks.rs), plus `Router::route` and backend lookup.
The boundary carries no Lua values, VM handles, socket ownership, or borrowed
strings. A future C ABI can marshal endpoints and UTF-8 strings into these types
and expose a tagged backend/rejection result with explicit ownership. The current
Rust structs and enum are not a stable C layout; no C ABI is exported yet.

## Plugin messaging

Rift includes a shared async pub/sub broker for native Rust components, Lua
hooks and external server plugins over authenticated QUIC. It supports wildcard
subjects, queue groups, request/reply, bounded subscriber queues and optional
retained streams with durable consumers and acknowledgements. Control subjects
expose status, maintenance, draining, player transfers and reloads.

See the [messaging guide](docs/messaging.md), [Lua examples](docs/messaging-lua.md)
and [QUIC wire protocol](docs/messaging-protocol.md). The UDP listener is opt-in;
local messaging is available without opening a port. This is a Rift protocol
inspired by NATS and JetStream, not a NATS-compatible server. Measure local and
QUIC latency with `cargo run --release --example messaging_bench -- 20000`.

## Implementation

- One async task per connection on Tokio's multithreaded runtime.
- `TCP_NODELAY` on both sockets for small-packet latency.
- Bounded packet buffers with 32 KiB socket read granularity by default, backpressure,
  and client half-close draining. Partial reads survive `select!` cancellation.
- Five-second handshake deadline on every listener; a separate backend
  DNS/connect/handshake deadline (five seconds by default). Login/configuration
  and client half-close draining have a 30-second deadline. No play idle timeout.
- By default, at most 1,024 active connections across all listeners, including
  pending script calls, handshakes and backend connections;
  excess clients are immediately closed. The OS file descriptor limit must allow
  two sockets per client plus headroom.
- No per-packet logging, shared traffic lock, or unbounded queue. Traffic metrics
  use relaxed atomic counters per socket I/O; status JSON is parsed only when
  status caching is enabled. Compression uses an in-tree safe Rust zlib codec,
  with no added Cargo or native-library dependencies.

## Tests

```sh
cargo fmt --all --check
cargo test --locked --all-targets
cargo test --locked --all-targets --release
cargo clippy --all-targets --locked -- -D warnings
python3 tests/protocol_wire.py --binary target/release/rift
python3 tests/network_wire.py --binary target/release/rift
python3 tests/network_minecraft.py --accept-eula --binary target/release/rift
python3 -m unittest discover -s tests -p 'test_*.py' -v
python3 tests/minecraft.py --accept-eula --jobs 3
python3 tests/minecraft.py --accept-eula --jobs 3 --compression disabled
python3 tests/bench.py --report target/benchmark.json
```

The network wire test exercises lobby/survival/lobby and recovery on one client
socket, access denials, explicit bans, configuration, independent compression,
world entry and fresh chat sessions. The real network test runs a pair of pinned vanilla
servers for **each switchable version (1.21.8 and 1.21.11)**, checks fresh world,
chunk, teleport and command-tree data plus chat after each switch, kills survival
to verify recovery, and checks actual login/play bans. Both versions run by default
and in CI; `--server vanilla-1.21.8` selects just the additional version.
The fixture matrix in `tests/servers.json` is checked against the Rust switching
capabilities. Adding relay support alone never enables switching.

The protocol wire test uses independent Python/zlib peers across ten protocol versions and
four compression thresholds, without downloads or a Minecraft server. The control
packet tables follow [minecraft-data](https://github.com/PrismarineJS/minecraft-data/tree/master/data/pc)
and the pinned Pumpkin fixture; zlib follows [RFC 1950](https://www.rfc-editor.org/rfc/rfc1950)
and [RFC 1951](https://www.rfc-editor.org/rfc/rfc1951).

Rust is pinned in `rust-toolchain.toml`; install it with rustup. The Python scripts
need Python 3.11+ and use only the standard library. Vanilla and Paper also need
Java 21; the pinned Pumpkin binary needs Linux x86_64. Server downloads require
internet access on the first run. `--accept-eula` accepts
the [Minecraft EULA](https://aka.ms/MinecraftEULA) for the local test servers.
CI passes this flag for its disposable fixtures. Servers bind only to loopback,
use offline mode, and stop with their proxies when testing finishes.

The harness pins the [official Mojang server](https://www.minecraft.net/en-us/download/server)
at **1.21.8 and 1.21.11** and [Paper](https://docs.papermc.io/misc/downloads-service/) at
**1.21.11 build 132**, plus [Pumpkin **0.2.0+26.3-26.51**](https://github.com/Pumpkin-MC/Pumpkin/releases/tag/0.2.0%2B26.3-26.51)
for Minecraft **26.3**. Versions, URLs and publisher checksums live in
[`tests/servers.json`](tests/servers.json); every download and cache hit is verified.
Each run creates a fresh world under `target/minecraft/runs/`, retains logs and a
`result.json`, and checks:

- Direct/proxied status equality and exact ping payloads.
- 64 status requests with 16 concurrent clients.
- Direct and proxied login, compression in both directions, configuration,
  teleport acknowledgement, world chunks, and two play keepalive exchanges.
- Recovery when the backend starts after Rift, and clean handling of its shutdown.
- Two servers with distinct MOTDs behind one routing listener: both domains,
  backend DNS, wildcard/default routes, concurrent status requests and full
  gameplay through each domain. Each test job briefly runs two server processes.
- On Linux, a combined operational scenario with health probes, fallback, rate
  limits, status caching, metrics and reloadable Lua configuration enabled:
  12 route reloads interleaved with invalid candidates and 256 status requests per
  round (16 concurrent, including unique cache keys and distinct ping payloads).
  Two established gameplay sessions receive and acknowledge teleports after
  every round and continue exchanging keepalives without reconnecting.
- Capacity reduction and rate-limit bursts while established players continue;
  real primary shutdown, fallback login, health recovery and fresh primary login
  while the lobby session survives; gameplay through graceful drain, shutdown
  deadline and a second shutdown signal. A player on the stopped backend is
  expected to disconnect; established sessions are never migrated.

The operational scenario samples **Rift's process**, excluding the game servers,
every 50 ms using Linux `/proc`. After warmup, every quiet sample must stay within
8 file descriptors and 6 sockets of baseline; all samples must stay within
16 MiB RSS and 148 descriptors/sockets of baseline. The peak socket allowance
covers 64 admitted connections with two sockets each, metrics and health probes.
These are fixed regression budgets for this workload, allowing allocator retention;
they do not establish an arbitrary-duration or player-capacity bound. Reports
include baseline, peak and settled measurements, gameplay checkpoints and metrics;
`operations/resources.jsonl` retains the sample timeline even on failure.
Use `--operation-rounds 100` for a longer run (minimum 4, default 12).
Non-Linux runs explicitly report the operational scenario as skipped; all six
Linux CI Minecraft jobs require it in both compression modes.
See the [recorded operational results](tests/OPERATIONS_RESULTS.md) for the local
six-case matrix and an additional 100-reload run.

Use `--server paper` to select one backend (repeat the option to select several),
`--binary /path/to/rift` to test an existing build, and `--report path.json` to
collect results. On macOS or Windows, select `--server vanilla --server paper`.
`--jobs 3` runs the backends concurrently; the default of one limits local resource
use. Downloads are cached separately from worlds. Do not run Python with `-O`,
which disables test assertions; the scripts reject it.

Rust tests additionally check simultaneous bulk transfer byte for byte, slow
readers, fragmented concurrent sessions, half-closes in both directions, connection
refusal, CLI startup failures, bounded handshake parsing and deadlines, and
hostname routing against independent Minecraft login fixtures. Protocol tests
cover fragmented/cancelled reads, compression against independent zlib streams,
state acknowledgements, structured disconnects and backend replacement. Hook tests cover
backend selection/rejection, malformed results, execution limits and cancellation,
state isolation, and traffic continuing alongside a runaway hook. Operational tests
cover failed and successful reloads with live sessions, shared admission and Lua
capacity across reloads, primary outage/fallback/recovery, health transitions,
status TTL and concurrent fills, malformed status responses, metrics, graceful
drain and forced shutdown. Signal-driven executable tests run on Unix; the other
regressions also run on Windows.
Authentication tests cover encrypted transport, Mojang response validation,
authentication outages and rejection of unverified identities. Forwarding tests
cover signed profiles, plugin-response spoofing, and initial/replacement login.
The [manual online-mode procedure](tests/MANUAL_ONLINE.md) covers a licensed account
joining and switching between two real Paper servers with UUID, skin and inventory
checks. Its visual checks require a signed-in client and explicit operator evidence.

## CI and releases

[`CI`](.github/workflows/ci.yml) runs on pull requests, pushes to `master`, merge
queues, manual dispatch, and weekly. Quality, dependency, regression and Linux
build jobs run in parallel; Minecraft and benchmark jobs then share that run's
Linux executable:

| Job | Required checks |
| --- | --- |
| Quality | Rust formatting, Clippy with warnings denied, offline harness tests, actionlint |
| Dependencies | `cargo audit --deny warnings` against the current RustSec database |
| Regression | Debug and release tests on Linux x86_64, macOS ARM64, Windows x86_64; package smoke tests |
| Linux build | Build one release executable for all Minecraft and benchmark jobs |
| Minecraft | Six jobs: vanilla, Paper, Pumpkin × compression enabled/disabled |
| Benchmark | Transfer correctness plus downloadable latency/throughput measurements |

All matrix jobs finish even when a sibling fails. The final **CI ready** check fails
if any required job fails, is cancelled, or is skipped. Configure branch protection
to require **CI ready** and require pull requests before merging; workflow files
cannot enable repository protection by themselves. Fork PRs need no custom secrets
or write token. Actions are pinned by commit, Cargo uses `--locked`, and Dependabot
proposes weekly Cargo and action updates.

Rust dependency caches include the runner OS/image, architecture, compiler,
build environment, Cargo manifests and lockfile. Clippy has its own cache;
regression jobs own the debug/release caches, with the Linux build restoring
the Linux regression cache without competing to save an incomplete version.
Cargo still checks and builds the current source on every run. The shared Linux
executable is passed as an artifact from the same workflow run, with executable
permissions preserved, so test jobs avoid seven separate builds and toolchain
setups. Stable and nightly releases reuse this workflow and its caches.

`cargo-audit` is cached by tool version, runner and pinned Rust toolchain,
independently of Rift's dependencies; every audit fetches current advisories.
The actionlint archive is cached by version and checksum and verified before
execution, including on cache hits. Server downloads are cached separately by
server checksum, shared across compression modes and verified on every use;
updating one server leaves the other download caches usable. Only default-branch
runs save caches, and only one compression mode saves each server download.
Pull requests and tags restore those caches and can build/download normally
on a miss. Test worlds, logs, reports and check results are never cached.

The Minecraft jobs upload reports, proxy/server logs and crash reports even on
failure. Logs, benchmark reports, the shared executable and native release
packages are retained for 14 days.
Benchmark numbers are informational because shared runners have variable load;
functional assertions still fail the job. Compare JSON reports from repeated runs
on the same hardware before setting a performance budget.

[`Release`](.github/workflows/release.yml) runs when a `v*` tag is pushed. The tag
must exactly match `Cargo.toml` (for example, version `0.2.0` requires `v0.2.0`).
It reruns the entire CI workflow on that tag, then publishes the tested Linux,
macOS and Windows archives with `SHA256SUMS` to a GitHub Release. Only the final
publishing job receives write permission; it uses the workflow's built-in token.
This distributes the standalone proxy; running servers are managed separately.
Each archive includes the binary, BSD-2-Clause license, third-party notices, README,
all bundled Lua examples, the systemd unit, Dockerfile/Compose examples and
operator/pilot documentation. Packaging extracts the archive,
checks `--version` against `Cargo.toml`, runs `--help`, verifies `--license` without
external license files, validates every bundled configuration with `--check`,
and smoke-tests generated configuration and overwrite protection with `init`/`check`.
Build the same archive locally after a release build:
`python3 scripts/package.py --platform linux-x86_64` (or `macos-aarch64` /
`windows-x86_64` on the matching native host).

[`Nightly`](.github/workflows/nightly.yml) checks for new commits daily at **01:17
UTC** and weekly on **Monday at 02:47 UTC**. Each cadence tracks its own previous
published nightly, skips builds and publication when no commits have changed,
and runs the full CI workflow before publishing a GitHub prerelease. Releases use
dated `nightly-daily-*` or `nightly-weekly-*` tags and include the tested Linux,
macOS and Windows packages, `SHA256SUMS`, and `CHANGELOG.md`. The release notes and
changelog list every new commit's subject and linked full hash. The first release
for each cadence includes the entire commit history; failed runs leave those
commits for the next successful release. Stable releases remain the latest release.
You can also run Nightly manually and select either cadence. Scheduled runs use
the default branch and become active once the workflow is merged there.

When updating a server fixture, update its URL, version, checksum and protocol in
`tests/servers.json`, adjust `PROTOCOLS` and packet handling in `tests/minecraft.py`
against the upstream protocol/source, and run both compression modes locally.
Pumpkin's protocol source is pinned by `source_commit` in the fixture file. Keep
full gameplay checks required when upgrading; a status ping alone is insufficient.
Update `rust-toolchain.toml` explicitly when upgrading Rust, then run the full suite.

## Benchmark

```sh
python3 tests/bench.py --report target/benchmark.json
# Longer connection-only sweep using an already built release binary:
python3 tests/bench.py --binary target/release/rift --skip-throughput \
  --attempts 8192 --burst-sizes 1,4,16,64,256 --report target/benchmark-bursts.json
```

The standard-library harness runs a separate asynchronous Python backend process
and measures six paths:

| Scenario | Connection path |
| --- | --- |
| `direct` | Minecraft login and play packets straight to the echo fixture |
| `rift` | Rift's single-backend Minecraft session |
| `hostname` | Minecraft login handshake, exact hostname match, then relay |
| `lua` | Fresh Lua VM/config evaluation, a minimal `on_route` returning `nil`, then hostname routing |
| `lua_init` | Same hook and route, with a 10,000-entry table rebuilt during each config evaluation |
| `status_cached` | Hostname status request served from a primed cache; separate ping/pong per client |

Use `--scenarios` to select paths and `--lua-init-iterations` to vary initialization
work within the existing script budget. The benchmark uses the existing four Lua
job slots, immediate overload rejection, fresh VM per invocation, and 50 ms
execution deadline. It does not alter the routing implementation.

By default each path attempts 2,048 connections at each burst size: 1, 4, 16, 64
and 256. Clients are released together at an asyncio gate; the next wave starts
when the entire previous wave finishes. The final wave can be smaller. There are
no retries. Sixteen sequential warmup connections precede each measurement, and
each proxy scenario starts a fresh process. Rate limits and health probes are
disabled; the normal 1,024-connection admission limit remains in place. Larger
custom bursts can exercise admission rejection; the driver allows up to 4,096.

Setup latency starts before TCP connect and ends after an offline 1.8 login and
a verified play-packet echo, establishing that routing and backend setup completed.
The synthetic fixture does not send a world. Cached-status latency ends at the
complete expected status response; success additionally requires the correct
client-specific ping reply. Cache counters must confirm zero measured misses and
at least one hit per successful exchange. Cache priming is excluded and the
fixture uses a one-day TTL so expiration does not mix fills into this measurement.

The console reports success counts/rate, p95/p99 setup latency, successful setups
per second, Rift CPU, sampled peak RSS and Lua capacity rejections. JSON also
records p50, failures by category, failed-attempt p95, attempts per second,
successes per wave, client launch spread, proxy counter deltas, Lua error counts,
generated configs, binary/harness hashes, and platform/settings metadata.
Percentiles use nearest rank over **successful connections only**; an entirely
failed measurement has null latency percentiles. Read latency alongside success
rate: rejecting more connections can make the remaining successes look faster.

CPU and memory collection uses Linux `/proc`; unsupported platforms emit null
resource values. CPU is process user+system time over the measurement window,
with 100% representing one core. CPU time has the kernel's clock-tick resolution
(recorded in JSON), so very short runs can show zero or noisy utilization. RSS
is sampled every 5 ms, including the start and end; brief peaks can be missed.
JSON separates Rift, the Python backend and the Python driver. The direct path
has no Rift process. RSS reflects the entire process, including allocations
retained from earlier burst sizes and, for `rift`, the throughput test.

The original established-connection RTT and throughput measurements remain for
`direct` and `rift`: median/p95 RTT and median throughput over three runs with
one and sixteen clients. Throughput counts echoed payload once, although it
travels in both directions. `--skip-throughput` omits these tests.

These are loopback measurements including Python scheduling, socket and backend
overhead, not a Minecraft player-capacity estimate or an isolated Lua microbenchmark.
The backend has no fixed worker pool limiting burst concurrency. The driver is
single-threaded asyncio, so gate release does not mean simultaneous arrival at
Rift; inspect launch spread and driver CPU when comparing runs. Repeat on the same
hardware under comparable load before choosing a Lua architecture. CI archives
the report without timing thresholds on shared runners.

Example baseline on 2026-09-28: Intel Core Ultra 5 125U, 14 available logical
CPUs, Linux x86-64, Python 3.14.7, Rust 1.98.1 release build of `55a0298`.
The default command above measured 61,440 attempts in total. At burst size 256
(2,048 attempts per scenario), it produced:

| Scenario | Success | Setup p95 / p99 (ms) | Successful setups/s | Rift CPU | Peak Rift RSS (MiB) |
| --- | --- | --- | --- | --- | --- |
| `direct` | 100.00% | 82.59 / 83.07 | 5,028 | — | — |
| `rift` | 100.00% | 55.93 / 58.02 | 1,429 | 26.5% | 39.24 |
| `hostname` | 100.00% | 64.75 / 67.70 | 3,735 | 94.6% | 30.18 |
| `lua` | 52.78% | 70.42 / 85.90 | 2,026 | 162.9% | 17.94 |
| `lua_init` | 45.12% | 62.04 / 65.57 | 2,061 | 191.7% | 17.99 |
| `status_cached` | 100.00% | 77.88 / 87.84 | 1,140 | 12.8% | 5.79 |

All non-Lua scenarios succeeded at every measured burst size. Both Lua scenarios
succeeded at sizes 1 and 4; at size 16, success fell to 74.12% (`lua`) and
58.59% (`lua_init`). Every failed Lua attempt matched a logged capacity rejection
and a route-rejection counter increment; there were no script budget errors.
All 10,240 measured status exchanges were cache hits, with zero misses.

A second connection-only run used 8,192 attempts per size:

```sh
python3 tests/bench.py --binary target/release/rift --skip-throughput \
  --scenarios lua lua_init --attempts 8192 --burst-sizes 4,16,64,256 \
  --report target/benchmark-lua-repeat.json
```

| Scenario | Success at 4 | At 16 | At 64 | At 256 |
| --- | --- | --- | --- | --- |
| `lua` | 100.00% | 59.27% | 63.59% | 64.99% |
| `lua_init` | 100.00% | 55.58% | 42.70% | 45.21% |

Again, every failure was a Lua capacity rejection. Across the two runs, size-256
`lua` measured 47.53–70.42 ms p95, 85.90–110.49 ms p99, 163–195% CPU and
17.94–26.52 MiB sampled peak RSS. `lua_init` measured 62.04–83.52 ms p95,
65.57–102.67 ms p99, 179–192% CPU and 17.99–21.62 MiB RSS.
The default run’s p95 client launch spread at size 256 was 29.7–63.4 ms across
scenarios, so these tails include substantial driver/host scheduling delay.
The evidence supports overload at the existing four-job admission gate; it
does not establish a maximum sustainable arrival rate or a Lua-only latency.
The original VM lifecycle and concurrency limit remain unchanged.
