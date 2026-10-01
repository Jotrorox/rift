# Managed Minecraft servers

Rift can start local Minecraft server processes when players need them and stop
them after they become empty. It supervises one persistent directory and one
process per named backend on the same host. This provides the basic lifecycle
of a small server network; it does not provision remote machines, download server
jars, clone templates, allocate replicas or manage containers.

Start with [examples/managed.lua](../examples/managed.lua). Its lobby starts with
Rift; survival starts on demand and stops after five empty minutes. Players use
`/server survival` and `/hub` to move between them. Existing unmanaged backends
continue to work alongside managed servers.
Process management preserves your existing networking mode. Configure `network`
(as in the example) to enable `/server`, `/hub` and transfers on supported
backends; a standalone managed backend can also use ordinary proxy mode.

## Prepare the server directories

Install the Java runtime required by your Minecraft server and prepare a separate
server directory for each backend. Place the server jar, plugins and
configuration there, accept the Minecraft EULA where required, and make sure the
account running Rift can read and write the directory. Rift does not create or
delete these directories. Worlds and other server files survive process restarts.

The example expects `examples/servers/lobby/paper.jar` and
`examples/servers/survival/paper.jar`. For lobby, set these properties:

```properties
server-ip=127.0.0.1
server-port=25566
online-mode=false
prevent-proxy-connections=false
```

Use port `25567` for survival. Configure both Paper servers with the Velocity
forwarding settings in the [authentication guide](../README.md#authentication),
including the same `RIFT_FORWARDING_SECRET` used by Rift. The managed example
enables online authentication. Keep all backend ports private.

Validate and run the proxy after setting the two environment secrets:

```sh
rift check examples/managed.lua
rift --config examples/managed.lua
```

`rift check` validates configuration without starting any process or creating
server files. It does not verify that Java, a jar or a missing directory is
available. A missing or invalid executable/directory becomes a visible startup
failure when the server is requested.

## Configuration

Add `managed_servers` entries keyed by existing `backends` names:

```lua
rift.config.backends.survival = "127.0.0.1:25567"
rift.config.managed_servers.survival = {
    directory = "servers/survival",
    command = { "java", "-Xmx2G", "-jar", "paper.jar", "nogui" },
    autostart = false,
    start_on_connect = true,
    idle_timeout_ms = 300000,
    start_timeout_ms = 120000,
    stop_timeout_ms = 30000,
    restart_delay_ms = 5000,
}
```

| Field | Default | Behavior |
| --- | --- | --- |
| `directory` | Required | Persistent working directory, relative to the config file or an absolute path. |
| `command` | Required | Dense array of executable and arguments, executed directly. |
| `autostart` | `false` | Start once when Rift starts. |
| `start_on_connect` | `true` | Start a stopped server when an eligible player connection or transfer needs it. |
| `idle_timeout_ms` | Disabled | Stop after this interval without players or pending attachments; `0` disables it. |
| `start_timeout_ms` | `120000` | Deadline for the process to open its backend TCP port. |
| `stop_timeout_ms` | `30000` | Grace period after sending `stop` to stdin, before killing and reaping the child. |
| `restart_delay_ms` | `5000` | Cooldown after a failed start or unexpected process exit before another attempt. |

Commands run without a shell: arguments containing spaces remain single
arguments, and `$VARIABLE`, `~`, redirection and shell operators are not expanded.
Use a program available on `PATH` or an absolute executable path. The process
inherits Rift's environment and operating-system identity; configuration authors
who can change managed commands can run programs with that identity.

Timeouts accept whole milliseconds from `1` through `86400000` (24 hours), with
the additional disabled value `0` for `idle_timeout_ms`. There can be at most 128
managed servers, 128 arguments per command, 8192 bytes per argument and 65536
bytes across a command. The executable must be nonempty; command strings and
directory paths cannot contain NUL. Unknown fields and sparse arrays are rejected.

Managed backend addresses must be literal loopback IPs with nonzero ports, such
as `127.0.0.1:25567` or `[::1]:25567`. DNS backend names are supported for unmanaged
servers only. A managed endpoint cannot also appear under another backend name;
literal IPv4-mapped aliases and `localhost` aliases are rejected. Managed servers
must have distinct directories, including symlink aliases. In-memory Rust
`Config::from_lua` callers resolve relative directories against the current
working directory; file loading and `Config::from_lua_at` use the configuration
directory.

## Lifecycle and readiness

The observable states are `stopped`, `starting`, `running`, `stopping` and `failed`.
Simultaneous requests share the same supervised process and wait for its startup.
Readiness means that the configured TCP port accepts a connection; it does not
guarantee completion of plugin initialization or a successful Minecraft login.
An already occupied backend port is a startup failure: Rift does not adopt the
existing process or send it shutdown commands.

Output from the child is appended to `rift-server.log` in its directory. Inspect
this file alongside the lifecycle state when startup fails. Startup timeout and
shutdown timeout both clean up the supervised child. Programs should run in the
foreground; wrappers that daemonize or fork detached services are unsupported.

A crash records a failure and applies `restart_delay_ms`. There is no perpetual
restart loop: a later demand or explicit start can retry. `autostart` starts the
server once and does not repeatedly restart a server stopped by idle policy.
Idle shutdown preserves wake-on-demand behavior. Disabling `start_on_connect`
requires an explicit start or the initial `autostart` before players can connect.

Player sessions and pending attachments prevent an idle or manual shutdown. A
manual stop also disables automatic wake until an explicit start or a Rift
restart; this allows maintenance without a player immediately restarting the
server. These runtime decisions are not persisted across proxy restarts.
An orderly Rift shutdown drains proxy sessions before shutting down its managed
children. Run Rift under an operating-system service manager for recovery from
host failure or an uncatchable proxy termination.

## Operate the network

Grant the operational admin endpoint the `servers` permission and configure
`RIFT_ADMIN_TOKEN` as described in [operations](operations.md). Use:

```sh
rift admin servers
rift admin start survival
rift admin stop survival
```

`start` and `stop` accept work asynchronously. Poll `servers` for completion;
acceptance alone does not mean the process has started or exited. Listings show
the state, PID, player count, attachment reservations, automatic-start policy
and last error. Reservations cover active sessions and pending attachments, so
they overlap the player count. Listings do not return command arguments or
environment variables.

To stop an occupied server, drain it, transfer its players to another backend,
then stop it once the player and reservation counts reach zero. See the
[maintenance workflow](operations.md) for drain and transfer commands. Explicit
stop rejects an occupied server rather than disconnecting its players.

The web dashboard also exposes server state and start/stop controls. Its API has
`GET /api/servers`, `POST /api/servers/{name}/start` and
`POST /api/servers/{name}/stop`. It uses the web service's bearer token and access
rules; operational `admin.permissions` apply to the separate admin TCP endpoint.
Lifecycle mutation requests return HTTP `202` when accepted; poll the listing
to observe completion. See [HTTP services](http.md) for authentication.

Changing managed definitions, adding/removing a managed server, or changing its
backend address requires a Rift restart. Incompatible reloads reject the entire
candidate and retain the current configuration. Ordinary route and policy
changes can still reload under the existing configuration rules.

## Test the lifecycle

`cargo test --test managed` exercises real child processes for cancellation,
concurrent starts, failed startup, graceful and forced shutdown, and idle policy.
After building Rift, run the Minecraft wire scenario without downloads:

```sh
python3 tests/managed_wire.py --binary target/debug/rift
```

The real-server harness checks cold login, world/chunk delivery, idle shutdown
and restart using the pinned server fixtures. It requires the fixture's Java
runtime (except Pumpkin) and explicit EULA acceptance:

```sh
python3 tests/managed_minecraft.py --binary target/debug/rift --server paper --accept-eula
```

Real-server artifacts remain under `target/minecraft/runs/managed-*/`. CI runs
the wire scenario on Linux, macOS and Windows and the real Paper scenario on Linux.
