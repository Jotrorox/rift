# Managed Minecraft servers

Rift can start local Minecraft server processes when players need them and stop
them after they become empty. It supervises one directory and one process per
named backend on the same host. Service groups create and remove named instances
with allocated ports while the proxy stays running. Local asset templates copy
server jars, plugins, configs and maps into new instance directories. Persistent
worlds retain their files after removal; disposable game instances delete their
generated files after removal, and ephemeral ones after every run. Templates can also download Paper, Folia,
Purpur, Fabric or vanilla servers automatically and lay instances out like
CloudNet v4 (see [automatic server software](#automatic-server-software-and-cloudnet-style-folders)).
Rift does not provision remote machines or manage containers.

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
generated directory on explicit removal. Ephemeral instances delete it whenever
their process ends and start every run from a fresh template copy.

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
| `env` | None | Extra environment variables, `{ NAME = "value" }`. Values are literal (integers are written as numbers); service groups expand `{name}`, `{group}` and `{port}` in them. They are set after inheritance and win over inherited variables. |
| `inherit_env` | Everything | `false` inherits nothing from Rift; a list such as `{ "PATH", "HOME", "JAVA_HOME" }` inherits only those. Rift's own secrets are never inherited either way. |
| `messaging` | None | `{ preset = "backend", publish = {...}, subscribe = {...} }`: give the server (each instance of a group) its own bus login; see [bus logins](#bus-logins-for-instances). |
| `isolation` | `"off"` | `"landlock"` or `"required"`, or `{ mode = ..., read = {...}, write = {...} }`: confine the process to its own directory; see [isolation](#isolation). |

Commands run without a shell: arguments containing spaces remain single
arguments, and `$VARIABLE`, `~`, redirection and shell operators are not expanded.
Use a program available on `PATH` or an absolute executable path. The process
runs with Rift's operating-system identity; configuration authors who can
change managed commands can run programs with that identity.

**Environment.** A server inherits Rift's environment except Rift's own
secrets: the variables named by `admin.token_env`,
`messaging.principals[].token_env`, `nodes.*.token_env`,
`forwarding.secret_env` and `extensions.integrations.*.headers_env` are
removed from every process Rift starts (see
[operations](operations.md#secrets-and-the-processes-rift-starts)). Paper
reads the forwarding secret from the `config/paper-global.yml` that
provisioning writes, not from the environment. Everything else is inherited,
because networks commonly hand database passwords to their server plugins
that way:

```lua
rift.config.service_groups.lobby = {
    -- ...
    env = { LOBBY_DATABASE_URL = "jdbc:mariadb://10.0.0.5/lobby", SERVER_NAME = "{name}" },
    inherit_env = { "PATH", "HOME", "LOBBY_DATABASE_PASSWORD" },
}
```

`inherit_env` restricts inheritance to the names it lists (`false` to none). A
restricted environment is built from scratch, so keep `PATH` in the list or
use an absolute executable: without `PATH`, the program is looked up in the
system's default path only. Listing one of Rift's secret variables is a
configuration error. Names are letters, digits and `_`, not starting with a
digit; there are at most 128 entries in each field and 32 KiB per value. `env`
values are stored in the configuration like the rest of it, so put real
secrets in an inherited variable rather than in `rift.lua` when the
configuration is edited over HTTP or kept in version control. Changing `env`
or `inherit_env` is a change of the process definition, like `command`.

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
must contain `{name}` to isolate instance data. When `directory` is omitted it
defaults to `local/services/{name}` for persistent and `temp/services/{name}`
for disposable and ephemeral storage, relative to the configuration. Groups with a `template`
may also omit `command`; Rift then runs
`java -Xms{memory_mb}M -Xmx{memory_mb}M -jar server.jar nogui` with
`memory_mb` defaulting to `1024` (accepted range `128` through `1048576`).
`memory_mb` cannot be combined with an explicit `command`. `{name}`, `{group}` and `{port}`
expand in the directory and command arguments for each instance.

`translation` names the client versions Rift
[translates](translation.md#which-versions-a-backend-takes) for the group's
instances, for example `translation = "newer"`, `translation = false` or
`translation = { "1.20.*", ">=1.21.4" }`. Without it the group follows
`translation.clients`. It can be changed while instances run, here or under
*Groups & templates* in the web interface.

Without an asset template, creation makes a working directory when needed;
prepare its jar, EULA acceptance, plugins and backend settings before starting.
An absolute jar path can share a jar across these directories. This existing
prepared-directory mode uses persistent storage. See
[examples/services.lua](../examples/services.lua).

| Storage | Stop/start | Remove | Recreate |
| --- | --- | --- | --- |
| `persistent` (default) | Retains files and world changes | Stops and unregisters; retains files | Reuses an owned template directory without reseeding its world |
| `disposable` (requires `template`) | Retains files and world changes | Stops, reaps, unregisters and deletes the generated directory, including its world | Copies a fresh instance from the template |
| `ephemeral` (requires `template`, this host only) | Deletes the generated directory once the process ends for any reason; the next start copies a fresh instance from the template | Stops, reaps, unregisters and deletes any remaining copy | Copies a fresh instance from the template |

Choose persistent storage for survival/build worlds whose changes should outlive
an instance registration. Choose disposable storage for matches or rounds that
should start from a supplied map after removal and recreation; idle shutdown or
Stop does not reset its map or delete its files. Choose ephemeral storage for
minigames that reset every round: a game that ends by exiting with status 0
returns as a fresh copy on the next demand or scaling start, without removing
and recreating the instance.

Ephemeral instances delete their copy only after the child is reaped: after a
clean exit, a crash, an idle stop, an administrator Stop, a failed start, removal
and proxy shutdown. A crash recovery restart therefore also begins from a fresh
copy. The first run uses the copy built at creation. Every copy carries the
ownership marker recorded at creation, so SQLite registrations, restarts and
removal recognize it; an instance between runs has no directory, and a copy left
by a killed Rift is deleted when the instance's supervisor starts. Each run
copies the template files as they are at that moment, while the template
definition (paths, software, layers) stays the one the instance was created
with, like its process definition. A copy that cannot be deleted is reported in
the server's error and deleted before the next start. Ephemeral storage is
rejected for groups with `nodes`; use `disposable` there.

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
the backend. Persistent worlds remain on disk; disposable and ephemeral
instance files are deleted only after the child is reaped. A failed deletion reports a cleanup error
and leaves the backend deregistered; inspect the reported cleanup error. SQLite
stores instance names, groups, allocated ports, directories, template/storage
ownership and explicit stop intent. Registrations survive configuration reloads
and proxy restarts. Template and group changes apply
on reload while every registered instance keeps its process definition, storage
policy and an allocated port inside the range; otherwise the reload is rejected
with the affected instance named. Remove instances before changing their
directories or storage policy; restart to apply other process-definition changes.

Rift bundles SQLite into its executable. By default, the database is
`rift.sqlite3` beside the selected configuration, ready to hold other Rift state
in additional tables. Set a different
writable location in Lua:

```lua
instance_database = "state/rift.sqlite3",
```

Relative paths resolve beside the configuration; changing this path requires a
restart. Keep the database and generated instance directories on persistent
storage. One Rift process owns a database at a time, enforced with an OS lock on
the adjacent `.lock` file. Back up the database while Rift is stopped, or use
SQLite's backup API while it is running.

Startup restores all registrations before process autostart or scaling. It
checks group existence, name collisions, port ranges, directory/storage policy
and ownership markers. A saved port occupied by another process or configured
backend/listener stops startup with the instance and port named. Rift retains
the record and its files; correct the conflict and restart. Ports are retained
even for explicitly stopped instances. Existing instances retain their original
template ownership when the group's template changes; their worlds are not
reseeded.

Creation records its intent before copying assets and commits registration
before launching the worker. An interrupted creation rolls back its unfinished
owned copy on startup. Removal records intent before retiring its worker, reaps
the child, then renames disposable storage to an owned `.rift-remove-*`
directory before deleting it. Startup resumes interrupted deletion without
restoring the removed registration; persistent worlds remain on disk. If an old
server still occupies the saved port, recovery stops before deleting storage.
Failed cleanup retains its journal record and port reservation until recovery
can finish. Instance name sequences survive removal and restart, so new capacity
does not reuse a retired world's name.

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
Scaling respects a manual Stop and never restarts an instance whose retry
budget is exhausted. Failed instances do not count as capacity, so a group whose
instances crash keeps creating replacements instead of sitting at zero capacity.
Once a replacement exists, empty failed instances are retired first. An empty
instance that exhausted its retries is also retired when a disposable group is
at `max_instances`, to make room for its replacement; a persistent group at its
maximum keeps the instance until an operator starts or removes it. While a
group has failed instances, each automatic change doubles the group's cooldown,
up to five minutes (or `cooldown_ms`, if longer), so a broken template is not
recreated in a tight loop. The backoff resets once no instance is failed.
Scaling policy changes apply on reload and take effect at the next scaling
decision. Adding or removing a policy changes idle shutdown for instances of a
group with `idle_timeout_ms`, so remove those instances first. SQLite restores
registrations and manual stops before the first scaling decision.

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
| `server_jar` | One of `server_jar` or `software` | `server.jar` in the working root |
| `software` | One of `server_jar` or `software` | A downloaded jar, copied to `server.jar`; see [below](#automatic-server-software-and-cloudnet-style-folders) |
| `plugins` | No | Each jar's filename under `plugins/` |
| `configs` | No | A directory, or a list of up to 16 directories, overlaid onto the working root in order |
| `map` | No | Contents of this directory under `world/` |
| `accept_eula` | No (`false`) | Writes `eula=true` to `eula.txt` |

Config layers are copied first, in order, so later folders override files from
earlier ones (like CloudNet template priorities). The jar, plugins and map follow
and replace matching files supplied by the config layers.

Template names contain ASCII letters, digits, underscores or hyphens (up to
128 bytes). Asset paths do not expand `{name}`, `{group}` or `{port}`. The
plugin list is a dense array of at most 128 paths with distinct filenames.
Source trees contain ordinary files and directories; provisioning rejects
symlinks (including path ancestors), special files and ownership markers.
Sources and instance directories must not overlap.

Provisioning writes `server-ip` to the group's loopback address, `server-port`
to its allocated port, and `level-name=world` in the instance's
`server.properties`, preserving other settings. For a local `server_jar`,
supply `online-mode=false`, forwarding configuration and any server/plugin
settings required by your network in `configs`; templates with `software` set
the proxy-related settings themselves. Minecraft server plugins copied here are separate from Rift's
Lua folder plugins.

Rift writes `eula=true` only for templates with `accept_eula = true`; setting it
records that you accept the [Minecraft EULA](https://aka.ms/MinecraftEULA).
Otherwise provide your own `eula.txt` in a config layer or in each generated
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
the seed map again. After a proxy restart, existing instances return with the
same names and ports. Explicit removal retires a name permanently; new instances
get new names and directories. Disposable recreation copies
the source assets again. Editing template assets does not update existing
instances. Templates and group policies can change live through Lua or the
[structured operator editor](http.md). Existing instances must retain their
process definitions and storage policy; remove affected instances before
changing those settings. Static managed definitions require a restart;
ordinary routes and policies can reload while instances remain registered.

Proxy shutdown stops disposable processes and retains their registrations and
directories; it does not reset games. Ephemeral instances keep their
registrations but not their copies. After a restart, Rift restores their owned
directories. Remove instances explicitly to clean them up and create fresh games.
See [examples/templates.lua](../examples/templates.lua) for persistent survival
and disposable game groups using one asset template.

## Automatic server software and CloudNet-style folders

Instead of a local `server_jar`, a template can name server software. Rift
downloads it on first use, verifies it and keeps it in a cache, similar to
CloudNet's version installer:

```lua
local config = require("rift.config")
config.templates.lobby = {
    software = { type = "paper", version = "1.21.8" }, -- or version = "latest"
    configs = { "local/templates/Global/server", "local/templates/Lobby/default" },
    accept_eula = true,
}
config.service_groups.lobby = {
    template = "lobby",
    storage = "disposable", -- Instances live in temp/services/{name}.
    port_range = { 25600, 25649 },
    memory_mb = 2048,
}
```

| `type` | Source | `version` | Verification |
| --- | --- | --- | --- |
| `paper`, `folia` | PaperMC Fill API, latest build of the version | Release such as `1.21.8`, or `latest` (default) | SHA-256 published by PaperMC |
| `purpur` | Purpur API, latest build of the version | Release or `latest` | HTTPS only, unless pinned with `sha256` |
| `vanilla` | Mojang version manifest | Release or `latest` | SHA-1 published by Mojang |
| `fabric` | Fabric meta server launcher with the latest stable loader and installer | Minecraft release or `latest` | HTTPS only, unless pinned with `sha256` |
| `url` | `url = "https://…"` with the required `sha256 = "…"` | Not used | The pinned SHA-256 |

Any exact version can also be pinned with `sha256`, for example
`{ type = "purpur", version = "1.21.8", sha256 = "…" }`. A download whose digest
differs fails and leaves no jar behind. Purpur and Fabric publish no SHA-256 for
their artifacts, so pin them where integrity matters. A pin names a single build,
so a refresh fails once upstream ships a newer one, until you update the pin.
`latest` cannot be pinned.

`latest` resolves to the newest release when the jar is first downloaded.
Downloads are cached under `software_cache` (default `local/versioncache`
beside the configuration) as `{type}-{version}/server.jar` with a
`rift-software.json` record of the resolved version, build and digest, and are
reused for every later instance. A pinned entry's name also carries the start of
its digest (`purpur-1.21.8-0123456789ab`, or a hash of URL and digest for `url`),
so changing a pin never reuses a jar checked against another digest. Every reuse
first checks the jar against its recorded SHA-256 (and the pin), and a missing
record or altered jar is downloaded again. Instance creation downloads a missing jar
before it reserves a port or name; concurrent creations share one download.
Downloads are limited to 512 MiB, must use HTTPS (plain HTTP is accepted only
for loopback mirrors), stop following redirects after five hops, and are
cancelled when Rift shuts down. A failed or interrupted download leaves no jar
behind. `rift check` and configuration loading never download anything.

Fetch or update a jar ahead of time with the dashboard's **Download**/**Update**
button on the Service groups page, `rift admin download lobby [refresh]`,
`POST /api/templates/lobby/download`, `rift.control.download`, or
`rift.servers.download("lobby")` from Lua. Refreshing replaces the cached jar
for new instances; existing instances keep the jar they were created with.
`rift admin templates` and `GET /api/templates` report the cache state. Install
the Java version the server needs (for example Java 21 or newer for current
Paper releases) on the Rift host.

For downloaded software Rift also prepares the server to run behind the proxy:
`server.properties` gets `online-mode=false`, and `white-list=false` unless a
config layer sets `white-list` (new 26.x servers otherwise turn their whitelist
on; use `network.access` to restrict players). Offline networks without
`forwarding` also get `enforce-secure-profile=false` unless a layer sets it.
With `forwarding = { mode = "velocity", ... }`, Paper, Folia and Purpur
instances get `config/paper-global.yml` with Velocity forwarding enabled and the
secret from `secret_env`, unless a config layer supplies that file. The file is
created with mode `0600`. With `mode = "bungeecord"` or `"bungeeguard"` they get
`spigot.yml` with `settings.bungeecord: true` instead; `bungeeguard` also writes
the token to `plugins/BungeeGuard/config.yml` (mode `0600`). Add the BungeeGuard
plugin jar to the template's `plugins`: without it the backend accepts the
forwarded data unchecked. A config layer's own copy of either file wins. Templates with a local `server_jar` keep the previous
behavior and receive only the endpoint settings.

The resulting layout matches CloudNet v4:

| Folder | Contents |
| --- | --- |
| `local/templates/` | Template folders used as `configs` layers (`template_storage`) |
| `local/versioncache/` | Downloaded server software (`software_cache`) |
| `local/services/{name}` | Persistent instances (default group `directory`) |
| `temp/services/{name}` | Disposable and ephemeral instances (default group `directory`) |

Both `software_cache` and `template_storage` accept a different path relative
to the configuration. The dashboard's **Files** page (and `/api/files`) browses
and edits `template_storage` and every managed server's directory; see
[HTTP services](http.md#template-and-server-files).

Servers can be spun up from any of these places:

- the dashboard's **Create instance** button, or `POST /api/groups/{name}/instances`;
- `rift admin create <group>`;
- server plugins over the message bus with `rift.control.create`
  (see [control subjects](messaging.md#rift-control-subjects));
- Lua plugins with `rift.servers.create(group)`
  (see [Lua plugins](messaging-lua.md));
- automatic scaling.

All of them use the same serialized instance transaction. Each committed creation
or removal is published on `rift.events.instances`. See
[examples/cloud.lua](../examples/cloud.lua) for a complete setup.

## Bus logins for instances

A plugin on a backend that talks to Rift's [message bus](messaging.md), such
as [RiftPerms](permissions-java.md), needs a principal and a token. Listing
`server.lobby-1`, `server.lobby-2`, … under `messaging.principals` cannot
follow scaling, and one shared login per group would let any lobby speak for
any other, including reporting contexts for players who are not on it. A
`messaging` block makes Rift issue every instance its own login instead:

```lua
rift.config.messaging = {
    listen = "0.0.0.0:7443",
    advertise = "10.0.0.2:7443", -- what instances connect to; optional for local ones
    certificate = "certs/messaging.pem",
    private_key = "certs/messaging-key.pem",
}
rift.config.service_groups.lobby = {
    template = "lobby",
    port_range = { 25600, 25649 },
    messaging = { preset = "backend" }, -- plus publish = {...}, subscribe = {...} if needed
}
```

Each instance authenticates as `server.<instance>` with the grants of the
block (`preset = "backend"` adds the [permission replica's
subjects](messaging.md#backend-permission-principals)). Rift always adds
publish access to `_INBOX.>`, so the instance can answer requests, and
subscribe access to `_INBOX.server.<instance>.>` only: REQUEST uses a private
broker inbox and needs no grant, and manual reply subjects must sit in the
instance's own namespace. An instance never receives `_INBOX.>` subscribe
access, which would expose every other identity's replies. Rift sets these
variables for its process:

| Variable | Value |
| --- | --- |
| `RIFT_SERVER_NAME` | The instance (or managed server) name, such as `lobby-3`; set for every managed server |
| `RIFT_SERVER_GROUP` | Its service group, such as `lobby` |
| `RIFT_BUS_ADDRESS` | `messaging.advertise`, else `127.0.0.1` and the listener's bound port |
| `RIFT_BUS_SERVER_NAME` | `messaging.advertise_server_name`, else the host of `advertise`, else `localhost` |
| `RIFT_BUS_CERT` | An absolute path to `rift-bus.pem` in the server directory: the messaging certificate, written at creation and refreshed on every start |
| `RIFT_BUS_TOKEN` | The instance's token |

RiftPerms, the Pumpkin adapter and the Rust client fall back to these when
`server`, the bus address, certificate or token are unset, and fill the
`group` [permission context](permissions.md#model) from `RIFT_SERVER_GROUP`,
so one template configuration serves every instance. Java services can use
`MessagingClient.builder().fromEnvironment()`.

**Tokens.** A token is `ri1.<name>.<generation>.<HMAC>`, keyed by
`local/instance-key` beside the configuration (32 random bytes, mode 0600,
created on first use; keep it out of templates and backups you share). The
generation increases each time an instance of that name is created and is
kept in the instance database; static managed servers have generation 0.
Nothing stores tokens: the bus verifies the HMAC and then checks that the
instance is still registered at that generation, so tokens survive proxy
restarts, which servers on nodes need, and a recreated instance never inherits
its predecessor's token. Instance names are not reused by Rift's own name
sequence anyway; the generation guards a reused name after a database restore
or a recreated static server. Removing an instance withdraws its login and
closes its open bus connections at once.

Grants come from the definition when an instance registers; a reload that
changes them applies to new connections. A `messaging` block needs
`messaging.listen`, and the first one added to a running Rift takes effect
after a restart, like other messaging changes. Servers on [nodes](nodes.md)
get their token in each start command (the one secret that crosses the bus;
the node never sees the key) and need `messaging.advertise` set to an address
the nodes reach; `rift check` warns otherwise. `rift check` also warns when a
group's template ships RiftPerms but the group has no `messaging` block.

Everything else a server inherits from Rift is still subject to
[`env` and `inherit_env`](#configuration); the variables above are set last,
so `env` cannot override them.

## Isolation

Managed servers run as Rift's user, so without isolation a server plugin can
read everything Rift can: `rift.lua`, `rift.sqlite3`, the messaging key,
`local/instance-key` (with which it could mint any instance's
[bus token](#bus-logins-for-instances)) and other instances' directories, and
it can read other instances' tokens from `/proc/<pid>/environ`. On Linux 5.13
and newer, `isolation` confines a server with
[Landlock](https://docs.kernel.org/userspace-api/landlock.html), which needs
no privilege:

```lua
rift.config.service_groups.lobby = {
    -- ...
    isolation = "landlock",
    -- or: isolation = { mode = "required", read = { "/srv/shared-maps" }, write = { "/srv/backups/lobby" } },
}
```

A confined server may read and write its own directory and `/tmp` (shared
by every confined server, so keep Rift's configuration and instance
directories out of it); read and
execute `/usr`, `/lib*`, `/bin`, `/sbin`, the Java installation its command
resolves to and every existing absolute path in its command line (a shared
`/absolute/paper.jar`); and read the system files name resolution, TLS, time
zones and user lookups need (never the rest of `/etc`, where a packaged Rift
keeps `rift.lua`), the random devices, its own `/proc/self` and the
system-wide `/proc` and `/sys` files the JVM sizes itself from. Everything
else is invisible, other processes' `/proc` entries included. `read` adds
paths it may read and execute, `write` paths it may also write; relative paths
are relative to the configuration, and on [nodes](nodes.md) they must be
absolute paths on the node. On Linux 6.12 (Landlock ABI 6) the server also
cannot signal processes outside its sandbox, Rift included. Networking is not
restricted: plugins listen on their own ports (votes, maps, queries) and
connect to databases.

| `isolation` | Where Landlock is available | Elsewhere (macOS, Windows, Linux before 5.13 or without Landlock) |
| --- | --- | --- |
| `"off"` (default) | unconfined | unconfined |
| `"landlock"` | confined | **starts unconfined**, as Rift's user, and logs why on every start |
| `"required"` | confined | **refuses to start**, with the reason as the start failure |

`rift check` prints, for every server and group with `isolation`, what this
host gives it (`isolation service_groups.lobby: landlock ABI 6 (filesystem,
signals)`) or the fallback and what it means. A plugin that writes outside its
directory (a world on another disk, a shared database file) needs that path in
`write`; check the server log after enabling isolation. Changing `isolation`
is a change of the process definition, like `command`. `memory_mb` sets the
heap and placement only; Rift does not yet enforce a process memory limit.

## Servers on other hosts

Managed servers and service groups can also run on other machines through
`rift node` agents, with placement by labels and capacity, per-node port ranges
and templates mirrored from this host. Servers on an unreachable node show the
state `unknown`. See [multiple hosts](nodes.md).

## Lifecycle and readiness

The observable states are `stopped`, `starting`, `running`, `stopping` and `failed`.
Simultaneous requests share the same supervised process and wait for its startup.
Readiness means that the configured TCP port accepts a connection; it does not
guarantee completion of plugin initialization or a successful Minecraft login.
An already occupied backend port is a startup failure: Rift does not adopt the
existing process or send it shutdown commands.

Output from the child is appended to `rift-server.log` in its directory. The
log must be a regular file: Rift refuses to start a server whose log is a
symbolic link or special file, because it opens the log outside the server's
isolation. Inspect
this file alongside the lifecycle state when startup fails. Once the file
reaches 32 MiB, checked at each start and every 30 seconds while the server
runs, Rift copies it to `rift-server.log.1` (replacing the previous copy) and
empties it, so one server keeps at most about 64 MiB of output. A few lines
written during the copy can be lost; log readers following a cursor see
`truncated` and continue from the new file. Startup timeout and
shutdown timeout both clean up the supervised child. Each child leads its own
process group, and a kill reaches the whole group, so helper processes started by
a wrapper script die with it. Programs should run in the foreground; wrappers
that daemonize or fork detached services are unsupported.

A failed startup or an exit with a nonzero status or signal records an error
and automatically retries after `restart_delay_ms`, up to `restart_retries`
times after the initial attempt.
Successful readiness does not replenish this budget. When it is exhausted, the
server remains failed and subsequent player demand cannot restart it. Inspect
the error, fix the cause and explicitly Start the server to reset the budget.
`restart_retries = 0` leaves automatic recovery disabled.

A running server that exits on its own with status 0, such as a minigame
that ends its round, is not a failure: it becomes `stopped` without an error,
schedules no restart and leaves the restart budget untouched. Unlike a manual
stop it keeps automatic wake, so the next eligible login or a scaling policy
starts it again. An exit with status 0 before the server became
ready still counts as a failed start. For servers on a `rift node`, agents
report whether an exit was clean; an agent from before this report, a restarted
agent and a process that disappeared without a status are treated as failures.

`autostart` starts the server when registered and does not restart a server
stopped by idle policy. Idle shutdown preserves wake-on-demand behavior.
Disabling `start_on_connect` requires an explicit start, `autostart`, or a scaling
policy before players can connect.

Player sessions and pending attachments prevent an idle or manual shutdown. A
manual stop also disables automatic wake until an explicit start. For service
instances, SQLite preserves this intent across Rift restarts, including groups
with autostart or scaling. Idle stops and ordinary proxy shutdown do not set
explicit stop intent. Static managed servers retain their configuration-driven
startup behavior across proxy restarts.
An orderly Rift shutdown drains proxy sessions before shutting down its managed
children. Run Rift under an operating-system service manager for recovery from
host failure or an uncatchable proxy termination. If Rift is killed with
`SIGKILL` or by the OOM killer, Linux kills its managed children too. On other
systems they keep running and hold their ports, so the next start fails until
they are stopped by hand. Under systemd, keep the default `KillMode=control-group`
(as in the supplied unit) so that stopping or restarting the unit also ends any
leftover process.

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

The web dashboard also exposes service-group Create, Start all, Stop all and
Dev mode controls, a group filter for the server list, server state,
start/stop controls and dynamic instance removal. It labels persistent worlds
and disposable games separately, and indicates whether removal keeps or deletes
files. Occupied and removing instances cannot be removed from the dashboard. Its API has
`GET /api/servers`, `POST /api/servers/{name}/start` and
`POST /api/servers/{name}/stop`. It uses the web service's bearer token and access
rules; operational `admin.permissions` apply to the separate admin TCP endpoint.
Lifecycle mutation requests return HTTP `202` when accepted; poll the listing
to observe completion. See [HTTP services](http.md) for authentication.

### Start, stop and dev mode for a whole group

Each service group can be operated as a unit from the dashboard's Service
groups view, the HTTP API or the admin socket:

```sh
rift admin group stop arena
rift admin group mode arena dev
rift admin group start arena
rift admin group mode arena active
```

**Stop** keeps new players out of the group's instances (they count as
draining, so routing, fallbacks and transfers skip them), pauses automatic
scaling, refuses new instances and stops every empty instance now. Instances
with players keep running until the last one leaves, then stop; nobody is
disconnected. Each stopped instance is explicitly stopped, exactly as if you had
stopped it alone, so it does not wake on demand or after a restart.
`drain <backend> off` cannot undo the drain of a stopped group's instance.

**Start** resumes the group: scaling runs again and every instance that is not
running is started. Like a single Start, it clears explicit stops and exhausted
restart retries.

**Dev mode** lets only players granted `rift.group.<group>.dev` (for example
`rift.group.arena.dev`) join the group's instances, through routing, `/server`,
queues, fallbacks and transfers alike, and freezes automatic scaling so the
scaler neither creates nor removes instances and their files while you work on
them. Manual creation, removal, start and stop still work. Players already
connected stay when you switch modes. **Active** returns the group to normal.

A group can be stopped and in dev mode at the same time; starting it keeps dev
mode. Both states survive reloads and, with the instance database, restarts.
They are not part of the Lua configuration and are forgotten when the group's
definition is removed.

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
python3 tests/provisioning_wire.py --binary target/debug/rift
```

The service-group scenario also checks concurrent allocation, balancing, live
transfers and crash recovery, reload preservation, occupied removal, port reuse
and cleanup. The scaling scenario checks minimum and spare startup, occupancy
growth, maximum capacity, empty-only shrink, cooldown, bounded recovery and
manual reset/stop. The provisioning scenario checks template copies, persistent
reuse and disposable cleanup. The SQLite persistence scenario checks normal
restarts, explicit stops, port/configuration conflicts, and killed creation and
removal transactions. CI runs all five wire scenarios on Linux, macOS and Windows.

The real-server harness checks cold login, world/chunk delivery, idle shutdown
and restart using the pinned server fixtures. It requires the fixture's Java
runtime (except Pumpkin) and explicit EULA acceptance:

```sh
python3 tests/managed_minecraft.py --binary target/debug/rift --server paper --accept-eula
```

Real-server artifacts remain under `target/minecraft/runs/managed-*/`. CI runs
the real Paper scenario, including persistent and disposable template storage,
on Linux.
