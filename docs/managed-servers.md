# Managed Minecraft servers

Rift can start local Minecraft server processes when players need them and stop
them after they become empty. It supervises one directory and one process per
named backend on the same host. Service groups create and remove named instances
with allocated ports while the proxy stays running. Local asset templates copy
server jars, plugins, configs and maps into new instance directories. Persistent
worlds retain their files after removal; disposable game instances delete their
generated files after removal. Rift does not provision remote machines, download
server jars or manage containers.

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
account running Rift can read and write the directory. Static managed servers
use directories you prepare; service-group creation can create its instance
directory. Static managed servers and persistent service-group instances retain
worlds and other files across process restarts and removal. Disposable
service-group instances retain files across stop/start, then delete their
generated directory on explicit removal.

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
available. Template configuration checks are structural; actual asset copying
happens during instance creation. A missing or invalid executable/directory becomes a visible startup
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
    restart_retries = 3,
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
| `restart_delay_ms` | `5000` | Delay after a failed start or unexpected process exit before an automatic restart. |
| `restart_retries` | `3` | Maximum automatic restart attempts after the initial start; an explicit Start resets the budget. `0` disables automatic recovery. |

Commands run without a shell: arguments containing spaces remain single
arguments, and `$VARIABLE`, `~`, redirection and shell operators are not expanded.
Use a program available on `PATH` or an absolute executable path. The process
inherits Rift's environment and operating-system identity; configuration authors
who can change managed commands can run programs with that identity.

Timeouts accept whole milliseconds from `1` through `86400000` (24 hours), with
the additional disabled value `0` for `idle_timeout_ms`. `restart_retries` accepts
whole counts from `0` through `100`. There can be at most 128
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

## Service groups and dynamic instances

Define lifecycle settings once under `service_groups` and route to its group:

```lua
local config = require("rift.config")
config.listeners = { public = "0.0.0.0:25565" }
config.backends = {}
config.service_groups.lobby = {
    directory = "servers/{name}",
    command = { "java", "-jar", "/absolute/paper.jar", "--port", "{port}" },
    port_range = { 25600, 25700 },
    storage = "persistent",
    start_on_connect = true,
    idle_timeout_ms = 300000,
}
config.routes.public = "lobby"
```

Groups accept the managed-server lifecycle fields above plus an inclusive
`port_range`, an optional `template` name and a `storage` policy. The directory
must contain `{name}` to isolate instance data. `{name}`, `{group}` and `{port}`
expand in the directory and command arguments for each instance.

Without an asset template, creation makes a working directory when needed;
prepare its jar, EULA acceptance, plugins and backend settings before starting.
An absolute jar path can share a jar across these directories. This existing
prepared-directory mode uses persistent storage. See
[examples/services.lua](../examples/services.lua).

| Storage | Stop/start | Remove | Recreate |
| --- | --- | --- | --- |
| `persistent` (default) | Retains files and world changes | Stops and unregisters; retains files | Reuses an owned template directory without reseeding its world |
| `disposable` (requires `template`) | Retains files and world changes | Stops, reaps, unregisters and deletes the generated directory, including its world | Copies a fresh instance from the template |

Choose persistent storage for survival/build worlds whose changes should outlive
an instance registration. Choose disposable storage for matches or rounds that
should start from a supplied map after removal and recreation. Idle shutdown or
Stop does not reset a map or delete files for either policy.

With the `servers` admin permission:

```sh
rift admin groups
rift admin create lobby   # Registers lobby-1 and allocates its loopback port.
rift admin create lobby   # Registers lobby-2 on another free port.
rift admin servers
rift admin start lobby-1  # Optional: otherwise an eligible login starts it.
rift admin remove lobby-2
```

Creation registers an instance in the proxy immediately, without
restarting its listeners. Rift chooses an available port in the configured range
and rejects creation when no port remains. By default it stays stopped until
an eligible login or explicit start; group `autostart = true` starts it after
creation. Instances support the existing
`start`, `stop`, `drain` and transfer commands. Routing to `lobby` chooses an
eligible instance by occupancy with deterministic ties and tries alternatives
when an instance is unavailable. Routing directly to `lobby-1` targets that
instance. A group without a scaling policy has no destination until an instance
is created.

Removal refuses instances with players or attachment reservations. Drain and
empty the instance first; removal stops and reaps its child before deregistering
the backend. Persistent worlds remain on disk; disposable instance files are
deleted only after the child is reaped. A failed deletion reports a cleanup error
and leaves the backend deregistered; inspect the remaining directory. Runtime
instances survive a normal configuration reload but registrations are not
persisted across a proxy restart. The data lifetime is independent: recreating
a persistent template instance with the same generated name reuses its owned
directory without overwriting world changes. Changing template definitions,
group lifecycle settings, storage or port ranges requires a restart.

The authenticated web API exposes `GET /api/groups`,
`POST /api/groups/{name}/instances` and `DELETE /api/instances/{name}`. Send an
empty JSON object (`{}`) for both mutations. Both return HTTP `202` with an
`operation_id` and `poll` path. Poll `GET /api/operations/{id}` once per second
until `status` is `succeeded` or `failed`. Success includes `result` with instance
details or the removal outcome; failure includes `error` and `http_status`.
Polling requires current `servers` permission and access to the operation's
group, even after the instance is removed. Completed results remain available
for 10 minutes and are lost on proxy restart. Tracking capacity exhaustion
returns `503` without submitting work. See [HTTP services](http.md) for limits
and response details. Group listings
include names, port ranges, instance names, `template`, `storage` and `scaling`;
server
listings also include each instance's group, address, port, template and storage.
Static servers report persistent storage and no template. Neither listing
exposes process arguments or filesystem paths.

## Automatic scaling

Add an optional `scaling` table to a service group to maintain capacity:

```lua
config.service_groups.lobby.scaling = {
    min_instances = 1,
    max_instances = 8,
    spare_instances = 1,
    capacity_per_instance = 50,
    target_occupancy_percent = 80,
    queue_threshold = 4,
    cooldown_ms = 5000,
}
```

| Field | Default | Behavior |
| --- | --- | --- |
| `min_instances` | `1` | Minimum registered capacity; accepts `0` through `128`. |
| `max_instances` | Port-range size, capped at `128` | Upper instance bound, including manually created instances. Must fit the group's port range and be at least the minimum and spare counts. |
| `spare_instances` | `0` | Extra instances above occupancy demand; accepts `0` through `max_instances`. |
| `capacity_per_instance` | Required | Estimated players per instance, from `1` through `100000`. |
| `target_occupancy_percent` | `80` | Desired occupancy fraction, from `1` through `100`. |
| `queue_threshold` | `1` | Queued player count that adds capacity demand, from `1` through `1024`. |
| `cooldown_ms` | `5000` | Minimum interval between automatic instance creation/removal operations, from `1` through `86400000`. |

Rift creates and starts the minimum and spare capacity automatically after its
listeners have bound. Prepare the executable, assets, EULA acceptance and
forwarding settings before starting Rift with a scaling policy. Groups without
`scaling` retain explicit creation and wake-on-demand behavior.

The occupancy target is rounded up to whole player slots per instance. For
example, capacity `50` at `80` percent gives `40` slots: `41` players require two
instances, plus the configured spare count. Load counts the larger of connected
players and attachment reservations per instance so active sessions are counted
once. Rift reads configured extension queues for the group and its instances.
When their combined length reaches `queue_threshold`, queued players are added
to occupancy demand before rounding up to the target slots. The desired count
is bounded by the minimum and maximum; with no load it is the larger of the
minimum and spare counts.

Scaling creates capacity one instance at a time, respecting the cooldown.
When demand drops, it removes only empty instances with no pending attachment
reservations. Occupied instances remain registered even when current demand
would call for fewer instances. Automatic removal follows the group's storage
policy: persistent files remain, while disposable directories are deleted.
Later growth allocates new instance names; retained persistent worlds are not
automatically reopened during the same proxy process.
Scaled instances use this removal policy instead of `idle_timeout_ms` shutdown,
so the minimum and spare capacity stay running.

Manually stopped and failed instances count toward the maximum while registered.
Scaling respects a manual Stop and does not force a restart after the retry
budget is exhausted. Explicitly Start a repaired instance or remove it when
appropriate; empty excess instances remain eligible for automatic removal.
Scaling policies and runtime instance registrations follow the existing reload
and restart rules: policy changes require a proxy restart, and registrations are
not persisted across proxy restarts.

## Local asset templates

Define named source assets under `rift.config.templates` and refer to the name
from one or more service groups. Source paths can be absolute, or relative to
the configuration file's directory. Assets must already exist on the host when
an instance is created. Rift reads template sources without modifying them.

```lua
local config = require("rift.config")
config.templates.arena = {
    server_jar = "assets/paper.jar",
    plugins = { "assets/plugins/game.jar" },
    configs = "assets/configs",
    map = "assets/maps/arena",
}
config.service_groups.games = {
    template = "arena",
    storage = "disposable",
    directory = "servers/{name}",
    command = { "java", "-Xmx1G", "-jar", "server.jar", "nogui" },
    port_range = { 25600, 25649 },
    start_on_connect = true,
}
```

| Template field | Required | Instance destination |
| --- | --- | --- |
| `server_jar` | Yes | `server.jar` in the working root |
| `plugins` | No | Each jar's filename under `plugins/` |
| `configs` | No | Contents of this directory overlaid onto the working root |
| `map` | No | Contents of this directory under `world/` |

Configs are copied first, followed by the jar, plugins and map. Explicit jar,
plugin and map assets replace matching files supplied by the configs directory.

Template names contain ASCII letters, digits, underscores or hyphens (up to
128 bytes). Asset paths do not expand `{name}`, `{group}` or `{port}`. The
plugin list is a dense array of at most 128 paths with distinct filenames.
Source trees contain ordinary files and directories; provisioning rejects
symlinks (including path ancestors), special files and ownership markers.
Sources and instance directories must not overlap.

Provisioning writes `server-ip` to the group's loopback address, `server-port`
to its allocated port, and `level-name=world` in the instance's
`server.properties`, preserving other settings. Supply `online-mode=false`,
forwarding configuration and any server/plugin settings required by your network
in `configs`. Minecraft server plugins copied here are separate from Rift's
Lua folder plugins.

Rift never writes `eula=true` automatically. If you accept the Minecraft EULA,
provide your own `eula.txt` in the configs directory or in each generated
instance directory before start. `autostart=false` avoids immediate starts at
creation, but `start_on_connect=true` can still start the process when an
eligible player arrives. Complete setup before routing players to it.

A new template instance is assembled before backend registration; a failed copy
leaves no registered instance or partially provisioned final directory. Generated
directories carry ownership metadata so removal and persistent reuse can identify
Rift-owned data. Template provisioning does not adopt an arbitrary preexisting
server directory. Keep source assets separate from the generated `servers/`
directories. For an existing manually prepared world, use a static managed server
or a persistent group without an asset template.

Persistent reuse preserves server files and world changes rather than copying
the seed map again. Instance registrations remain runtime only. After a proxy
restart, create instances in the same group to reuse retained names/directories;
check the listing for their newly allocated ports. Disposable recreation copies
the source assets again. Editing template assets does not update existing
instances. Templates and group policies can change live through Lua or the
[structured operator editor](http.md). Existing instances must retain their
process definitions and storage policy; remove affected instances before
changing those settings. Static managed definitions require a restart;
ordinary routes and policies can reload while instances remain registered.

Remove disposable instances before restarting Rift if you want their files
cleaned up. Proxy shutdown stops their processes and retains their directories;
it does not reset games. After a restart, creation refuses to overwrite an
existing disposable directory. Handle any leftover directory before reusing its
name, or create another instance with the next generated name.
See [examples/templates.lua](../examples/templates.lua) for persistent survival
and disposable game groups using one asset template.

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

A failed startup or unexpected exit records an error and automatically retries
after `restart_delay_ms`, up to `restart_retries` times after the initial attempt.
Successful readiness does not replenish this budget. When it is exhausted, the
server remains failed and subsequent player demand cannot restart it. Inspect
the error, fix the cause and explicitly Start the server to reset the budget.
`restart_retries = 0` leaves automatic recovery disabled.

`autostart` starts the server when registered and does not restart a server
stopped by idle policy. Idle shutdown preserves wake-on-demand behavior.
Disabling `start_on_connect` requires an explicit start, `autostart`, or a scaling
policy before players can connect.

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
and last error. Listings also expose `automatic_enabled`, `restart_attempts` and
`restart_exhausted` so operators can distinguish maintenance stops from failed
recovery. Reservations cover active sessions and pending attachments, so they
overlap the player count. Listings do not return command arguments or
environment variables.

To stop an occupied server, drain it, transfer its players to another backend,
then stop it once the player and reservation counts reach zero. See the
[maintenance workflow](operations.md) for drain and transfer commands. Explicit
stop rejects an occupied server rather than disconnecting its players.

The web dashboard also exposes service-group Create controls, server state,
start/stop controls and dynamic instance removal. It labels persistent worlds
and disposable games separately, and indicates whether removal keeps or deletes
files. Occupied and removing instances cannot be removed from the dashboard. Its API has
`GET /api/servers`, `POST /api/servers/{name}/start` and
`POST /api/servers/{name}/stop`. It uses the web service's bearer token and access
rules; operational `admin.permissions` apply to the separate admin TCP endpoint.
Lifecycle mutation requests return HTTP `202` when accepted; poll the listing
to observe completion. See [HTTP services](http.md) for authentication.

Changing managed definitions, adding/removing a managed server, or changing its
static backend address requires a Rift restart. Use service-group instance
operations above for live creation and removal. Incompatible reloads reject the entire
candidate and retain the current configuration. Ordinary route and policy
changes can still reload under the existing configuration rules.

## Test the lifecycle

`cargo test --test managed` exercises real child processes for cancellation,
concurrent starts, failed startup, graceful and forced shutdown, and idle policy.
After building Rift, run the Minecraft wire scenario without downloads:

```sh
python3 tests/managed_wire.py --binary target/debug/rift
python3 tests/services_wire.py --binary target/debug/rift
python3 tests/scaling_wire.py --binary target/debug/rift
```

The service-group scenario also checks concurrent allocation, balancing, live
transfers and crash recovery, reload preservation, occupied removal, port reuse
and cleanup. The scaling scenario checks minimum and spare startup, occupancy
growth, maximum capacity, empty-only shrink, cooldown, bounded recovery and
manual reset/stop. CI runs the existing lifecycle and service-group wire scenarios
on Linux, macOS and Windows.

The real-server harness checks cold login, world/chunk delivery, idle shutdown
and restart using the pinned server fixtures. It requires the fixture's Java
runtime (except Pumpkin) and explicit EULA acceptance:

```sh
python3 tests/managed_minecraft.py --binary target/debug/rift --server paper --accept-eula
```

Real-server artifacts remain under `target/minecraft/runs/managed-*/`. CI runs
the wire scenario on Linux, macOS and Windows and the real Paper scenario on Linux.
