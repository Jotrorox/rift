# Rift

A Minecraft Java Edition proxy written in Rust with embedded LuaJIT. Route
hostnames through one port, authenticate players, switch backends, and manage
configuration through Lua or the bundled web dashboard. No Java or separate Lua
installation is needed to run Rift.

## Quick start

Download a native archive from [Releases](https://github.com/Jotrorox/rift/releases),
extract it, then run:

```sh
./rift init rift.lua
# Edit listener and backend addresses in rift.lua.
./rift check rift.lua
./rift --config rift.lua
```

On Windows, use `rift.exe`. The default listener is `0.0.0.0:25565` and the backend
is `127.0.0.1:25566`. `rift init` refuses to overwrite existing files. With no
arguments, Rift loads `./rift.lua` if present or uses those default addresses.
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

Lua extension API v1 provides login and transfer decisions, lifecycle events,
commands with UUID permission checks, and FIFO server queues. The
[extension example](examples/extensions.lua) demonstrates a survival queue and
staff-only server. See the [extension API contract](docs/extensions.md) for
ordering, deadlines, permissions and reload behavior. Extensions require online
authentication and a switchable client protocol (Java 1.21.8 or 1.21.11).

## Compatibility

Login supports protocol 47 (1.8), 761–775 (1.19.3–26.1), and the pinned Pumpkin
protocol 777. Online authentication requires a supported modern protocol;
protocol 47 is offline only. Backend switching and recovery support **1.21.8 and
1.21.11**. Clients and backends must use the same version. Protocol translation,
Bedrock/UDP, and legacy pre-1.7 pings are unsupported.

## Documentation

- [Lua configuration and plugins](docs/lua.md): script-style settings, modules and folder plugins.
- [Authenticated extensions](docs/extensions.md): lifecycle hooks, commands, permissions and queues.
- [Operations](docs/operations.md): installation, systemd/containers, administration and upgrades.
- [HTTP services](docs/http.md): web dashboard, status, API and Lua extensions.
- [Messaging](docs/messaging.md): plugin messaging, [Lua API](docs/messaging-lua.md) and [QUIC protocol](docs/messaging-protocol.md).
- [Network protocol](docs/network-protocol.md): backend switching and recovery.

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

CI also runs release tests, wire-protocol checks, real-server integration tests,
benchmarks and packaging smoke tests. See the [CI workflow](.github/workflows/ci.yml)
and [manual online-mode procedure](tests/MANUAL_ONLINE.md) for those checks.

On Linux, benchmark peers negotiate a 1460-byte TCP MSS to avoid loopback window
stalls. Benchmark and pilot JSON reports record this cap; compare results with
matching socket settings.

[BSD-2-Clause](LICENSE). Run `rift --license` for the embedded license and
[third-party notices](THIRD_PARTY_NOTICES), or `rift --help` for CLI usage.
