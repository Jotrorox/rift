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
> There is exactly one proxy. The message broker and the player registry both
> live in the proxy process, so nodes add game-server capacity, not proxy
> redundancy. Multiple proxies and proxy failover are out of scope.

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
    labels = { "games" },   -- optional; service groups select nodes by label
    -- token_env = "RIFT_NODE_NODE1_TOKEN", -- the default for node1
}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `address` | required | IP address or DNS name of the node, without a port |
| `memory_mb` | no limit | Memory that placement may allocate to instances |
| `max_instances` | 128 | Instances (and named servers) this node may run |
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
trusts. Plain `http://` is accepted only for loopback addresses, for an agent
on the controller's host.

Rift's web server speaks plain HTTP, so put a TLS-terminating reverse proxy
(Caddy, nginx, ...) in front of it for remote nodes. The agent trusts the
public CAs plus the certificates in its `--ca` file, so the proxy may use a
certificate from the same private CA as the message bus.

### Give each node the forwarding secret

Agents never receive the forwarding secret (the Velocity secret or BungeeGuard
token) from the controller. If `forwarding` uses either and nodes run
Paper-family templates, set the same value on every node, in
`RIFT_FORWARDING_SECRET` or the variable named by `--forwarding-secret-env`.
`mode = "bungeecord"` has no secret.

## Run an agent

Copy the `rift` binary to the node and start it as an unprivileged user, for
example from a systemd unit:

```sh
RIFT_NODE_TOKEN=... RIFT_FORWARDING_SECRET=... rift node \
  --controller 10.0.0.2:7443 --name node1 --root /srv/rift \
  --ca /etc/rift/messaging-ca.pem --web https://rift.internal:8443 \
  --allow-program java
```

| Option | Meaning |
| --- | --- |
| `--controller` | The controller's `messaging.listen` address |
| `--name` | This node's name in `rift.config.nodes` |
| `--root` | All server directories and caches live below it |
| `--ca` | PEM certificate(s) that sign the controller's messaging certificate |
| `--token-env` | Variable holding this node's token (default `RIFT_NODE_TOKEN`) |
| `--server-name` | TLS name of the controller certificate (default: the controller host) |
| `--web` | The controller's web API, needed for templates; `https://` unless loopback |
| `--forwarding-secret-env` | Variable holding the Velocity forwarding secret or BungeeGuard token (default `RIFT_FORWARDING_SECRET`); required only for Paper-family templates with `velocity` or `bungeeguard` forwarding |
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
plain list of node names. A new instance goes to an online, matching node with
a free instance slot and enough unallocated memory:

| `placement` | Picks |
| --- | --- |
| `least_memory` (default) | the node with the most unallocated memory |
| `fewest_instances` | the node running the fewest instances |

- Allocated memory is the sum of `memory_mb` of the group instances on a node
  (1024 MiB for groups with an explicit `command`). Named servers count toward
  `max_instances` but not memory.
- An instance stays on its node for its lifetime. Its node is stored in the
  instance database and survives restarts.
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

## Failure behavior

| Situation | What happens |
| --- | --- |
| The controller connection drops | The agent **keeps its servers running** and reconnects. On every (re)connection the controller sends its instance list: the agent stops servers no longer assigned to it and deletes unassigned disposable directories. |
| The controller restarts or shuts down | Remote servers keep running and are **adopted** again after the restart. An administrator stop saved before the restart still wins. Local servers stop, unless they have [`keep_running`](kept-servers.md). |
| A node is unreachable (no snapshot for 10 s) | Its servers show `unknown` and refuse new players. Disposable groups create replacement capacity on other nodes and retire empty unreachable instances. **Persistent instances stay tied to their node** and never move automatically, because their data is there; persistent groups do not shrink while one of their nodes is unreachable. Recovery waits for the node instead of spending restart retries. |
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
- A stolen node token lets an attacker impersonate the node and read template
  files, but not run commands on the node: only the controller can publish
  node commands.

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
- The forwarding secret never crosses the network. Prepare commands only say
  whether to enable forwarding and which mode, and each agent writes the secret
  from its own environment.
- One secret does cross the bus: a server with a
  [bus login](bus-logins.md) gets its token in
  every start command, also after a crash recovery or an agent restart. The
  token is scoped to that one instance and dies with it; the key that derives
  tokens stays on the controller.
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
| Dashboard | The Servers page shows each server's node and a Nodes panel with reachability, agent version (flagged when outdated or different from the proxy), labels, instances and allocated memory, and whether a node is cordoned or draining. |
| HTTP | `GET /api/nodes`, with each node's `schedule` and `drained`; `POST /api/nodes/{name}/cordon\|drain\|uncordon`; every server in `/api/servers` has a `node` field (`local` for the controller). Server logs, console and Files work for remote servers; remote files are limited to 512 KiB through the web console. |
| Control subject and CLI | `rift.control.nodes` and `rift admin nodes` (grant `servers`). `rift admin node cordon\|drain\|uncordon <node>` [takes a node out of service](#take-a-node-out-of-service). |
| Java | `RiftControl.nodes()` in the `rift-control` artifact (see [control and events](messaging-subjects.md#java-control-and-events)). |

## Wire protocol

Agents and the controller exchange JSON over the existing bus:

| Subject | Direction | Purpose |
| --- | --- | --- |
| `rift.node.<n>.cmd` | controller → agent | Request/reply: `prepare`, `start`, `stop`, `console`, `logs`, `remove`, `files`, `reconcile` |
| `rift.node.<n>.state` | agent → controller | Process list every 2 s and on every change; none for 10 s makes the node `unreachable` |

Logs are read on demand through `logs` commands with the same byte cursors as
local logs, so the logs API and the dashboard work unchanged. See
`src/node/protocol.rs` for the message types.
