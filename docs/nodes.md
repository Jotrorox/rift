# Multiple hosts

Rift can run game servers on more than one machine and manage all of them from
the proxy: start, stop, console, logs, files, templates and scaling, the way
CloudNet nodes work. Every extra host runs `rift node`, an agent built into the
same binary. The proxy stays the **controller**: it decides what runs where.
Agents only prepare server directories and run processes.

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

**Out of scope: multiple proxies and proxy failover.** There is exactly one
proxy. The message broker and the player registry both live in the proxy
process, so running several proxies or failing over between them is a separate,
much bigger project. Nodes add game-server capacity, not proxy redundancy.

## Do you need nodes?

If only a few servers, such as a survival world, live on other machines, you may
not: add them as plain static backends with `health_check`. They route, show up
in `RiftServers` and support transfers, but get no dynamic scaling, templates,
start/stop or console from Rift.

```lua
rift.config.backends.survival = "10.0.0.11:25565"
rift.config.health_check = { interval_ms = 5000, timeout_ms = 1000 }
```

Use nodes when Rift should start, stop and scale servers on those hosts.

## Set up the controller

Nodes connect to the controller's QUIC message bus, so `messaging.listen` with a
certificate is required. Each node gets its own principal named `node.<name>`;
no `messaging.principals` entry is needed for it.

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

The node name is upper-cased and `-` becomes `_` in the default variable name,
so `eu-2` reads `RIFT_NODE_EU_2_TOKEN`. The name `local` is reserved for servers
on the controller itself. Adding, removing or changing nodes requires a restart,
like other messaging changes.

Agents download template folders from the controller's web API, authenticated
with the same node token. Enable `web` on an address the nodes can reach. A node
token can only read `/api/files/templates`; every other request is refused.

## Run an agent

Copy the `rift` binary to the node and start it as an unprivileged user, for
example from a systemd unit:

```sh
RIFT_NODE_TOKEN=... rift node \
  --controller 10.0.0.2:7443 --name node1 --root /srv/rift \
  --ca /etc/rift/messaging-ca.pem --web http://10.0.0.2:8080 \
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
| `--web` | The controller's web API, needed for templates |
| `--allow-program` | A program servers may run; repeatable (default: `java`) |

The agent reconnects with backoff when the controller is unreachable. Below its
root it keeps CloudNet's layout:

```text
/srv/rift/local/services/{name}     persistent instances
/srv/rift/temp/services/{name}      disposable instances
/srv/rift/cache/templates/          template files mirrored from the controller
/srv/rift/cache/versioncache/       downloaded server software
```

## Named servers on a node

A managed server with `node` runs on that node. Its backend must be the node's
address with the server's port, and its `directory` is relative to the agent's
`--root`. Prepare the directory on the node, or upload files through the
dashboard's Files page.

```lua
rift.config.backends.survival = "10.0.0.11:25570"
rift.config.managed_servers.survival = {
    node = "node1",
    directory = "survival",
    command = { "java", "-Xmx4G", "-jar", "server.jar", "nogui" },
    start_on_connect = true,
}
```

Start, stop, automatic start on connect, idle stops, restart retries, console
commands and logs work exactly as for local servers. Readiness is probed from
the proxy, since it must reach the server anyway.

## Service groups across nodes

A group with `nodes` creates its instances on matching nodes. Groups without
`nodes` keep running on the controller.

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
plain list of node names. A new instance goes to an online, matching node with a
free instance slot and enough unallocated memory:

| `placement` | Picks |
| --- | --- |
| `least_memory` (default) | the node with the most unallocated memory |
| `fewest_instances` | the node running the fewest instances |

Allocated memory is the sum of `memory_mb` of the group instances on a node
(1024 MiB for groups with an explicit `command`). Named servers count toward
`max_instances` but not memory. An instance stays on its node for its lifetime;
its node is stored in the instance database and survives restarts.

**Ports are per node.** Each node allocates from the group's whole port range,
so `bedwars-1` on node1 and `bedwars-7` on node2 can both use port 30000. The
backend address is the node's address plus the port, registered live like a
local instance. New instances on nodes write `server-ip=<node address>` to
`server.properties`. Allow these ports from the proxy only: Paper's Velocity
forwarding rejects direct connections, but other software runs offline behind
Rift.

## Templates and software

The controller stays the source of truth for templates. When a node prepares a
new instance it:

1. downloads server software (Paper and others) itself, with the same version
   resolution, checksums and version cache as the controller;
2. fetches a manifest of each template folder (`GET
   /api/files/templates/<folder>?manifest`, listing each file's path, size and
   SHA-256) and downloads only files whose hash differs from its cache, deleting
   cached files that were removed from the template;
3. builds the instance with the same code as local provisioning: config layers
   in order, `server.jar`, plugins, map, EULA, Velocity forwarding and
   `server.properties`, staged and then published atomically.

Template folders, plugins and jars used on nodes must live in
`template_storage` (`local/templates` by default), since that is what the web
API serves. Templates are not sent over the message bus: its 1 MiB payload
limit makes it the wrong channel for files. Persistent instances are prepared
once; later starts reuse their directory and only refresh the port and bind
address.

## Failure behaviour

| Situation | What happens |
| --- | --- |
| The controller connection drops | The agent **keeps its servers running** and reconnects. On every (re)connection the controller sends its instance list: the agent stops servers no longer assigned to it and deletes unassigned disposable directories. |
| The controller restarts or shuts down | Remote servers keep running and are **adopted** again after the restart. An administrator stop that was saved before the restart still wins. Local servers stop, as before. |
| A node is unreachable (no snapshot for 10 s) | Its servers show `unknown` and refuse new players. Disposable groups create replacement capacity on other nodes and retire empty unreachable instances. **Persistent instances stay tied to their node** and never move automatically, because their data is there; persistent groups do not shrink while one of their nodes is unreachable. Recovery waits for the node instead of spending restart retries. |
| The node comes back | Servers that are still running return to `running`. Servers that exited meanwhile are recovered with their normal restart policy. |
| A server crashes on a node | Reported as a failure with its exit status and recovered on the same node within `restart_retries`. |
| The agent crashes or is killed | Its servers die with it (child processes are killed with the agent). After the agent restarts, the controller restarts them. A later version could run servers under systemd scopes to survive agent restarts. |

## Security

- Each node authenticates with its own token and principal. The principal may
  only publish its state and replies and subscribe to its own command subject.
  Rift refuses `rift.node.*` traffic from every other remote identity, so other
  plugins cannot command agents or read their commands.
- The agent only runs programs from its `--allow-program` list, matched exactly
  against the first command argument.
- The agent only touches directories below `--root`: paths must be relative,
  without `.` or `..`, and symlinks are refused at every level.
- The controller is trusted. A stolen node token lets an attacker impersonate
  the node and read template files, but not run commands on the node: only the
  controller can publish node commands.
- Prepare commands contain the Velocity forwarding secret for Paper instances,
  inside the TLS-encrypted bus.

## Operate nodes

- Dashboard: the Servers page shows each server's node and a Nodes panel with
  reachability, labels, instances and allocated memory.
- HTTP: `GET /api/nodes`; every server in `/api/servers` has a `node` field
  (`local` for the controller). Server logs, console and Files work for remote
  servers; remote files are limited to 512 KiB through the web console.
- Control subject and CLI: `rift.control.nodes` and `rift admin nodes` (grant
  `servers`).
- Java: `RiftControl.nodes()` in rift-java.

### Wire protocol

Agents and the controller exchange JSON over the existing bus:

| Subject | Direction | Purpose |
| --- | --- | --- |
| `rift.node.<n>.cmd` | controller → agent | Request/reply: `prepare`, `start`, `stop`, `console`, `logs`, `remove`, `files`, `reconcile` |
| `rift.node.<n>.state` | agent → controller | Process list every 2 s and on every change; none for 10 s makes the node `unreachable` |

Logs are read on demand through `logs` commands with the same byte cursors as
local logs, so the existing logs API and dashboard work unchanged without
streaming every log line through the controller. See `src/node/protocol.rs` for
the message types.
