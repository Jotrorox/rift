# Rift

Rift is a Minecraft Java Edition proxy written in Rust with embedded LuaJIT.
It puts a network of servers behind one port: it routes hostnames,
authenticates players, moves them between backends and can start and stop the
servers itself. You configure it in Lua or in the bundled web dashboard.
Running Rift needs no Java and no separate Lua installation.

## Features

- **[Routing](docs/lua.md):** many hostnames through one port, with
  fallbacks, health checks and rate limits.
- **[Authentication](docs/authentication.md):** Mojang account checks, with
  Velocity, BungeeGuard or BungeeCord forwarding to backends.
- **[Switching](docs/network-protocol.md):** `/server`, `/hub`, crash recovery.
- **[Translation](docs/translation.md):** players and backends on different
  Minecraft versions, built in.
- **[Managed servers](docs/managed-servers.md):** Rift starts servers on
  demand and stops empty ones, with [service groups](docs/service-groups.md),
  [templates](docs/templates.md) and [multiple hosts](docs/nodes.md).
- **[Plugins](docs/plugins.md):** Lua, Rust, C or Go, with login and transfer
  decisions, chat hooks, commands, queues and timers.
- **[Permissions](docs/permissions.md):** LuckPerms-style users, groups,
  contexts, tracks and prefixes, with LuckPerms import.
- **[Web dashboard](docs/http.md):** server logs and consoles, configuration
  editing with history and rollback, operator tokens and audit records.
- **[Message bus](docs/messaging.md):** subjects, queue groups and
  request/reply for Lua, plugins and backend servers.
- **[Server list and tab list](docs/server-list.md):** network MOTD and
  favicon; tab list header, footer and network-wide players.
- **[Styled text](docs/text.md):** markup such as
  `<gradient:gold:red>Welcome</gradient>` in every text a player sees, and
  Rift's and plugins' messages in each player's language.
- **[Dialogs](docs/dialogs.md):** 1.21.6+ screens with inputs and buttons.
- **[Clustering](docs/clustering.md):** several proxies that all accept
  players, with one player list, a shared bus and replicated state, and no
  external database.
- **[Operations](docs/operations.md):** an [admin CLI](docs/admin.md),
  [metrics and logs](docs/monitoring.md), live reload, and
  [systemd and Docker setups](docs/deployment.md).
- **[Performance](docs/performance.md):** reproducible runs against Velocity.

## Quick start

Download the archive for your platform and `SHA256SUMS` from
[Releases](https://github.com/Jotrorox/rift/releases),
[verify it](docs/install.md#verifying-a-download) and extract it into an empty
directory. Linux hosts with a glibc older than 2.35 take the
[`-musl` archive](docs/install.md#which-linux-build). Then run:

```sh
./rift init
# Edit listener and backend addresses in rift.lua.
./rift check
./rift
```

On Windows, use `rift.exe`. The default listener is `0.0.0.0:25565` and the
default backend is `127.0.0.1:25566`. Configure a local backend to match in
its `server.properties`:

```properties
server-ip=127.0.0.1
server-port=25566
online-mode=false
prevent-proxy-connections=false
```

> [!WARNING]
> The default configuration uses unauthenticated offline identities. Keep
> backend ports private, and enable authentication before you open a network
> to the public.

For a quick run without a configuration file, pass both addresses:
`./rift 0.0.0.0:25565 127.0.0.1:25566`. [Install](docs/install.md) explains
each step, including macOS and Windows.

## Authentication

Enable online mode and forwarding together in `rift.lua`:

```lua
rift.config.authentication = { online_mode = true, timeout_ms = 10000 }
rift.config.forwarding = { mode = "velocity", secret_env = "RIFT_FORWARDING_SECRET" }
```

Set `RIFT_FORWARDING_SECRET` in Rift's environment and the same secret in each
Paper backend's Velocity settings. Rift then verifies accounts with Mojang and
forwards player identities to the backends.
[Authentication and forwarding](docs/authentication.md) has the Paper
settings, backends older than 1.13, offline mode and signed chat.

## Routing and configuration

`rift.lua` is an ordinary Lua script that fills in `rift.config`:

```lua
local config = require("rift.config")
config.listeners.public = "0.0.0.0:25565"
config.backends.lobby = "127.0.0.1:25566"
config.routes.public = "lobby"
```

Listeners require an IP and port; backends accept IPs or DNS names with ports.
Routes prefer exact hostnames, then the longest wildcard suffix, then the
default. [Lua configuration](docs/lua.md) covers modules, folder plugins,
callbacks and commands, and hostname routing also works
[from the command line](docs/install.md#commands).

Validate edits with `rift check rift.lua`. Reload with SIGHUP on Unix,
Ctrl-Break on Windows, or `rift admin reload`. An invalid reload keeps the
working configuration, and existing sessions keep their routes. Ctrl-C drains
sessions for up to 30 seconds by default; a second Ctrl-C closes them at once.

The [examples](examples) directory has a complete configuration for each
feature, such as [online.lua](examples/online.lua) for an authenticated
network and [nodes.lua](examples/nodes.lua) for servers on several hosts.

## Plugins

One plugin system and one contract
([`wit/rift-plugin.wit`](wit/rift-plugin.wit)), with three ways to run code:

- **script**: `rift.lua` and its folder plugins, sandboxed and editable from
  the dashboard;
- **persistent**: plugin folders with a `plugin.toml`, running trusted in
  their own LuaJIT state or as WebAssembly components in Rust, C or Go;
- **remote**: services on the message bus that answer login and transfer
  decisions.

Start with the [plugin overview](docs/plugins.md) and its trust model, then
the guides for [Lua](docs/plugins-lua.md), [Rust](docs/plugins-rust.md),
[C](docs/plugins-c.md) and [Go](docs/plugins-go.md).
[`examples/plugins`](examples/plugins) has the same plugin in all four
languages. Player hooks require online authentication.

## Compatibility

- **Versions:** Minecraft Java 1.8.9 through 26.3, all 66 releases, with
  login, `/server`, `/hub`, backend switching and crash recovery. Each release
  is tested against a checksum-pinned official server; see the
  [protocol matrix](docs/network-protocol.md).
- **Translation:** on by default between any two releases from 1.12 to 26.3,
  in both directions; `config.translation = false` turns it off.
- **Authentication:** online mode and plugin player hooks work on every
  listed version. Velocity modern forwarding needs backends on 1.13 or newer;
  BungeeCord and BungeeGuard forwarding cover all of them.
- **Paper:** signed chat, resource packs and common plugin stacks have a
  separate authenticated test matrix
  ([`tests/manual_online.py`](tests/manual_online.py)).
- **BungeeCord plugins:** `rift.config.network = { bungeecord = true }` serves
  the [BungeeCord plugin channel](docs/bungeecord.md) to backend plugins.
- **Unsupported:** Bedrock/UDP, unlisted snapshots and legacy pre-1.7 pings.
- **Managed servers** need their own Java runtime and EULA acceptance.

## Documentation

Read the documentation at **[jotrorox.github.io/rift](https://jotrorox.github.io/rift/docs/)**.
The pages are built from the Markdown in [docs/](docs), which every release
archive also includes for offline use.

| | |
| --- | --- |
| **Start** | [Install](docs/install.md) · [Authentication](docs/authentication.md) |
| **Configure** | [Lua configuration](docs/lua.md) · [Server list and tab list](docs/server-list.md) · [Styled text](docs/text.md) · [Protocol translation](docs/translation.md) · [BungeeCord compatibility](docs/bungeecord.md) |
| **Operate** | [Day-to-day operations](docs/operations.md) · [Admin CLI](docs/admin.md) · [Deployment](docs/deployment.md) · [SQLite, PostgreSQL or MariaDB](docs/database.md) · [Monitoring and logs](docs/monitoring.md) · [Hardening](docs/hardening.md) · [Upgrade and rollback](docs/upgrading.md) · [Clustering](docs/clustering.md) |
| **Servers** | [Managed servers](docs/managed-servers.md) · [Service groups and scaling](docs/service-groups.md) · [Templates and software](docs/templates.md) · [Servers that outlive the proxy](docs/kept-servers.md) · [Isolation and memory limits](docs/server-limits.md) · [Bus logins for servers](docs/bus-logins.md) · [Multiple hosts](docs/nodes.md) |
| **Web** | [Web dashboard](docs/http.md) · [Status page](docs/status-page.md) · [HTTP API](docs/http-api.md) · [Lua endpoints](docs/http-lua.md) |
| **Plugins** | [Plugins](docs/plugins.md) · [Events and commands](docs/plugin-events.md) · [Running plugins](docs/plugin-operations.md) · [Hooks in rift.lua](docs/extensions.md) · [Remote decisions](docs/remote-decisions.md) · [Server queues](docs/queues.md) · [Host services](docs/host-services.md) · [Online players API](docs/players-api.md) · [Dialogs](docs/dialogs.md) · [Lua](docs/plugins-lua.md), [Rust](docs/plugins-rust.md), [C](docs/plugins-c.md) and [Go](docs/plugins-go.md) plugins |
| **Messaging** | [Messaging](docs/messaging.md) · [Built-in subjects](docs/messaging-subjects.md) · [Messaging from Lua](docs/messaging-lua.md) · [Network API for Java](docs/network-java.md) · [Messaging protocol](docs/messaging-protocol.md) |
| **Permissions** | [Permissions](docs/permissions.md) · [Commands](docs/permissions-commands.md) · [Lua](docs/permissions-lua.md) · [HTTP API](docs/permissions-http.md) · [RiftPerms for Paper](docs/riftperms.md) · [Java API](docs/permissions-java.md) · [Pumpkin](docs/permissions-pumpkin.md) · [Rust and C clients](docs/permissions-clients.md) · [Protocol](docs/permissions-protocol.md) |
| **Reference** | [Network protocol](docs/network-protocol.md) · [How translation works](docs/translation-internals.md) · [How clustering works](docs/clustering-internals.md) · [Stability and versioning](docs/stability.md) · [Performance](docs/performance.md) |

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
(Node 24 or newer), then open <http://localhost:4321>. The build fails on a
broken link and on a page in `docs/` that `site/lib/content.mjs` does not
list; add new pages there and to the table above.

[CI](.github/workflows/ci.yml) also runs release tests, wire-protocol checks,
real-server integration tests, benchmarks and packaging smoke tests. On Linux,
benchmark peers negotiate a 1460-byte TCP MSS to avoid loopback window stalls.
Benchmark and pilot JSON reports record this cap, so compare results with
matching socket settings.

[BSD-2-Clause](LICENSE). Run `rift --license` for the embedded license and
[third-party notices](THIRD_PARTY_NOTICES), or `rift --help` for CLI usage.
