# Rift

A small TCP reverse proxy for Minecraft Java Edition. Route multiple hostnames
through one port, or forward all traffic to one backend. In routing mode Rift
reads the initial handshake; the backend handles login, encryption, compression,
and gameplay. LuaJIT is embedded through `mlua`; the binary needs no Java or
separate Lua installation. Building requires a C toolchain (MSVC on Windows).

## Run

```sh
cargo build --release --locked
./target/release/rift 0.0.0.0:25565 127.0.0.1:25566
```

Players connect to port `25565`; Rift connects to the server on `25566`.
With no arguments, Rift loads `./rift.lua` if present, otherwise it uses those
same addresses. Explicit CLI addresses or routing options override the file.
The listener must be an IP literal with a port. Backends accept IP literals or DNS hostnames with ports;
IPv6 works too: `rift '[::]:25565' '[::1]:25566'`.
Use `--help` for usage. Stop with Ctrl-C (active connections close).

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
patterns or defaults are rejected. Without a default, unmatched clients close.

Routing mode requires a modern Java Edition handshake within five seconds total.
The initial packet body is limited to 2 KiB and the address to 1020 UTF-8 bytes
and 255 UTF-16 code units. Malformed, oversized, truncated or late handshakes
close without contacting a backend. Only the hostname before any NUL-delimited
mod metadata is used for matching; the entire handshake is forwarded unchanged.
Status, login and transfer handshakes all enter the same transparent relay.
Legacy pre-1.7 server-list pings cannot select a hostname route.

The positional single-backend command remains a transparent TCP relay and does
not require a Minecraft handshake. Do not mix a positional backend with routing
options.

For a backend on the same machine, set these in `server.properties`:

```properties
server-ip=127.0.0.1
server-port=25566
online-mode=true
prevent-proxy-connections=false
```

Keep Paper's BungeeCord/Velocity forwarding disabled. This is transparent TCP:
the backend sees Rift's IP address, with no player IP forwarding. There is no
protocol translation or Bedrock/UDP support.

## Lua configuration

Copy [`examples/rift.lua`](examples/rift.lua) to `rift.lua` in the working directory,
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
        max_connections = 4096,
        connect_timeout_ms = 5000,
        buffer_size = 32 * 1024,
    },
}
```

`listeners`, `backends`, and `routes` are required, nonempty tables with string
names. Listeners require IP literals with ports; backends also accept DNS
hostnames with ports. Listener port `0` asks the OS to choose an available port.
Each listener needs a route. A string value selects one backend and preserves
transparent TCP forwarding. A table enables Minecraft hostname routing:

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
milliseconds; `buffer_size` accepts 1–16,777,216 bytes per direction. All three
reject fractions, strings, and nonfinite numbers.

Lua evaluates during startup and produces a typed Rust `Config`; changing the file
requires a restart. An optional `on_route` function also runs for each connection,
as described below. Treat configuration scripts as trusted local code. Unknown
fields, invalid types or addresses, missing references, duplicate listener
addresses, and direct proxy loops fail startup with a contextual error. Syntax
and execution errors include the filename and Lua diagnostics. A missing explicit
`--config` file is an error; only a missing implicit `./rift.lua` uses the defaults.
All listeners bind before Rift begins accepting connections.

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
selection bypasses hostname matching and forwards all bytes unchanged. Returning
`nil` preserves hostname parsing, matching, handshake deadlines and forwarding;
it does not jump directly to the hostname table's `"*"` fallback. Script-selected
backends retain DNS resolution, connection deadlines and proxy-loop checks.

Return `nil` to continue the configured direct or hostname routing policy,
`{ backend = "name" }` to select a configured backend, or `{ reject = true, reason = "optional explanation" }` to close TCP.
Rejection reasons must be UTF-8 strings of at most 1,024 bytes; they are available
in the Rust result but are not sent to the client. Rift sends no Minecraft
disconnect packet. Unknown fields, conflicting choices, wrong types and unknown
backend names are errors. Arbitrary backend addresses are not accepted.

Startup syntax errors, invalid configuration, or a non-function `on_route` fail
startup. During routing, script errors, invalid decisions, exceeded budgets and
worker overload **fail closed**: only that connection closes, with an error on
stderr. Lua errors include the script filename and hook context. There is no
automatic fallback after an error; a successful `nil` result explicitly selects
the configured policy. Displayed script diagnostics are capped at 2,048 characters,
and unknown backend names at 256 characters. Backend connection failures retain the
usual connection timeout.

Each invocation creates a fresh Lua state and re-evaluates the startup source
snapshot, including its top-level code, before calling the hook. Globals and
closure upvalues are private to that invocation and never persist across clients.
The file is not read again. Keep top-level initialization small and deterministic.
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

## Implementation

- One async task per connection on Tokio's multithreaded runtime.
- `TCP_NODELAY` on both sockets for small-packet latency.
- By default, two reusable 32 KiB relay buffers per connection, with backpressure and half-close support.
- Five-second handshake deadline in routing mode; a separate backend
  DNS/connect/forward deadline (five seconds by default). No idle timeout for established sessions.
- By default, at most 4,096 active connections across all listeners, including
  pending script calls, handshakes and backend connections;
  excess clients are immediately closed. The OS file descriptor limit must allow
  two sockets per client plus headroom.
- No per-packet logging, serialization, shared traffic lock, or unbounded queue.

## Tests

```sh
cargo fmt --all --check
cargo test --locked --all-targets
cargo test --locked --all-targets --release
cargo clippy --all-targets --locked -- -D warnings
python3 -m unittest discover -s tests -p 'test_*.py' -v
python3 tests/minecraft.py --accept-eula --jobs 3
python3 tests/minecraft.py --accept-eula --jobs 3 --compression disabled
python3 tests/bench.py --report target/benchmark.json
```

Rust is pinned in `rust-toolchain.toml`; install it with rustup. The Python scripts
need Python 3.11+ and use only the standard library. Vanilla and Paper also need
Java 21; the pinned Pumpkin binary needs Linux x86_64. Server downloads require
internet access on the first run. `--accept-eula` accepts
the [Minecraft EULA](https://aka.ms/MinecraftEULA) for the local test servers.
CI passes this flag for its disposable fixtures. Servers bind only to loopback,
use offline mode, and stop with their proxies when testing finishes.

The harness pins the [official Mojang server](https://www.minecraft.net/en-us/download/server)
at **1.21.11** and [Paper](https://docs.papermc.io/misc/downloads-service/) at
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

Use `--server paper` to select one backend (repeat the option to select several),
`--binary /path/to/rift` to test an existing build, and `--report path.json` to
collect results. On macOS or Windows, select `--server vanilla --server paper`.
`--jobs 3` runs the backends concurrently; the default of one limits local resource
use. Downloads are cached separately from worlds. Do not run Python with `-O`,
which disables test assertions; the scripts reject it.

Rust tests additionally check simultaneous bulk transfer byte for byte, slow
readers, fragmented concurrent sessions, half-closes in both directions, connection
refusal, CLI startup failures, bounded handshake parsing and deadlines, and
hostname routing against two independent Minecraft status fixtures. Hook tests cover
backend selection/rejection, malformed results, execution limits and cancellation,
state isolation, and traffic continuing alongside a runaway hook.
Microsoft account authentication and
encrypted gameplay are not exercised by the offline integration fixtures.

## CI and releases

[`CI`](.github/workflows/ci.yml) runs on pull requests, pushes to `master`, merge
queues, manual dispatch, and weekly. Its independent jobs run in parallel:

| Job | Required checks |
| --- | --- |
| Quality | Rust formatting, Clippy with warnings denied, offline harness tests, actionlint |
| Dependencies | `cargo audit --deny warnings` against the current RustSec database |
| Regression | Debug and release tests on Linux x86_64, macOS ARM64, Windows x86_64; package smoke tests |
| Minecraft | Six jobs: vanilla, Paper, Pumpkin × compression enabled/disabled |
| Benchmark | Transfer correctness plus downloadable latency/throughput measurements |

All matrix jobs finish even when a sibling fails. The final **CI ready** check fails
if any required job fails, is cancelled, or is skipped. Configure branch protection
to require **CI ready** and require pull requests before merging; workflow files
cannot enable repository protection by themselves. Fork PRs need no custom secrets
or write token. Actions are pinned by commit, Cargo uses `--locked`, and Dependabot
proposes weekly Cargo and action updates.

The Minecraft jobs upload reports, proxy/server logs and crash reports even on
failure. CI caches only verified server downloads, never test worlds. Logs,
benchmark reports, and native release packages are retained for 14 days.
Benchmark numbers are informational because shared runners have variable load;
functional assertions still fail the job. Compare JSON reports from repeated runs
on the same hardware before setting a performance budget.

[`Release`](.github/workflows/release.yml) runs when a `v*` tag is pushed. The tag
must exactly match `Cargo.toml` (for example, version `0.2.0` requires `v0.2.0`).
It reruns the entire CI workflow on that tag, then publishes the tested Linux,
macOS and Windows archives with `SHA256SUMS` to a GitHub Release. Only the final
publishing job receives write permission; it uses the workflow's built-in token.
This distributes the standalone proxy; running servers are managed separately.

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

The benchmark compares direct TCP with Rift using a local Python echo server.
It reports median/p95 round-trip latency and median throughput over three runs
with one and sixteen clients. Throughput counts echoed payload once, although it
travels in both directions. These are loopback measurements including Python
overhead, not a Minecraft player-capacity estimate.

Example release-build results from the development machine (Linux, 2026-09-28):

| Route | RTT median / p95 | 1 client | 16 clients combined |
| --- | --- | --- | --- |
| Direct | 12.5 / 19.5 µs | 2,180 MiB/s | 1,957 MiB/s |
| Rift | 32.0 / 36.8 µs | 1,200 MiB/s | 1,204 MiB/s |

That run added 19.5 µs to median round-trip latency. Results vary with the host
and other running workloads.
