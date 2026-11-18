# Multiple hosts

Rift can run game servers on more than one machine and manage all of them from
the proxy: start, stop, console, logs, files, templates and scaling, the way
CloudNet nodes work. Every extra host runs `rift node`, an agent built into the
same binary. The proxy stays the **controller** and decides what runs where;
agents only prepare server directories and run processes.

```text
             players
                │
        ┌───────▼────────┐  QUIC message bus (TLS, per-node principal)
        │  rift (proxy,  │◄──────────────────────────────┐
        │  controller)   │◄─────────────┐                │
        └───────┬────────┘              │                │
                │ game traffic   ┌──────┴──────┐  ┌──────┴──────┐
                └───────────────►│ rift node   │  │ rift node   │
                                 │ node1       │  │ node2       │
                                 │ bedwars-1…  │  │ bedwars-7…  │
                                 └─────────────┘  └─────────────┘
```

> [!NOTE]
> There is exactly one active proxy. The message broker and the player
> registry both live in the proxy process, so nodes add game-server capacity,
> not proxy capacity. With the instance database in
> [PostgreSQL or MariaDB](database.md), a second controller can wait as a
> [standby](database.md#standby-controllers) and take over, adopting the
> nodes' running servers, when the active one stops.

[examples/nodes.lua](../examples/nodes.lua) is a complete controller
configuration.

## Do you need nodes?

Maybe not. If only a few servers, such as a survival world, live on other
machines, add them as plain static backends with `health_check`:

```lua
rift.config.backends.survival = "10.0.0.11:25565"
rift.config.health_check = { interval_ms = 5000, timeout_ms = 1000 }
```

They route, show up in `RiftServers` and support transfers, but get no dynamic
scaling, templates, start/stop or console from Rift. Use nodes when Rift should
start, stop and scale servers on those hosts.

## Quick start: `rift node init`

Run `rift node init` on the controller, next to `rift.lua`, for each new node:

```sh
rift node init node1 --address 10.0.0.11
```

It prints, and writes nothing:

1. the `rift.config.nodes.node1` entry and the node's token for the
   controller's environment (a new random token, or the one already set in
   this shell as `RIFT_NODE_NODE1_TOKEN`);
2. for the node: the account to create, the CA file to copy, the
   environment file and a hardened systemd unit with `KillMode=control-group`
   and `Delegate=memory`;
3. checks: whether the messaging certificate is valid for the address the
   agent dials (verified exactly as the agent will), whether the web API is
   reachable over HTTPS, and which ports to open between the hosts.

The bus address nodes dial comes from `messaging.advertise`, else
`messaging.listen`; pass `--controller host:port` when that listens on
`0.0.0.0`. See `rift node init --help` for `--web`, `--ca`, `--root`, `--user`
and `--binary`. [examples/rift-node.service](../examples/rift-node.service) is
the same unit with placeholders. The sections below explain each piece.

## Set up the controller

Nodes connect to the controller's QUIC message bus, so `messaging.listen` with
a certificate is required. Each node gets its own principal named
`node.<name>`; it needs no `messaging.principals` entry.

```lua
rift.config.messaging = {
    listen = "0.0.0.0:7443",
    certificate = "certs/messaging.pem",
    private_key = "certs/messaging-key.pem",
}
rift.config.nodes.node1 = {
    address = "10.0.0.11",  -- the proxy connects to <address>:<port>
    memory_mb = 32768,      -- optional; memory available for instances
    max_instances = 40,     -- optional; default 128
    min_free_disk_mb = 4096, -- optional; default 1024, 0 disables
    labels = { "games" },   -- optional; service groups select nodes by label
    -- token_env = "RIFT_NODE_NODE1_TOKEN", -- the default for node1
}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `address` | required | IP address or DNS name of the node, without a port |
| `memory_mb` | no limit | Memory that placement may allocate to instances |
| `max_instances` | 128 | Instances (and named servers) this node may run |
| `min_free_disk_mb` | 1024 | Placement skips the node while its agent reports less free disk below `--root`; `0` disables the check |
| `labels` | none | Names that `service_groups.*.nodes` can select |
| `token_env` | `RIFT_NODE_<NAME>_TOKEN` | Environment variable with the node's token (32–1024 bytes) |

- In the default variable name the node name is upper-cased and `-` becomes
  `_`, so `eu-2` reads `RIFT_NODE_EU_2_TOKEN`.
- The name `local` is reserved for servers on the controller itself.
- Nodes can be added, removed and changed with a reload; see
  [change nodes while Rift runs](#change-nodes-while-rift-runs).

### Change nodes while Rift runs

A [reload](operations.md#reload-the-configuration) applies changes to
`rift.config.nodes` without dropping players. Only the nodes that changed are
touched; the other agents keep their sessions.

| Change | What happens |
| --- | --- |
| Add a node | Its agent may log in from now on and receives work once it reports. Its token variable must already be in Rift's environment (see below) |
| Change `labels`, `memory_mb` or `max_instances` | Placement uses the new values for new instances. Running instances stay where they are, also when their node no longer matches their group |
| Change `token_env` | The agent's session closes; it must reconnect with the token from the new variable |
| Change `address` | Only while no server runs on the node: backend addresses of its servers are built from it |
| Remove a node | Only while no server runs on it. Its session closes and its agent can no longer log in |

The reload is refused as a whole, and nothing changes, when a token variable
is unset or too short, when a node to remove still runs servers, or when a
node's address would change under its servers. The error names the servers.

To take a node out of service:

1. Remove its labels (or its name from `service_groups.*.nodes`) and reload,
   so no new instances are placed there.
2. Remove its instances (`rift admin remove <instance>` or `DELETE
   /api/instances/<name>`), and move or delete named servers that use it.
3. Remove the node and reload.

> [!IMPORTANT]
> Rift reads node tokens from its own environment, which is fixed when the
> process starts. To add a node later, start Rift with the token variable
> already set: either the node's default `RIFT_NODE_<NAME>_TOKEN`, or spare
> variables such as `RIFT_NODE_SPARE1_TOKEN` that a new node names with
> `token_env`. A token variable set only after startup needs a restart.

### Serve templates to the nodes

Agents download template folders from the controller's web API, authenticated
with the same node token. Enable `web` on an address the nodes can reach. A
node token can only read `/api/files/templates`; every other request is
refused.

Agents talk to the web API over `https://` only, because the requests carry the
node token and the SHA-256 manifest that decides which template files a node
trusts. Set `web.tls = true` to serve HTTPS with the messaging certificate,
which agents already trust through `--ca`:

```lua
rift.config.web = { listen = "0.0.0.0:8443", token_env = "RIFT_WEB_TOKEN", tls = true }
```

Agents then use `--web https://<controller>:8443`; the certificate must name
that host, as it must for the bus. A TLS-terminating reverse proxy (Caddy,
nginx, ...) in front of a plain listener works too. The agent trusts the public
CAs plus the certificates in its `--ca` file, so the proxy may use a public
certificate or one from the same private CA as the message bus. Plain `http://`
is accepted only for loopback addresses, for an agent on the controller's host.
See [Serve HTTPS](http.md#serve-https).

### The forwarding secret reaches nodes by itself

Nodes need no copy of the forwarding secret (the Velocity secret or BungeeGuard
token). When a Paper-family instance with `velocity` or `bungeeguard`
forwarding is created on a node, the controller puts the secret into that
node's `prepare` command, encrypted with a key derived from the node's token
(see [Security](#security)). Every node therefore always writes the
controller's current secret, and a stale copy on one node cannot get players
rejected.

An agent still reads `RIFT_FORWARDING_SECRET` (or `--forwarding-secret-env`),
but only as a fallback for a controller older than the agent. If the variable
is set and differs from the controller's secret, the agent uses the
controller's and logs a warning; remove the variable from the node. Agents
that open the controller's secret advertise the `forwarding_secret` feature;
an older one is flagged as outdated (see below) and writes only its own copy.

## Run an agent

Copy the `rift` binary to the node and start it as an unprivileged user, for
example from a systemd unit:

```sh
RIFT_NODE_TOKEN=... rift node \
  --controller 10.0.0.2:7443 --name node1 --root /srv/rift \
  --ca /etc/rift/messaging-ca.pem --web https://rift.internal:8443 \
  --allow-program java
```

`rift node init` prints a complete unit for this; it keeps the systemd
defaults that matter (`KillMode=control-group`) and adds `Delegate=memory`,
which [memory limits](server-limits.md#memory-limits) need.

| Option | Meaning |
| --- | --- |
| `--controller` | The controller's `messaging.listen` address |
| `--name` | This node's name in `rift.config.nodes` |
| `--root` | All server directories and caches live below it |
| `--ca` | PEM certificate(s) that sign the controller's messaging certificate |
| `--token-env` | Variable holding this node's token (default `RIFT_NODE_TOKEN`) |
| `--server-name` | TLS name of the controller certificate (default: the controller host) |
| `--web` | The controller's web API, needed for templates; `https://` unless loopback |
| `--forwarding-secret-env` | Variable with a fallback forwarding secret, used only when the controller sends none (a controller older than the agent); default `RIFT_FORWARDING_SECRET`. Naming it explicitly makes the variable required |
| `--allow-program` | A program servers may run; repeatable (default: `java`) |

The agent reconnects with backoff when the controller is unreachable. Below
its root it keeps CloudNet's layout:

```text
/srv/rift/local/services/{name}     persistent instances
/srv/rift/temp/services/{name}      disposable instances
/srv/rift/cache/templates/          template files mirrored from the controller
/srv/rift/cache/versioncache/       downloaded server software
```

## Named servers on a node

A [managed server](managed-servers.md) with `node` runs on that node.

```lua
rift.config.backends.survival = "10.0.0.11:25570"
rift.config.managed_servers.survival = {
    node = "node1",
    directory = "survival",
    command = { "java", "-Xmx4G", "-jar", "server.jar", "nogui" },
    start_on_connect = true,
}
```

- Its backend must be the node's address with the server's port.
- Its `directory` is relative to the agent's `--root`. Prepare the directory on
  the node, or upload files through the dashboard's Files page.
- Start, stop, start on connect, idle stops, restart retries, console commands
  and logs work exactly as for local servers.
- `rift admin move` [moves it](#move-a-named-server) and its folder to
  another node.
- The proxy probes readiness, since it must reach the server anyway.

## Service groups across nodes

A [service group](service-groups.md) with `nodes` creates its instances on
matching nodes. Groups without `nodes` keep running on the controller.

```lua
rift.config.service_groups.bedwars = {
    template = "bedwars", storage = "disposable",
    port_range = { 30000, 30099 }, memory_mb = 2048,
    nodes = { labels = { "games" } },   -- or nodes = { "node1", "node2" }
    placement = "least_memory",
    scaling = { min_instances = 2, max_instances = 40, capacity_per_instance = 16 },
}
```

`nodes` accepts `labels` (a node must have all of them), `names`, both, or a
plain list of node names. A new instance goes to an online, matching node
with:

- a free instance slot;
- at least the group's `memory_mb` both unallocated (when the node sets
  `memory_mb`) and available on the host, as its agent
  [measures it](#host-resources);
- at least the node's `min_free_disk_mb` of free disk below the agent's
  `--root`;
- an agent that applies every start setting the group requires (see
  [agents older than the controller](#security)).

Among those, the group's `placement` picks:

| `placement` | Picks |
| --- | --- |
| `least_memory` (default) | the node with the most free memory: the smaller of unallocated `memory_mb` and the host's available memory |
| `fewest_instances` | the node running the fewest instances |

- Allocated memory is the sum of `memory_mb` of the group instances on a node
  (1024 MiB for groups with an explicit `command`). Named servers count toward
  `max_instances` but not allocated memory; the memory they really use shows
  up in the host's available memory.
- When no node qualifies, the scaling error names why each matching node was
  skipped, for example `node2 has 812 MiB of free disk, below its
  min_free_disk_mb of 1024`.
- An instance stays on its node until an administrator
  [moves it](#move-a-persistent-instance-to-another-node). Its node is stored
  in the instance database and survives restarts.
- `storage = "ephemeral"` (a fresh copy per run) is available only on the
  controller. Groups with `nodes` use `disposable` or `persistent`.

**Ports are per node.** Each node allocates from the group's whole port range,
so `bedwars-1` on node1 and `bedwars-7` on node2 can both use port 30000. The
backend address is the node's address plus the port, registered live like a
local instance. New instances on nodes write `server-ip=<node address>` to
`server.properties`.

> [!WARNING]
> Allow these ports from the proxy only. Paper's Velocity forwarding and
> BungeeGuard reject direct connections, but plain BungeeCord forwarding and
> other software run offline behind Rift.

### Host resources

Every state snapshot (every 2 s) carries what the agent measured on its host:

| Field | Meaning |
| --- | --- |
| `memory_total_mb`, `memory_available_mb` | Physical memory and the memory new processes can use without swapping (`MemAvailable`) |
| `cpus`, `load` | CPUs available to the agent and the one-minute load average |
| `disk_total_mb`, `disk_free_mb` | The file system holding `--root`, and the space an unprivileged process may still use on it |

- Available memory and load are measured on Linux. On macOS an agent reports
  total memory but not available memory; a field the platform cannot measure
  is `null`, and placement then relies on `memory_mb` alone.
- The available memory lags behind new servers: a JVM commits its heap
  gradually, so a freshly started instance looks smaller than it will become.
  Keep setting `memory_mb` on nodes that run many instances; placement uses
  whichever leaves less room.
- An agent older than the controller reports nothing, and placement works as
  before for its node.
- The dashboard's Nodes panel shows host memory, load (flagged `Busy` above
  one runnable process per CPU) and disk (flagged `Full` below
  `min_free_disk_mb`).

## Take a node out of service

Before maintenance on a node, cordon or drain it from the controller:

```sh
rift admin node drain node2      # no new instances; disposable games empty out
rift admin nodes                 # wait for "drained": true
# ... maintenance, agent restart, reboot ...
rift admin node uncordon node2   # back in service
```

| Command | New instances | Disposable instances | Persistent instances and named servers |
| --- | --- | --- | --- |
| `node cordon <node>` | none | keep running and take players | keep running and take players |
| `node drain <node>` | none | take no new players; each is retired (stopped and its files deleted) once it is empty | keep running and take players |
| `node uncordon <node>` | placed again | take players again | unchanged |

- Nobody is disconnected. A disposable instance with players keeps running
  until they leave, for example at the end of a round.
- Draining instances do not count as capacity, so a group with `scaling`
  creates replacements on other nodes right away. Their players still count
  toward the load.
- Draining retires disposable instances in every group, with or without
  `scaling` and also in [stopped groups](service-groups.md#start-stop-and-dev-mode-for-a-whole-group).
  Groups in dev mode are left alone: switch them back to `active` or remove
  their instances by hand.
- Persistent instances never move, because their data is on the node. Stop
  them yourself (`rift admin stop <instance>`) when maintenance needs it;
  stopping them does not remove them.
- Manual `create <group>` also skips cordoned and draining nodes. When no other
  node fits, it fails with `no node for <group> is online, not cordoned or
  draining, ...`.
- `drained` in `rift admin nodes` and `GET /api/nodes` turns `true` once a
  draining node has no disposable instances left.
- The schedule survives reloads and, with an instance database, restarts. It
  is forgotten when the node is removed from `rift.config.nodes`.

Over HTTP, `POST /api/nodes/{name}/cordon`, `/drain` and `/uncordon` (body
`{}`) do the same. They need the `servers` permission and an operator who is
not restricted to groups, since a node runs instances of every group. The
reply lists what happens to each server on the node:

```json
{"node": "node2", "schedule": "draining", "drained": false, "instances": [
  {"name": "bedwars-7", "result": "draining", "message": "3 players; retired once they leave"},
  {"name": "bedwars-9", "result": "retiring"},
  {"name": "survival", "result": "kept"}
]}
```

## Templates and software

The controller stays the source of truth for [templates](templates.md). When a
node prepares a new instance it:

1. downloads server software (Paper and others) itself, with the same version
   resolution, checksums and version cache as the controller;
2. fetches a manifest of each template folder (`GET
   /api/files/templates/<folder>?manifest`, listing each file's path, size and
   SHA-256) and downloads only files whose hash differs from its cache,
   deleting cached files that were removed from the template;
3. builds the instance like local provisioning does: config layers in order,
   `server.jar`, plugins, map, EULA, forwarding and `server.properties`.

Template folders, plugins and jars used on nodes must live in
`template_storage` (`local/templates` by default), since that is what the web
API serves. Folders
[checked out from git](templates.md#template-folders-from-git) are there
already, so only the controller needs `git` and repository access.

Templates are not sent over the message bus, because of its 1 MiB payload
limit. Persistent instances are prepared once; later starts reuse their
directory and only refresh the port and bind address.

### Warm the node caches

The first instance on a node waits for every download it needs. To fetch them
ahead of time, for example after adding a node or after
[`rift admin sync`](templates.md#sync-new-commits), use any of:

- `rift admin warm`, or `rift admin warm bedwars` for one template;
- `POST /api/nodes/warm` with `{}` or `{"template": "bedwars"}`;
- the dashboard's **Warm template caches** button in the Nodes panel.

Each node then fetches the templates of the service groups whose `nodes`
match it, including their server software, exactly as a `prepare` would.

- Nodes work in parallel, one template at a time. Only files whose SHA-256
  changed are downloaded, so warming a warm node is cheap.
- The result lists, per node and template, the files in the template, how
  many were downloaded and how many bytes.
- The command fails if any node is unreachable or any template fails, after
  the others have finished. The message names each failure; run it again once
  the node is back.
- Agents older than the controller do not know the command and report that
  they need an upgrade.

Warming does not touch existing instances: like a template edit, it only
changes what the next new instance copies.

## Move a persistent instance to another node

A persistent instance keeps its world on its node, so Rift never moves it on
its own. Move it yourself, for example to empty a node before retiring it:

```sh
rift admin move lobby-3 node2
```

The command needs the `servers` [admin permission](admin.md) and returns when
the move has finished. In the [dashboard](http.md), choose **Move…** next to
the server on the Servers page instead, pick the new node and follow the copy
in the server list; the web API has the same as
[`POST /api/servers/{name}/move`](http-api.md#managed-servers). Either way:

1. Rift checks the move: the instance must be a persistent instance of a
   service group with `nodes`, and `node2` must match the group's `nodes`, be
   online, not cordoned or draining, run an agent that can start the group's
   servers, and have a free instance slot, enough unallocated and available
   memory, and at least its `min_free_disk_mb` of free disk.
2. It stops the instance and refuses every start until the move ends. Players
   must have left first: move them with `rift admin transfer`, or
   `rift admin drain lobby-3 on` and wait.
3. It copies the instance folder from the old node to the new one through the
   proxy, over the message bus, in chunks of 512 KiB. Agents need no extra
   ports or credentials. The new node checks every file against the SHA-256
   of the bytes read on the old one, gives it the modification time it has
   there, and assembles the copy in a staging folder next to the instance
   folder.
4. It puts the copy in place, records the new node in the instance database
   and gives the instance a new backend address. The port stays the same if it
   is free on the new node; otherwise the instance gets the first free port of
   its group's range, and `server.properties` is updated.
5. It deletes the old folder, and starts the instance again if it was running.

| Option | Effect |
| --- | --- |
| `--keep-source` | Keep the old folder on the old node instead of deleting it |
| `--no-copy` | Copy nothing: the folder is already on the new node |

The reply lists what was copied (`copied`, with `reused_files` and
`reused_bytes` that an earlier attempt had already copied), the new port and
address, and `cleanup_error` if the old folder could not be deleted. Delete that folder by
hand then: it is not used again. Plugins see the move on
[`rift.events.instances`](messaging-subjects.md#events) with
`"event":"moved"`.

**If something fails**, the instance stays on its old node with its files
untouched, and starts again if it was running. Rift never replaces a folder it
did not put there itself.

**While a move runs**, the instance's service group holds still: its scaling
does not create, start, stop or remove instances. Other groups and reloads are
not held up. The copy is limited by the bus, so plan for a few minutes per
gigabyte on a slow link. `rift admin servers` and `GET /api/servers` show the
move under `move`: its phase (`stopping`, `copying` or `committing`), the
files and bytes copied so far and, when the old node's agent reports it, the
size of the whole folder. The dashboard turns that into a progress bar with
the rate and the time left.

### Interrupted and cancelled moves

A long copy does not have to finish in one go. Cancel it with

```sh
rift admin move lobby-3 --cancel
```

or **Cancel move** in the dashboard, and the instance goes back to its old
node and starts again if it was running. A move can be cancelled until its
copy is done; once it commits, it finishes.

What was copied is kept. When a copy fails (the old node went offline, the
bus dropped, the proxy restarted) or is cancelled, the files that arrived
whole stay on the new node in `.rift-move-lobby-3` next to the instance
folder. Running the same move again, to the same node, continues the copy: it
sends only the files that are missing, and those that changed on the old node
meanwhile, which it tells by their size and modification time. Files the old
node no longer has are left out of the result. A copy that was already put in
place, but whose move the proxy could not record, is taken back the same way.

Resuming needs the `move_resume` feature on both agents, new in this release.
With an older agent on the new node, a failed copy is deleted and the next
move starts over; with an older agent on the old node, every file is sent
again.

If you decide against the move instead, delete `.rift-move-lobby-3` and
`.rift-resume-lobby-3` below `local/services` on the new node, or let a later
move of the instance to that node reuse them.

### Recover the instances of a dead node

A copy needs the old node online. If it is gone for good and you use
[built-in backups](backups.md), restore each instance onto another node from
its newest snapshot:

```sh
rift admin restore lobby-3 --node node2
```

With backups of your own, restore each instance folder onto a new node, below
the agent's `--root` at the same path (`local/services/<instance>`), then:

```sh
rift admin move lobby-3 node2 --no-copy
```

Built-in backups cover persistent instances only, so recover a
[named server](#move-a-named-server) the same way: put its folder on the new
node at the same path and run `rift admin move <server> <node> --no-copy`.

Rift checks that the folder exists and records the new node; it deletes
nothing. Keep the dead node in `rift.config.nodes` until its instances have
moved, since Rift refuses a configuration that drops a node with instances. If the
old node comes back, its agent stops any server that no longer belongs to it,
but leaves the old folder for you to delete.

### Move a named server

Named servers in `managed_servers` move with the same command:

```sh
rift admin move survival node2
```

It works like an instance move, with these differences:

- No service group applies, so neither do `nodes` and memory. `node2` must
  still be online, not cordoned or draining, run an agent that can start the
  server, and have a free instance slot and its `min_free_disk_mb` of free
  disk.
- The server keeps its port, since its own files may name it. Rift refuses
  the move when another server on `node2` already uses that port.
- Nothing in the folder is rewritten, `server.properties` included.
- The old folder is only deleted when it is named after the server, like
  `directory = "survival"`. Otherwise Rift keeps it and says so in
  `cleanup_error`.
- The reply and the `moved` event have `group` and `storage` set to `null`,
  and `configured_node` names the node the configuration still has.
- Rift needs its instance database to remember the move, so it refuses to
  move named servers when it runs without a configuration file.

**Your configuration file is not edited.** Rift records the new node and
backend in the instance database and keeps using them across reloads and
restarts, for as long as the configuration names the server's old node and
backend. `rift admin servers` shows the old node as `configured_node`, and
Rift logs the move at every start. To finish the move, put the new values in
the configuration and reload:

```lua
rift.config.backends.survival = "10.0.0.12:25570"
rift.config.managed_servers.survival = {
    node = "node2",
    directory = "survival",
    command = { "java", "-Xmx4G", "-jar", "server.jar", "nogui" },
    start_on_connect = true,
}
```

Rift then forgets the move. Any other change to the server's `node` or
backend also hands the server back to the configuration; like every other
change to a named server's definition, that one needs a restart. Moving the
server back to the node the configuration names forgets the move too.

## Failure behavior

| Situation | What happens |
| --- | --- |
| The controller connection drops | The agent **keeps its servers running** and reconnects. On every (re)connection the controller sends its instance list: the agent stops servers no longer assigned to it and deletes unassigned disposable directories. |
| The controller restarts or shuts down | Remote servers keep running and are **adopted** again after the restart. An administrator stop saved before the restart still wins. Local servers stop, unless they have [`keep_running`](kept-servers.md). |
| A node is unreachable (no snapshot for 10 s) | Its servers show `unknown` and refuse new players. Disposable groups create replacement capacity on other nodes and retire empty unreachable instances. **Persistent instances stay tied to their node** and never move automatically, because their data is there; persistent groups do not shrink while one of their nodes is unreachable. [Restore or move them](#recover-the-instances-of-a-dead-node) yourself if the node does not come back. Recovery waits for the node instead of spending restart retries. |
| The node comes back | Servers that are still running return to `running`. Servers that exited meanwhile are recovered with their normal restart policy. |
| A server crashes on a node | Reported as a failure with its exit status and recovered on the same node within `restart_retries`. |
| The agent crashes or is killed | Its servers die with it, and after the agent restarts the controller restarts them. |

Each server runs in its own process group, which the agent kills when it stops
or kills the server. When the agent itself dies from `SIGKILL` or the OOM
killer:

- On Linux the kernel kills each server too, but a helper process that a start
  script left behind can survive.
- Elsewhere the server keeps running and keeps its port, so the agent refuses
  to start it again until you stop it by hand.

Run agents under systemd with `KillMode=control-group` (the default): stopping
or killing the unit then also kills every server process the agent started, so
no server outlives its agent.

## Security

The controller is trusted. Each node authenticates with its own token and
principal.

**The node principal**

- It may only publish its state and replies and subscribe to its own command
  subject. Rift refuses `rift.node.*` traffic from every other remote
  identity, so other plugins cannot command agents or read their commands.
- It has **one session at a time**. A new login closes the previous connection
  (the agent logs `replaced by a newer session of this node`), so a second
  client never receives the node's commands alongside the real agent.
- A stolen node token lets an attacker impersonate the node, read template
  files and, once the controller places a Paper-family instance on the fake
  node, learn the forwarding secret. It does not let them run commands on the
  node: only the controller can publish node commands. Keep game-server ports
  reachable from the proxy only, so the forwarding secret alone cannot be used
  to log in to them.

**The agent**

- It only runs programs from its `--allow-program` list, matched exactly
  against the first command argument.
- It only touches directories below `--root`: paths must be relative, without
  `.` or `..`, and symlinks are refused at every level.
- On Linux it clears its dumpable flag at startup, like the proxy (see
  [secrets](hardening.md#secrets-in-environment-variables)).

**Secrets**

- The node token only travels over TLS: the QUIC bus, and `--web`, which must
  be `https://` unless it is a loopback address. The agent does not follow
  redirects, so the token cannot be forwarded elsewhere.
- Two secrets cross the bus, both only to the node that needs them and with the
  same trust model: whoever holds a node's token receives them.
  - The forwarding secret, in the `prepare` command of a Paper-family instance
    with `velocity` or `bungeeguard` forwarding. It is sealed with AES-256-GCM
    under a key derived (HKDF-SHA256) from the node's token and bound to the
    node name, so command payloads never contain it in plain text and only
    that node can open it. See `src/node/seal.rs`.
  - A server with a [bus login](bus-logins.md) gets its token in every start
    command, also after a crash recovery or an agent restart. The token is
    scoped to that one instance and dies with it; the key that derives tokens
    stays on the controller.
- The start command also carries the bus certificate, which the agent writes
  to `rift-bus.pem` in the server directory.
- Servers do not inherit the agent's secrets: the variables named by
  `--token-env` and `--forwarding-secret-env` are removed from every server's
  environment.

**Server settings on a node**

- [`isolation`](server-limits.md#isolation) is applied by the agent on the
  node's kernel. `rift check` on the controller cannot tell what that kernel
  offers.
- [`memory_limit`](server-limits.md#memory-limits) is enforced by the agent in
  its own cgroup, so run the agent from a systemd unit with `Delegate=memory`.
- `env` and `inherit_env` travel with the start command and apply on the node
  as they would locally, against the agent's environment.

**Agents older than the controller**

Upgrade agents together with the controller. An agent ignores start settings
it does not know, so each agent reports the settings it applies, and the
controller checks them before it starts a server there:

| The server sets | On an agent that cannot apply it |
| --- | --- |
| `env` or `inherit_env`, or has a bus login | Refused |
| `isolation = "required"` or a `required` `memory_limit` | Refused |
| `isolation = "landlock"` or a `memory_limit` that is not `required` | Starts without it; Rift logs a warning, as it does locally on a kernel without support |

[Moves](#move-a-persistent-instance-to-another-node) need the `move` feature on
both agents (only the new one with `--no-copy`); an older agent refuses the
move before the instance is stopped. Resuming an
[interrupted move](#interrupted-and-cancelled-moves) needs `move_resume` on
both; without it a move starts over. [Backups](backups.md) and restores in
place need the `backup` feature.

A refused server shows the reason as its last error, and service groups place
new instances only on nodes whose agent can run them. Agents from 0.1.0 and
earlier report no settings at all, whatever their version says (release
candidates and nightlies report the version they lead to), so they get only
servers that need none of the above.

Rift logs a warning when an outdated agent or one with a different version
connects, the dashboard's Nodes panel flags it, `GET /api/nodes` reports
`features`, `missing_features` and `version_mismatch`, and
`rift check --strict` asks a running controller (over [`admin`](admin.md))
and reports an agent that would refuse this configuration's servers as an
error.

## Operate nodes

| Where | What you get |
| --- | --- |
| Dashboard | The Servers page shows each server's node and a Nodes panel with reachability, agent version (flagged when outdated or different from the proxy), labels, instances, allocated memory, the measured host memory, load and disk, and whether a node is cordoned or draining. |
| HTTP | `GET /api/nodes`, with each node's `schedule` and `drained`; `POST /api/nodes/{name}/cordon\|drain\|uncordon`; every server in `/api/servers` has a `node` field (`local` for the controller). Server logs, console and Files work for remote servers. Files over 512 KiB cross the message bus in parts, up to the same 64 MiB as local files; uploading those needs an agent with the `file_parts` feature. |
| Control subject and CLI | `rift.control.nodes` and `rift admin nodes` (grant `servers`). `rift admin node cordon\|drain\|uncordon <node>` [takes a node out of service](#take-a-node-out-of-service). |
| Java | `RiftControl.nodes()` in the `rift-control` artifact (see [control and events](messaging-subjects.md#java-control-and-events)). |

## Wire protocol

Agents and the controller exchange JSON over the existing bus:

| Subject | Direction | Purpose |
| --- | --- | --- |
| `rift.node.<n>.cmd` | controller → agent | Request/reply: `prepare`, `start`, `stop`, `console`, `logs`, `remove`, `files`, `reconcile`, `warm`, for moves `tree`, `read`, `receive` and `discard`, and for [backups](backups.md) `backup` and `release_backup` |
| `rift.node.<n>.state` | agent → controller | Process list and [host resources](#host-resources) every 2 s and on every change; none for 10 s makes the node `unreachable` |

Logs are read on demand through `logs` commands with the same byte cursors as
local logs, so the logs API and the dashboard work unchanged. See
`src/node/protocol.rs` for the message types.
