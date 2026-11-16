# Service groups and scaling

A service group defines a server once and lets Rift create and remove
instances of it (`lobby-1`, `lobby-2`, …) while the proxy runs. Each instance
is a [managed server](managed-servers.md) with its own directory and an
allocated port. Use groups for lobbies and games whose number changes with
demand, and add a `scaling` policy to let Rift maintain the capacity.

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

Routing to `lobby` picks one of the group's instances; routing to `lobby-1`
targets that instance. A group without a scaling policy has no destination
until you [create an instance](#create-and-remove-instances).
[examples/services.lua](../examples/services.lua) adds scaling.

## Group settings

Groups accept every
[managed-server setting](managed-servers.md#configure-a-managed-server), plus:

| Field | Default | Meaning |
| --- | --- | --- |
| `port_range` | Required | Inclusive range of ports for instances. |
| `directory` | `local/services/{name}` (persistent), `temp/services/{name}` (disposable and ephemeral) | Instance directory, relative to the configuration. Must contain `{name}` to isolate instance data. |
| `command` | Required without `template` | With a template you can omit it; Rift then runs `java -Xms{memory_mb}M -Xmx{memory_mb}M -jar server.jar nogui`. |
| `memory_mb` | `1024` | Java heap of that default command, `128` through `1048576`. Cannot be combined with an explicit `command`. |
| `template` | None | Name of the [template](templates.md) that instances are copied from. |
| `storage` | `"persistent"` | What happens to instance files; see [storage](#choose-a-storage-policy). |
| `balance` | `"least_players"` | How Rift picks an instance; see [balancing](#balance-players-across-instances). |
| `translation` | Follows `translation.clients` | Client versions Rift [translates](translation.md#which-versions-a-backend-takes) for the group, for example `"newer"`, `false` or `{ "1.20.*", ">=1.21.4" }`. |
| `scaling` | None | An [automatic scaling](#automatic-scaling) policy. |
| `nodes`, `placement` | This host | Run instances on [other hosts](nodes.md#service-groups-across-nodes). |

`{name}`, `{group}` and `{port}` expand in the directory, the command
arguments and `env` values of each instance. You can change `translation`
while instances run, here or under *Groups & templates* in the web interface.

Without a template, creation makes the working directory when needed. Prepare
its jar, EULA acceptance, plugins and backend settings before the first start.
An absolute jar path can share one jar across the directories. This mode uses
persistent storage.

## Balance players across instances

`balance` chooses the instance for a player sent to the group by a route, a
hub, a fallback, `/server lobby` or a transfer. Sending a player to `lobby-1`
by name is not balanced.

| `balance` | Picks |
| --- | --- |
| `least_players` (default) | the instance with the fewest players |
| `fill` | the instance with the most players that still has room, so lobbies fill one after another and empty ones can be [scaled down](#automatic-scaling) or stopped when idle |

For `fill`, an instance has room below the group's
`scaling.capacity_per_instance` or, without `scaling`, below the
[queue capacity](queues.md) configured for the group. With
neither there is no limit and the fullest instance always comes first.
Instances at or over the limit come last, the one with the fewest players
first.

Players count together with attachments in progress, and ties go to the first
name. The other instances remain alternatives when the chosen one is
unavailable.

## Choose a storage policy

| Storage | Stop/start | Remove | Recreate |
| --- | --- | --- | --- |
| `persistent` (default) | Retains files and world changes | Stops and unregisters; retains files | Reuses an owned template directory without reseeding its world |
| `disposable` (requires `template`) | Retains files and world changes | Stops, reaps, unregisters and deletes the generated directory, including its world | Copies a fresh instance from the template |
| `ephemeral` (requires `template`, this host only) | Deletes the generated directory once the process ends for any reason; the next start copies a fresh instance from the template | Stops, reaps, unregisters and deletes any remaining copy | Copies a fresh instance from the template |

- **Persistent** suits survival and build worlds whose changes should outlive
  an instance registration.
- **Disposable** suits matches that start from a supplied map after removal
  and recreation. An idle stop or Stop does not reset the map or delete files.
- **Ephemeral** suits minigames that reset every round. A game that ends by
  exiting with status 0 returns as a fresh copy on the next demand or scaling
  start, without removal and recreation.

Details for ephemeral instances:

- Rift deletes the copy once the child is reaped: after a clean exit, a crash,
  an idle stop, an administrator Stop, a failed start, removal and proxy
  shutdown. A crash-recovery restart also begins from a fresh copy.
- The first run uses the copy built at creation. Between runs the instance has
  no directory.
- Each run copies the template files as they are at that moment. The template
  definition (paths, software, layers) stays the one the instance was created
  with.
- A copy left by a killed Rift is deleted when Rift supervises the instance
  again. A copy that cannot be deleted is reported in the server's error and
  deleted before the next start.
- Groups with `nodes` reject ephemeral storage; use `disposable` there.

## Create and remove instances

With the `servers` [admin permission](admin.md):

```sh
rift admin groups
rift admin create lobby   # Registers lobby-1 and allocates its loopback port.
rift admin create lobby   # Registers lobby-2 on another free port.
rift admin servers
rift admin start lobby-1  # Optional: otherwise an eligible login starts it.
rift admin remove lobby-2
```

You can create instances from any of these places:

- the dashboard's **Create instance** button, or
  `POST /api/groups/{name}/instances`;
- `rift admin create <group>`;
- server plugins over the message bus with `rift.control.create`
  (see [control subjects](messaging-subjects.md#control-subjects));
- Lua plugins with `rift.servers.create(group)`
  (see [messaging from Lua](messaging-lua.md));
- [automatic scaling](#automatic-scaling).

Rift publishes each committed creation or removal on `rift.events.instances`.

**Creation**

- Rift registers the instance at once, without restarting its listeners. It
  picks a free port in `port_range` and rejects creation when none remains.
- The instance stays stopped until an eligible login or an explicit start, or
  starts right away with `autostart = true` on the group.
- Instances support the usual `start`, `stop`, `drain` and transfer commands.

**Removal**

- Rift refuses to remove an instance with players or attachment reservations.
  Drain and empty it first.
- Removal stops and reaps the child, then deregisters the backend. Persistent
  worlds stay on disk; disposable and ephemeral files are deleted.
- A failed deletion reports a cleanup error and leaves the backend
  deregistered. The port stays reserved until recovery can finish.
- Names are not reused: the name sequence survives removal and restart, so new
  capacity never takes a retired world's name.

### The HTTP API

The authenticated [HTTP API](http-api.md) exposes `GET /api/groups`,
`POST /api/groups/{name}/instances` and `DELETE /api/instances/{name}`. Send
an empty JSON object (`{}`) with either mutation. Both return HTTP `202` with
an `operation_id` and a `poll` path; poll `GET /api/operations/{id}` once per
second until `status` is `succeeded` or `failed`.

Polling requires current `servers` permission and access to the operation's
group. Results stay available for 10 minutes and are lost on a proxy restart.
Listings never expose process arguments or filesystem paths.

## The instance database

Rift keeps instance names, groups, allocated ports, directories, template and
storage ownership and explicit stop intent in its bundled SQLite database, so
registrations survive configuration reloads and proxy restarts.

By default the database is `rift.sqlite3` beside the selected configuration;
it also holds other Rift state. Set a different writable location in Lua:

```lua
instance_database = "state/rift.sqlite3",
```

- Relative paths resolve beside the configuration. Changing the path requires a
  restart.
- Keep the database and the generated instance directories on persistent
  storage.
- One Rift process owns a database at a time, enforced with an OS lock on the
  adjacent `.lock` file.
- Back up the database while Rift is stopped, or use SQLite's backup API while
  it runs.

### What a restart restores

Startup restores all registrations before autostart or scaling, and checks
them against the groups, names, port ranges, directories, storage policies
and ownership markers it finds.

- Ports are retained, even for explicitly stopped instances.
- A saved port occupied by another process or by a configured backend or
  listener stops startup, with the instance and port named. Rift keeps the
  record and its files; correct the conflict and restart.
- A port held by the instance's own
  [`keep_running`](kept-servers.md) server
  is no conflict: Rift adopts that server.
- A proxy shutdown stops disposable processes but keeps their registrations
  and directories, so it does not reset games; remove instances explicitly to
  create fresh ones. Ephemeral instances keep their registrations but not
  their copies.

A killed Rift recovers interrupted work on the next start:

- An interrupted creation is rolled back, including its unfinished copy.
- An interrupted removal is finished without restoring the registration.
  Disposable storage waiting for deletion is an owned `.rift-remove-*`
  directory; persistent worlds remain on disk.
- If an old server still occupies the saved port, recovery stops before it
  deletes storage.

## Change a group

Template and group changes apply on reload, from Lua or the dashboard's
[structured editor](http.md), as long as every registered instance keeps its
process definition, its storage policy and an allocated port inside the range.
Otherwise Rift rejects the reload and names the affected instance.

- Remove instances before you change their directories or storage policy.
- Restart Rift to apply other process-definition changes.
- When a group's template changes, existing instances keep their original
  template ownership; their worlds are not reseeded.

## Automatic scaling

Add a `scaling` table to a service group to maintain capacity:

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

| Field | Default | Meaning |
| --- | --- | --- |
| `min_instances` | `1` | Minimum registered capacity; accepts `0` through `128`. |
| `max_instances` | Port-range size, capped at `128` | Upper instance bound, including manually created instances. Must fit the group's port range and be at least the minimum and spare counts. |
| `spare_instances` | `0` | Extra instances above occupancy demand; accepts `0` through `max_instances`. |
| `capacity_per_instance` | Required | Estimated players per instance, from `1` through `100000`. |
| `target_occupancy_percent` | `80` | Desired occupancy fraction, from `1` through `100`. |
| `queue_threshold` | `1` | Queued player count that adds capacity demand, from `1` through `1024`. |
| `cooldown_ms` | `5000` | Minimum interval between automatic instance creation/removal operations, from `1` through `86400000`. |

Rift creates and starts the minimum and spare capacity once its listeners are
bound, so prepare the executable, assets, EULA acceptance and forwarding
settings before you start Rift with a scaling policy.

Policy changes apply on reload and take effect at the next scaling decision.
Adding or removing a policy changes idle shutdown for the instances of a group
with `idle_timeout_ms`, so remove those instances first.

### How the scaler decides

1. **Slots.** The occupancy target is rounded up to whole player slots per
   instance. Capacity `50` at `80` percent gives `40` slots, so `41` players
   require two instances, plus the spare count.
2. **Load.** Each instance counts the larger of its connected players and its
   attachment reservations, so active sessions count once.
3. **Queues.** Rift reads the configured extension queues for the group and its
   instances. When their combined length reaches `queue_threshold`, the queued
   players are added to the demand before rounding up.
4. **Bounds.** The desired count stays between the minimum and the maximum.
   With no load it is the larger of the minimum and spare counts.

The scaler then acts:

- It creates one instance at a time and respects the cooldown.
- It removes only empty instances without pending attachment reservations,
  following the storage policy. Occupied instances stay registered even when
  demand calls for fewer.
- Later growth allocates new instance names. Retained persistent worlds are not
  reopened automatically during the same proxy process.
- Scaled instances are not stopped by `idle_timeout_ms`, so the minimum and
  spare capacity stay running.

### Stopped and failed instances

- Manually stopped and failed instances count toward the maximum while
  registered. After a restart, SQLite restores registrations and manual stops
  before the first scaling decision.
- Scaling respects a manual Stop and never restarts an instance whose retry
  budget is exhausted.
- Failed instances do not count as capacity, so a group whose instances crash
  keeps creating replacements. Once a replacement exists, empty failed
  instances are retired first.
- A disposable group at `max_instances` also retires an empty instance that
  exhausted its retries, to make room for the replacement. A persistent group
  at its maximum keeps the instance until an operator starts or removes it.
- While a group has failed instances, each automatic change doubles the group's
  cooldown, up to five minutes (or `cooldown_ms`, if longer), so a broken
  template is not recreated in a tight loop. The backoff resets once no
  instance is failed.

## Start, stop and dev mode for a whole group

Operate a group as a unit from the dashboard's Service groups view, the HTTP
API or the admin socket:

```sh
rift admin group stop arena
rift admin group mode arena dev
rift admin group start arena
rift admin group mode arena active
```

| Action | Effect |
| --- | --- |
| **Stop** | Keeps new players out of the group's instances, pauses automatic scaling, refuses new instances and stops every empty instance now. Instances with players stop when the last one leaves; nobody is disconnected. |
| **Start** | Resumes the group: scaling runs again and every instance that is not running is started. Like a single Start, it clears explicit stops and exhausted restart retries. |
| **Dev mode** | Lets only players granted `rift.group.<group>.dev` (for example `rift.group.arena.dev`) join the group's instances, and freezes automatic scaling. |
| **Active** | Returns the group to normal. |

After a stop:

- The instances count as draining, so routing, fallbacks and transfers skip
  them. `drain <backend> off` cannot undo the drain of a stopped group's
  instance.
- Each instance is explicitly stopped, as if you had stopped it alone, so it
  does not wake on demand or after a restart.

In dev mode:

- The permission applies to routing, `/server`, queues, fallbacks and
  transfers alike.
- The scaler neither creates nor removes instances and their files while you
  work on them. Manual creation, removal, start and stop still work.
- Players already connected stay when you switch modes.

A group can be stopped and in dev mode at the same time; starting it keeps dev
mode. Both states survive reloads and, with the instance database, restarts.
They are not part of the Lua configuration and are forgotten when the group's
definition is removed.
