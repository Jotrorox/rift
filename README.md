# Rift

A Minecraft Java Edition proxy written in Rust with embedded LuaJIT. Route
hostnames through one port, authenticate players, switch backends, and manage
configuration through Lua or the bundled web dashboard. No Java or separate Lua
installation is needed to run Rift.

Rift can also supervise local Minecraft servers: start them on demand, stop
empty servers, and operate their lifecycle from the CLI or web dashboard.
Service groups can maintain minimum and spare capacity, scale from occupancy or
queue pressure, and recover failed processes with bounded retries. Define
local templates from server jars, plugins, configs and maps, then create instances
with allocated ports and live backend registration. Persistent worlds retain
files after removal; disposable game instances delete their generated files on
removal. Both retain files across stop/start. See the
[managed-server guide](docs/managed-servers.md), [managed-server example](examples/managed.lua),
[service groups](examples/services.lua) and [provisioning templates](examples/templates.lua).
The dashboard provides live server logs and commands, structured group/template
editing, configuration deployment history and rollback, scoped operator tokens,
and retained audit records. See the [HTTP operator guide](docs/http.md).
Managed Minecraft processes need their own Java runtime and explicit EULA acceptance.

## Quick start

Download a native archive from [Releases](https://github.com/Jotrorox/rift/releases),
extract it, then run:

```sh
./rift init
# Edit listener and backend addresses in rift.lua.
./rift check
./rift
```

On Windows, use `rift.exe`. The default listener is `0.0.0.0:25565` and the backend
is `127.0.0.1:25566`. `rift init` refuses to overwrite existing files. With no
arguments, Rift loads `rift.lua` from the current working directory if present
or uses those default addresses. `rift check` and `rift --check` validate that
same file by default. Use `rift --config path/to/custom.lua` to start with another
configuration file.
For a quick local run without a configuration file:

```sh
./rift 0.0.0.0:25565 127.0.0.1:25566
```

Configure a local backend in `server.properties`:

```properties
server-ip=127.0.0.1
server-port=25566
online-mode=false
prevent-proxy-connections=false
```

**The default configuration uses unauthenticated offline identities.** Keep
backend ports private and enable authentication for public Paper networks as
shown below. Offline backends should have forwarding disabled and
`enforce-secure-profile=false`.

## Authentication

Start from [examples/online.lua](examples/online.lua), which configures an
authenticated lobby/survival network. Enable both fields in your Lua script:

```lua
rift.config.authentication = { online_mode = true, timeout_ms = 10000 }
rift.config.forwarding = { mode = "velocity", secret_env = "RIFT_FORWARDING_SECRET" }
```

Set `RIFT_FORWARDING_SECRET` in Rift's environment. On each Paper backend, keep
`online-mode=false` in `server.properties` and `settings.bungeecord=false` in
`spigot.yml`, then configure `config/paper-global.yml`:

```yaml
proxies:
  velocity:
    enabled: true
    online-mode: true
    secret: "the same secret as RIFT_FORWARDING_SECRET"
```

Restart Paper after changing these settings. Rift verifies accounts with Mojang,
handles client encryption, and forwards player identities to Paper. Failed
verification never falls back to offline authentication.

## Routing and configuration

Write an ordinary Lua script using `rift.config`; no outer return table is needed:

```lua
local config = require("rift.config")
config.listeners.public = "0.0.0.0:25565"
config.backends.lobby = "127.0.0.1:25566"
config.routes.public = "lobby"

-- Import lua/config/services.lua:
-- require("config.services")
-- Load plugins/greeting/init.lua and its lua/ modules:
-- rift.plugin("greeting")
```

`rift.on(event, callback)` composes callbacks in registration order, and
`rift.command(name, definition)` registers authenticated commands. Local modules
and folder-based plugins are captured on load/reload, with paths relative to the
configuration file. Existing `return { ... }` configurations remain supported.
See the [Lua API](docs/lua.md) and [modular example](examples/modular/rift.lua).

Generate a commented starter with `rift init`, or use the
[network example](examples/network.lua) for hostname routing, fallback, health
checks, rate limits and metrics. Hostname routing also works from the CLI:

```sh
./rift 0.0.0.0:25565 \
  --route survival.example.com=127.0.0.1:25566 \
  --route '*.games.example.com=127.0.0.1:25567' \
  --default 127.0.0.1:25566
```

Routes prefer exact names, then the longest wildcard suffix, then the default.
Listeners require an IP and port; backends accept IPs or DNS names with ports.

Validate edits with `rift check rift.lua`. Reload with SIGHUP on Unix,
Ctrl-Break on Windows, or `rift admin reload` when administration is configured.
Invalid reloads retain the working configuration; existing sessions keep their
routes. Gameplay listener changes require a restart. Ctrl-C drains active
sessions for up to 30 seconds by default; a second Ctrl-C closes them immediately.

## Authenticated extensions

Lua extensions provide login and transfer decisions, lifecycle events,
commands with UUID permission checks, and FIFO server queues. API v2 adds
durable namespaced state, recurring jobs, configured HTTP integrations and
permissions that update during a session; v1 remains supported. The
[extension example](examples/extensions.lua) demonstrates these APIs with a
survival queue and staff-only server. See the [extension API contract](docs/extensions.md) for
ordering, deadlines, permissions and reload behavior. Extensions require online
authentication and Java 1.19.3–26.3 (the online authentication range).

## Compatibility

Minecraft Java **1.8.9 through 26.3**, including all intervening releases, supports
login, `/server`, `/hub`, backend switching and crash recovery. All 66 releases
have checksum-pinned official server fixtures for joining, world/chunk delivery,
chat, switching, ban rollback and recovery. Older clients use Join Game/Respawn
world resets; 1.20.2+ uses the configuration phase.
See the [protocol matrix and test scope](docs/network-protocol.md).

Authenticated acceptance has a separate [Paper test matrix](tests/MANUAL_ONLINE.md)
for signed chat and commands, resource-pack acceptance/refusal and transfer cleanup,
plus pinned LuckPerms/EssentialsX and ViaVersion/ViaBackwards combinations.
CI runs their startup and rejection preflights; full acceptance requires a signed-in
1.21.11 client and records Paper observations alongside operator confirmations.

Clients and backends must use the same protocol version. Rift does not translate
between Minecraft versions. Online authentication, Velocity modern forwarding and
authenticated extensions require **1.19.3 or newer**; earlier versions use offline
backends with forwarding disabled. Bedrock/UDP, unlisted snapshots and legacy
pre-1.7 pings are unsupported.

Set `rift.config.network = { bungeecord = true }` for existing backend plugins to
request transfers and query players/servers over the BungeeCord plugin channel.
See the [supported subchannels and configuration](docs/bungeecord.md).

## Documentation

Read the documentation at **[jotrorox.github.io/rift](https://jotrorox.github.io/rift/)**.
The pages are built from the Markdown in [docs/](docs), which every release archive
also includes for offline use.

- [Lua configuration and plugins](https://jotrorox.github.io/rift/docs/lua/): script-style settings, modules and folder plugins.
- [Authenticated extensions](https://jotrorox.github.io/rift/docs/extensions/): lifecycle hooks, commands, permissions and queues.
- [Operations](https://jotrorox.github.io/rift/docs/operations/): installation, systemd/containers, administration and upgrades.
- [Managed servers](https://jotrorox.github.io/rift/docs/managed-servers/): local processes, automatic start/stop and persistent worlds.
- [HTTP services](https://jotrorox.github.io/rift/docs/http/): web dashboard, status, API and Lua extensions.
- [Messaging](https://jotrorox.github.io/rift/docs/messaging/): plugin messaging, [Lua API](https://jotrorox.github.io/rift/docs/messaging-lua/) and [QUIC protocol](https://jotrorox.github.io/rift/docs/messaging-protocol/).
- [BungeeCord compatibility](https://jotrorox.github.io/rift/docs/bungeecord/): backend plugin transfers and player/server queries.
- [Network protocol](https://jotrorox.github.io/rift/docs/network-protocol/): backend switching and recovery.
- [Performance comparison](https://jotrorox.github.io/rift/docs/performance/): CPU, memory, latency and failures against Velocity, with reproducible workloads.

## Development

Building requires Rust (pinned in `rust-toolchain.toml`) and a C toolchain,
including MSVC on Windows. Python harnesses use the standard library.

```sh
cargo build --release --locked
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets -- --test-threads=1
python3 -m unittest discover -s tests -p 'test_*.py'
node src/web_assets/tests.js
```

To preview the documentation site, run `npm ci` and `npm run serve` in `site/`
(Node 24 or newer), then open <http://localhost:4321>.

CI also runs release tests, wire-protocol checks, real-server integration tests,
benchmarks and packaging smoke tests. See the [CI workflow](.github/workflows/ci.yml)
and [manual online-mode procedure](tests/MANUAL_ONLINE.md) for those checks.

On Linux, benchmark peers negotiate a 1460-byte TCP MSS to avoid loopback window
stalls. Benchmark and pilot JSON reports record this cap; compare results with
matching socket settings.

[BSD-2-Clause](LICENSE). Run `rift --license` for the embedded license and
[third-party notices](THIRD_PARTY_NOTICES), or `rift --help` for CLI usage.
