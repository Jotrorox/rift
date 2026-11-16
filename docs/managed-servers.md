# Managed servers

Rift can start Minecraft server processes when players need them and stop them
once they are empty. A *managed server* is a backend whose process Rift
supervises: one directory and one process per backend name. This page covers
named managed servers and the lifecycle all managed processes share.

| Building block | Use it for |
| --- | --- |
| Named managed server (this page) | One server in a directory you prepare, such as a lobby or a survival world. |
| [Service group](service-groups.md) | Instances that Rift creates and removes while it runs, with allocated ports and optional scaling. |
| [Template](templates.md) | Copying a jar, plugins, configs and a map into each new instance, or downloading Paper, Folia, Purpur, Fabric or vanilla. |

To confine server processes, see
[isolation and memory limits](server-limits.md); to run them on other
machines, see [multiple hosts](nodes.md). Rift does not provision remote
machines or manage containers. Unmanaged backends keep working alongside
managed ones.

## Try the example

In [examples/managed.lua](../examples/managed.lua) the lobby starts with Rift;
survival starts on demand and stops after five empty minutes. Players move
between them with `/server survival` and `/hub`.

1. Install the Java runtime your Minecraft server requires.
2. Prepare one directory per backend with the server jar, plugins and
   configuration. The example expects `examples/servers/lobby/paper.jar` and
   `examples/servers/survival/paper.jar`. Accept the Minecraft EULA where
   required. The account running Rift needs read and write access.
3. Set these properties for lobby, and use port `25567` for survival:

   ```properties
   server-ip=127.0.0.1
   server-port=25566
   online-mode=false
   prevent-proxy-connections=false
   ```

4. Configure both Paper servers for Velocity forwarding as in the
   [authentication guide](authentication.md), with Rift's
   `RIFT_FORWARDING_SECRET`. The example enables online authentication. Keep
   all backend ports private.
5. Set the two environment secrets, then validate and run:

   ```sh
   rift check examples/managed.lua
   rift --config examples/managed.lua
   ```

`rift check` starts no process and creates no files. It does not verify that
Java, a jar or a directory exists; those become a start failure when the
server is requested.

The example sets `network` to enable `/server`, `/hub` and transfers on
supported backends. A standalone managed backend also works in ordinary proxy
mode.

## Configure a managed server

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
    restart_retries = 3,
}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `directory` | Required | Persistent working directory, relative to the config file or absolute. |
| `command` | Required | Dense array of executable and arguments, executed directly. |
| `autostart` | `false` | Start once when Rift starts. |
| `start_on_connect` | `true` | Start a stopped server when an eligible player connection or transfer needs it. |
| `idle_timeout_ms` | Disabled | Stop after this interval without players or pending attachments; `0` disables it. |
| `start_timeout_ms` | `120000` | Deadline for the process to open its backend TCP port. |
| `stop_timeout_ms` | `30000` | Grace period after sending `stop` to stdin, before killing and reaping the child. |
| `restart_delay_ms` | `5000` | Delay before an automatic restart after a failed start or unexpected exit. |
| `restart_retries` | `3` | Automatic restarts after the initial start; an explicit Start resets the budget. `restart_retries = 0` disables recovery. |
| `env` | None | Extra variables, `{ NAME = "value" }`; see [environment](#environment). |
| `inherit_env` | Everything | `false`, or a list such as `{ "PATH", "HOME", "JAVA_HOME" }`: what the server inherits from Rift. |
| `messaging` | None | `{ preset = "backend", publish = {...}, subscribe = {...} }`: the server's own [bus login](#bus-logins-for-instances). |
| `isolation` | `"off"` | `"landlock"`, `"required"` or `{ mode = ..., read = {...}, write = {...} }`: confine the process to its own directory; see [isolation](server-limits.md#isolation). |
| `memory_limit` | None | Hard limit in MiB on the whole process tree, or `{ mb = ..., required = true }`; Linux only. See [memory limits](server-limits.md#memory-limits). |
| `keep_running` | `false` | Let the process [outlive the proxy](#servers-that-outlive-the-proxy); Linux and macOS, this host only. |
| `node` | This host | Run the server on a [node](nodes.md#named-servers-on-a-node). |

Unknown fields and sparse arrays are rejected.

Changing a managed definition, adding or removing a managed server, or
changing its backend address requires a Rift restart; a reload with such a
change is rejected as a whole. Ordinary route and policy changes still reload.
To add and remove servers live, use [service groups](service-groups.md).

### Command

Commands run without a shell: an argument with spaces stays one argument, and
`$VARIABLE`, `~`, redirection and shell operators are not expanded. Use a
program on `PATH` or an absolute path.

The process runs with Rift's operating-system identity, so whoever can change
managed commands can run programs with that identity.

### Environment

A server inherits Rift's environment except
[Rift's own secrets](hardening.md#secrets-in-environment-variables): the
variables named by `admin.token_env`, `messaging.principals[].token_env`,
`nodes.*.token_env`, `forwarding.secret_env` and
`extensions.integrations.*.headers_env`. A database password you export
therefore reaches your server plugins:

```lua
rift.config.service_groups.lobby = {
    -- ...
    env = { LOBBY_DATABASE_URL = "jdbc:mariadb://10.0.0.5/lobby", SERVER_NAME = "{name}" },
    inherit_env = { "PATH", "HOME", "LOBBY_DATABASE_PASSWORD" },
}
```

- `env` values are literal (integers are written as numbers) and win over
  inherited variables. Service groups expand `{name}`, `{group}` and `{port}`
  in them.
- `inherit_env` limits inheritance to the listed names, or to none with
  `false`. Keep `PATH` in the list or use an absolute executable: without
  `PATH`, the program is looked up in the system's default path only.
- Listing one of Rift's secret variables in `inherit_env` is a configuration
  error.
- `env` is stored in the configuration. If that is edited over HTTP or kept in
  version control, put real secrets in an inherited variable instead.
- Paper reads the forwarding secret from the `config/paper-global.yml` that
  provisioning writes, not from the environment.
- Changing `env` or `inherit_env` changes the process definition, like
  `command`.

### Addresses and directories

- A managed backend address is a literal loopback IP with a nonzero port, such
  as `127.0.0.1:25567` or `[::1]:25567`. DNS names work for unmanaged servers
  only.
- A managed endpoint cannot appear under a second backend name. Literal
  IPv4-mapped aliases and `localhost` aliases are rejected.
- Managed servers need distinct directories, symlink aliases included.
- In Rust, `Config::from_lua` resolves relative directories against the
  current working directory; file loading and `Config::from_lua_at` use the
  configuration directory.

### Limits

| Limit | Value |
| --- | --- |
| Managed servers | 128 |
| Timeouts | Whole milliseconds, `1` through `86400000` (24 hours); `idle_timeout_ms` also accepts `0` |
| `restart_retries` | `0` through `100` |
| Command | 128 arguments, 8192 bytes per argument, 65536 bytes in total; nonempty executable |
| Command strings and directory paths | No NUL |
| `env` and `inherit_env` | 128 entries each, 32 KiB per value |
| Variable names | Letters, digits and `_`, not starting with a digit |

## Lifecycle and readiness

The observable states are `stopped`, `starting`, `running`, `stopping` and
`failed`. Servers on an unreachable [node](nodes.md) show `unknown`.

**Starting**

- `autostart` starts the server when it is registered, but not again after an
  idle stop.
- Without `start_on_connect`, a server needs an explicit start, `autostart` or
  a [scaling policy](service-groups.md#automatic-scaling) before players can
  connect.
- Simultaneous requests share one process and wait for its startup.
- Ready means that the configured TCP port accepts a connection, not that
  plugins finished initializing or that a Minecraft login succeeds.
- An occupied backend port is a start failure. Rift does not adopt the other
  process or send it shutdown commands, unless Rift itself started it with
  [`keep_running`](#servers-that-outlive-the-proxy).

**Stopping**

- Player sessions and pending attachments prevent an idle or manual stop.
- An idle stop keeps wake on connect. A manual stop disables automatic wake
  until an explicit start.
- For service instances, SQLite keeps a manual stop across Rift restarts, also
  in groups with `autostart` or scaling. Idle stops and an ordinary proxy
  shutdown set no stop intent. Named managed servers start as configured after
  a proxy restart.
- A start or stop timeout cleans up the child. A kill reaches the child's
  whole process group, so helper processes of a wrapper script die with it.
- Programs must run in the foreground; wrappers that daemonize or fork
  detached services are unsupported.

## Recover from failures

A failed start, or an exit with a nonzero status or a signal, records an error.
Rift retries after `restart_delay_ms`, up to `restart_retries` times after the
initial attempt.

- Successful readiness does not refill the budget.
- With the budget exhausted, the server stays `failed` and player demand cannot
  restart it. Fix the cause and explicitly start the server to reset the
  budget.
- A running server that exits on its own with status 0, such as a minigame
  ending its round, becomes `stopped` without an error, restart or budget
  change. It keeps automatic wake, so the next eligible login or a scaling
  policy starts it again.
- An exit with status 0 before the server became ready is a failed start.
- On a `rift node`, the agent reports whether an exit was clean. An agent from
  before this report, a restarted agent and a process that disappeared without
  a status count as failures.

## Read the server log

Server output goes to `rift-server.log` in the server directory; read it when a
start fails.

- The log must be a regular file. Rift refuses to start a server whose log is a
  symbolic link or special file.
- At 32 MiB, checked at each start and every 30 seconds while the server runs,
  Rift copies the file to `rift-server.log.1` (replacing the previous copy) and
  empties it. One server keeps at most about 64 MiB of output.
- A few lines written during the copy can be lost. Log readers following a
  cursor see `truncated` and continue from the new file.

## What happens when Rift stops

An orderly shutdown drains proxy sessions and then stops the managed children,
except servers with [`keep_running`](#servers-that-outlive-the-proxy).

If Rift is killed with `SIGKILL` or by the OOM killer, Linux kills its managed
children too. Elsewhere they keep running and hold their ports, so the next
start fails until you stop them by hand.

Run Rift under a service manager to recover from a host failure or an
uncatchable termination. Under systemd, keep the default
`KillMode=control-group` (as in the supplied unit) so that stopping or
restarting the unit also ends leftover processes.

## Operate servers

Grant the [admin endpoint](admin.md) the `servers` permission and set
`RIFT_ADMIN_TOKEN`. Then use:

```sh
rift admin servers
rift admin start survival
rift admin stop survival
```

`start` and `stop` are asynchronous: acceptance does not mean the process has
started or exited. Poll `servers` for completion.

The listing shows the state, PID, player count, attachment reservations,
automatic-start policy and last error. `automatic_enabled`, `restart_attempts`
and `restart_exhausted` tell a maintenance stop from failed recovery.
Reservations cover active sessions and pending attachments, so they overlap the
player count. Listings never return command arguments or environment variables.

An explicit stop rejects an occupied server instead of disconnecting its
players. Drain it, transfer its players to another backend, and stop it once
the player and reservation counts reach zero; see
[maintenance and transfers](operations.md#run-a-maintenance-window).

The [web dashboard](http.md) has the same server controls and a group filter,
plus Create, Start all, Stop all and Dev mode for service groups. It says
whether removing an instance keeps its files (persistent worlds) or deletes
them (disposable games), and does not remove occupied instances.

The [HTTP API](http-api.md) has `GET /api/servers`,
`POST /api/servers/{name}/start` and `POST /api/servers/{name}/stop`, which
return HTTP `202` when accepted. It uses the web service's bearer token and
access rules; `admin.permissions` apply to the admin TCP endpoint only.

## Servers that outlive the proxy

With `keep_running = true` on a managed server or service group, its
processes keep running while Rift shuts down, restarts, is upgraded or
crashes, and the next Rift adopts them. See
[servers that outlive the proxy](kept-servers.md).

## Bus logins for instances

A backend plugin that talks to Rift's [message bus](messaging.md), such as
[RiftPerms](riftperms.md), needs a principal and a token. Rift can issue every
managed server and group instance its own login. See
[bus logins for servers](bus-logins.md).

## Test the lifecycle

`cargo test --test managed` runs real child processes for cancellation,
concurrent starts, failed startup, graceful and forced shutdown, idle policy
and, on Linux and macOS, the adoption of `keep_running` servers.

After building Rift, run the Minecraft wire scenarios without downloads:

```sh
python3 tests/managed_wire.py --binary target/debug/rift
python3 tests/services_wire.py --binary target/debug/rift
python3 tests/scaling_wire.py --binary target/debug/rift
python3 tests/provisioning_wire.py --binary target/debug/rift
python3 tests/instance_persistence_wire.py --binary target/debug/rift
python3 tests/kept_servers_wire.py --binary target/debug/rift   # Linux and macOS
```

They cover the lifecycle, service groups, scaling, template provisioning,
SQLite recovery after restarts and killed transactions, and kept servers. CI
runs the first five on Linux, macOS and Windows.

The real-server harness checks cold login, world/chunk delivery, idle shutdown
and restart with the pinned server fixtures. It needs the fixture's Java
runtime (except Pumpkin) and explicit EULA acceptance:

```sh
python3 tests/managed_minecraft.py --binary target/debug/rift --server paper --accept-eula
```

Artifacts remain under `target/minecraft/runs/managed-*/`. CI runs the real
Paper scenario, including persistent and disposable template storage, on
Linux.
