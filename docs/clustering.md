# Clustering: several active proxies

A cluster is several Rift processes, called **peers**, that accept players at
the same time and act as one network. Players see one player list, one tab
list and network-wide counts, and `/server`, messages, kicks and transfers
work between players on different peers. The peers replicate their durable
state with the Raft consensus algorithm, share one message bus, and elect one
of themselves, the **leader**, to run the work that must happen exactly once:
scaling, managed servers, node commands and backups.

```text
                     players (DNS round robin, SRV records or an L4 load balancer)
          ┌──────────────────────┼──────────────────────┐
   ┌──────▼──────┐        ┌──────▼──────┐        ┌──────▼──────┐
   │ rift  eu-1  │◄──────►│ rift  eu-2  │◄──────►│ rift  eu-3  │   peer links: QUIC, mutual TLS
   │  (leader)   │  Raft  │ (follower)  │  Raft  │ (follower)  │
   └──────┬──────┘        └──────┬──────┘        └──────┬──────┘
          │ game traffic, bus clients and node agents connect to any peer
          ▼                      ▼                      ▼
     backends · node agents · server plugins on the bus
```

No external database, Redis or shared file system is needed: every peer keeps
a full replica in its own SQLite instance database.
[How clustering works](clustering-internals.md) explains the design for
contributors.

## Cluster or standby?

Rift offers two ways to survive the loss of a proxy host:

| | Cluster | [Standby controller](database.md#standby-controllers) |
| --- | --- | --- |
| Active proxies | All peers accept players | One; the standby waits |
| When a proxy host dies | Only its players are disconnected; they log in again through another peer | Every player is disconnected; the standby starts when the database lock is released |
| Player capacity | Grows with the peers | One process |
| Durable state | SQLite, replicated by Raft to every peer | PostgreSQL or MariaDB, which you run and keep available |
| Hosts | Three voters, or two proxies and a [witness](#voters-learners-and-witnesses) | Two proxies and a database server |
| Addresses | Players reach any peer (DNS, SRV, load balancer) | One address that moves to the standby |

Choose a cluster when one process is not enough for your players, when a
proxy failure must not disconnect everybody, or when you do not want to run a
database server. Keep a single Rift, with or without a standby, when one
process is enough and you already operate highly available PostgreSQL or
MariaDB. The two cannot be combined: a cluster always replicates SQLite.

## Requirements

- **The same Rift release on every peer.** Peers of different releases run
  together only while a [rolling upgrade](#upgrades) passes through them.
  Every build includes clustering; there is no build option for it.
- **SQLite.** The instance database is the `instance_database` file.
  `database.backend = "postgres"` or `"mariadb"` together with a
  `cluster.toml` or a `cluster` section is refused. A PostgreSQL or MariaDB
  installation copies its data first (see
  [getting there from one Rift](#getting-there-from-one-rift)).
- **Three voters** to survive the loss of one, five to survive two. Two
  proxies need a third host as a witness. Voters should be within a few tens
  of milliseconds of each other, since every login and write waits for a
  majority of them. Distant proxies join as learners.
- **A certificate authority** for the peer links, and a certificate and key
  per peer that it signs (see [certificates](#certificates-and-the-token)).
- **A cluster token** of at least 16 bytes, the same on every peer, in the
  environment variable `RIFT_CLUSTER_TOKEN` (or the one `token_env` names).
- **The same secrets on every peer:** the forwarding secret, node tokens and
  the web tokens. The peer links refuse a peer whose forwarding secret or
  node tokens differ. The admin token may differ per host, since each admin
  endpoint is local.
- **Network.** Peers reach each other on the UDP port of `listen` in
  `cluster.toml`. Bus clients and node agents reach every peer's
  `messaging.listen` port (UDP), and players reach every peer's game
  listener (TCP).

## Set up a cluster

This walkthrough builds three peers, `eu-1` (10.0.0.1), `eu-2` (10.0.0.2) and
`eu-3` (10.0.0.3), from an existing Rift on `eu-1`.
[examples/cluster](../examples/cluster/rift.lua) has a complete
configuration, the three `cluster.toml` files, a witness and systemd units.

### 1. Create the certificates

The peer links use mutual TLS. Each peer presents its certificate both when
it dials and when it accepts, so the certificate must name the host of the
peer's `advertise` address (as a DNS name or an IP address) and allow both
server and client authentication: no extended key usage at all, or both
`serverAuth` and `clientAuth`. Create a CA once and a certificate for each
peer:

```sh
mkdir cluster && cd cluster
# The cluster CA. Keep ca.key offline; peers need only ca.pem.
openssl req -x509 -new -nodes -days 3650 \
  -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout ca.key -out ca.pem -subj "/CN=rift cluster CA" \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign"

# One certificate per peer, naming the host of its advertise address.
for peer in eu-1:10.0.0.1 eu-2:10.0.0.2 eu-3:10.0.0.3; do
  name=${peer%%:*} ip=${peer#*:}
  openssl req -new -nodes -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
    -keyout "$name.key" -out "$name.csr" -subj "/CN=$name"
  printf 'subjectAltName=IP:%s\nextendedKeyUsage=serverAuth,clientAuth\n' "$ip" > "$name.ext"
  openssl x509 -req -in "$name.csr" -CA ca.pem -CAkey ca.key -CAcreateserial \
    -days 825 -out "$name.pem" -extfile "$name.ext"
done
```

Use `subjectAltName=DNS:eu-1.internal` instead when `advertise` names a host.
Copy `ca.pem`, `<peer>.pem` and `<peer>.key` to each peer, readable by the
account Rift runs as.

Create the token once and put it in every peer's environment:

```sh
openssl rand -hex 32    # RIFT_CLUSTER_TOKEN=<this value>, on every peer
```

### 2. Write `cluster.toml` on each host

`cluster.toml` sits beside `rift.lua`. It holds what a peer needs before it
can reach the cluster, which is why it is not part of the replicated
configuration:

```toml
peer = "eu-1"
listen = "0.0.0.0:7444"
advertise = "10.0.0.1:7444"
certificate = "/etc/rift/cluster/eu-1.pem"
private_key = "/etc/rift/cluster/eu-1.key"
ca = "/etc/rift/cluster/ca.pem"
seeds = ["10.0.0.2:7444", "10.0.0.3:7444"]
public_address = "eu-1.play.example.com:25565"

[labels]
region = "eu"
```

Every key is described under [`cluster.toml`](#clustertoml). `rift check`
validates the file; `rift check --strict` also loads the certificate, key
and CA and checks the token.

### 3. Initialize the cluster on the first peer

Stop Rift on `eu-1`, then run, beside its `rift.lua` and `cluster.toml`:

```sh
rift cluster init --peer eu-1
```

`--peer` must match `peer` in `cluster.toml`. This creates a cluster whose
only voter is `eu-1` and whose state is this Rift's instance database. What
else it moves is listed under
[getting there from one Rift](#getting-there-from-one-rift). It prints what
to change in the configuration before the peer starts, such as servers that
still run on `local`.

Then start Rift on `eu-1` as usual (`rift` or `rift --config rift.lua`). It
now runs as the cluster's leader.

### 4. Start and add the other peers

On `eu-2` and `eu-3`, put the certificates and `cluster.toml` in place, set
the token, and start Rift without a `rift.lua` (`rift`, or
`rift --config <path>` with a path that does not exist yet). The peer waits
until a member adds it, installs a snapshot of the cluster's state, then
writes the deployed configuration to `rift.lua` and starts with it. A peer
started with a `rift.lua` of its own must hold a revision the cluster
deployed (see [deployments](#deployments-and-hand-edits)).

On any member, add each new peer with its `advertise` address and, once it
has caught up, make it a voter:

```sh
rift admin cluster add eu-2 10.0.0.2:7444
rift admin cluster add eu-3 10.0.0.3:7444
rift admin cluster                 # wait until both are up
rift admin cluster promote eu-2
rift admin cluster promote eu-3
```

`add` dials the address, checks that the peer there is the one named, hands
it its Raft id and adds it as a learner. `promote` refuses a learner that is
more than 1,000 entries behind the leader. Finally run `rift check --strict`
on each peer: it reports peers that are unreachable, behind, on another
version or holding other secrets.

### 5. Point players, bus clients and nodes at every peer

- **Players** reach the peers through DNS round robin, an SRV record per
  peer, or a TCP load balancer. Behind a load balancer, enable the
  [PROXY protocol](hardening.md#run-behind-a-load-balancer) on every peer.
  `public_address` in each `cluster.toml` must reach that one peer the way
  a player does, since a peer that [drains](#rolling-restarts) sends players
  there.
- **Bus clients** get every peer's address. Each peer's is its own:
  `bus_address` in its `cluster.toml`, else the host of its `advertise` with
  the port of `messaging.listen` (the messaging section is shared, so
  `messaging.advertise` cannot tell peers apart). Servers with a
  [bus login](bus-logins.md) find the addresses in `RIFT_BUS_ADDRESSES`,
  their own peer's first, and the Java client uses that list to fail over.
  Bus clients verify every peer's messaging certificate for one name
  (`messaging.advertise_server_name`, else the host of `messaging.advertise`,
  else each peer's own host), so the certificates name it.
- **Node agents** take one `--controller` per peer, each peer's bus address
  (see [nodes](nodes.md#run-an-agent)); the agent connects to the first that
  accepts and moves on when one fails.

### Getting there from one Rift

`rift cluster init` turns a single Rift's state into the cluster's first
state:

- The instance database becomes the replicated state. A PostgreSQL or
  MariaDB installation first runs `rift database copy --to rift.sqlite3`
  and removes `database.backend` from the configuration (see
  [copy between backends](database.md#copy-between-backends)).
- A key-value store in a file of its own (`extensions.storage.path`) is
  copied into the instance database; set
  `extensions.storage.database = true` afterwards, since a cluster refuses
  the file.
- Instances registered on `local` move to the initializing peer's own
  node. Their folders stay where they are when they already lie where that
  node keeps them, below the directory of `rift.lua`; otherwise init says
  where to move them.
- The bus-login key (`local/instance-key`) moves into the cluster, so the
  tokens this Rift issued stay valid on every peer.
- The deployment history (`deployments.json`), the operator records of
  `audit.jsonl` and the template folders move into the cluster, and the
  current `rift.lua` becomes the last deployment.

Init refuses a peer whose Raft file already belongs to a cluster, and an
instance database that already holds another cluster's state. Init reads
the configuration as the peer will: `rift.peer` is the peer's name, and
servers and groups may name peer-nodes (any valid peer name, checked once
the peers run). Servers still on `local` do not stop init; it names them,
and they must get a node before the peer starts.

## `cluster.toml`

Relative paths are relative to the directory of `rift.lua`, which also holds
the Raft log (`rift.raft.sqlite3`), the last snapshot
(`rift.raft.snapshots/`) and, by default, the instance database. Unknown keys
are refused, and the file may be at most 64 KiB.

| Key | Default | Meaning and validation |
| --- | --- | --- |
| `peer` | required | This peer's name, also its node name. 1–64 lowercase letters, digits, `-` or `_`; not `local` |
| `listen` | required | `ip:port` the peer links listen on (UDP, QUIC) |
| `advertise` | `listen` | `host:port` other peers dial. Required when `listen` is a wildcard address. The certificate must name its host |
| `certificate` | required | This peer's certificate (PEM), signed by the cluster CA |
| `private_key` | required | Its private key (PEM) |
| `ca` | required | The cluster CA (PEM); peers' certificates must chain to it |
| `token_env` | `RIFT_CLUSTER_TOKEN` | The environment variable that holds the cluster token, at least 16 bytes |
| `seeds` | none | `host:port` addresses of other peers to contact first; at most 64 |
| `public_address` | none | `host:port` where players reach this peer, for [transfers](#rolling-restarts) from a draining peer |
| `bus_address` | host of `advertise`, port of `messaging.listen` | `host:port` where bus clients and node agents reach this peer's messaging listener. The peer records it for the other peers, and its servers find it first in `RIFT_BUS_ADDRESSES`. With a wildcard `advertise` and no `bus_address`, the peer uses `messaging.advertise` as a single Rift does |
| `witness` | `false` | This host is a [witness](#voters-learners-and-witnesses); `rift witness` sets it |
| `[labels]` | none | Labels of this peer's [node](#servers-on-the-peers-own-hosts): keys of 1–64 bytes, values up to 256 bytes; keys starting with `node.` are reserved |
| `[node] memory_mb` | none | Memory this peer's node offers servers, 1 to 16777216 |
| `[node] max_instances` | 128 | Instances on this peer's node, 1 to 4096 |
| `[node] min_free_disk_mb` | as for [nodes](nodes.md) | Free disk below which the node takes no new instance, 0 to 16777216 |

Every `host:port` value takes a DNS name, an IPv4 address or a bracketed IPv6
address, with a nonzero port.

## `rift.config.cluster`

The settings every peer shares, in `rift.lua`. The section has no effect on
a Rift without `cluster.toml`, except that `rift check` then flags plugins
that are not cluster-aware.

```lua
rift.config.cluster = {
    peer_timeout_ms = 10000,
    partition_grace_ms = 60000,
    max_players = 20000,
}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `peer_timeout_ms` | 10000 | The leader expires a peer it has not heard from for this long, releasing its players. A peer that has not heard from the leader for half of it stops admitting players. 2000 to 3600000 |
| `partition_grace_ms` | 60000 | How long players on a peer that lost contact stay connected before it disconnects them. 1 to 86400000 |
| `max_players` | none | Players in the whole network. A login beyond it is refused with `cluster.max_players` unless a login hook grants a bypass. 1 to 100000000 |
| `partition_message` | none | Shown to players a cut-off peer refuses or disconnects; 1–1024 bytes. Without it they get the [translatable](text.md) `cluster.partition_message` |

A peer reads this section when it starts; a change takes effect on each peer
at its next restart.

## Commands

### `rift cluster` and `rift witness`

These run on the peer's host, beside its `rift.lua` and `cluster.toml`, and
take `--config <path>` for a `rift.lua` elsewhere.

| Command | What it does |
| --- | --- |
| `rift cluster init --peer <name>` | Creates a cluster whose only voter is this peer, from this Rift's instance database. Run it while Rift is stopped |
| `rift cluster init --peer <name> --from-backup <file>` | The same, from a [backup](#backup-and-recovery) instead of the local database |
| `rift cluster recover --peer <name> --i-understand-data-loss` | Makes this peer the only voter when the other voters are gone for good. Run it while Rift is stopped on this host |
| `rift witness` | Runs this host as a [witness](#voters-learners-and-witnesses) until SIGTERM (`systemctl stop`) or Ctrl-C (SIGINT), either of which stops it cleanly |

### `rift admin cluster`

`rift admin cluster` talks to the local peer's [admin endpoint](admin.md).
Reading the status needs the `status` permission; every other command needs
`cluster`. Membership commands run on the leader: a follower forwards them,
so they work from any peer.

| Command | What it does |
| --- | --- |
| `cluster`, `cluster status` | This peer's view: members, roles, leader, term, indexes and each peer's state (see [status](#status)) |
| `cluster status --leader` | The leader's view, fetched over the peer link. Only the leader knows how far each peer is behind (`lag`) |
| `cluster add <name> <host:port> [--witness]` | Adds a peer as a learner. Its Rift must be running with its `cluster.toml`; `<host:port>` is its `advertise` address |
| `cluster promote <name>` | Makes a learner a voter once it is at most 1,000 entries behind |
| `cluster demote <name> [--force]` | Makes a voter a learner |
| `cluster remove <name> [--force]` | Removes a peer from the cluster and releases its players |
| `cluster replace <name> [--force]` | Re-adds a peer that lost its Raft file, under a new Raft id. If the peer is not running yet, `add` with the same name and address finishes the job later |
| `cluster leave <name>` | Hands leadership away if the peer has it, and makes the peer [drain and exit](#rolling-restarts) |
| `cluster transfer-leader [<name>]` | Moves leadership to the named voter, or to the best reachable, caught-up voter (a player-serving one before a witness) |
| `cluster backup <file>` | Writes a [backup](#backup-and-recovery) of the cluster's state |

- **The voter rule.** A change that would leave fewer than three voters (a
  witness counts as one) is refused unless it has `--force`. Growing from
  fewer is always allowed, and the last voter can never be removed.
- **One change at a time.** A second membership change is refused while the
  first is still being committed; retry shortly.
- **Raft ids** are never reused, and a cluster assigns at most 255. Every
  `add` of a new name and every `replace` uses one. A cluster that used them
  all starts again from a backup with `rift cluster init --from-backup`,
  which assigns ids from 1.
- **Elsewhere.** `GET /api/cluster` returns the status over HTTP, and the bus
  subject [`rift.control.cluster`](messaging-subjects.md) runs every command
  except `backup`.

## Certificates and the token

- Every peer certificate must be signed by the CA in `ca`, name the host of
  its `advertise` address, and allow server and client authentication.
- The token is the only secret that keeps a holder of a CA-signed
  certificate out of another cluster, so give every cluster its own CA or at
  least its own token. Peers prove that they hold it without sending it.
- During the handshake each peer also proves which forwarding secret and
  node tokens it holds. A peer that differs is refused, and the log names
  the secret that differs.
- To change a certificate, replace the files and restart that peer. To
  change the CA or the token, every peer needs the new value; peers with
  different values cannot talk, so change them during a maintenance window.

## Voters, learners and witnesses

Raft needs a majority of the **voters** for every commit: three voters
tolerate one failure, five tolerate two. Every other peer is a **learner**.
A learner replicates the state, takes players, serves the bus and sends its
writes to the leader, but it does not vote, so adding learners does not slow
commits. A network with twelve proxies might run three voters and nine
learners, with the voters close to each other.

A **witness** is a voter that takes no players and no bus clients. It makes
two proxies a cluster that survives the loss of one host. Run it on a third
host with its own `cluster.toml` and certificate:

```sh
rift witness                                           # on the witness host
rift admin cluster add w-1 10.0.0.9:7444 --witness     # on a member
rift admin cluster promote w-1
```

When the cluster sets `forwarding.secret_env` or node tokens, the witness
needs the same `rift.lua` as the members, with the files it names (plugins,
icons, certificates) and the same secrets in its environment: the members
refuse a peer whose forwarding secret or node tokens differ, and a witness
reads them from its `rift.lua`. It takes no deployments, so its copy only
has to keep those secrets current. Without such settings `rift.lua` is
optional on the witness host.

A witness holds the whole state, including the bus-login key and template
files, so treat its host like a proxy host. It may lead for a moment, but it
hands the leadership to a player-serving voter as soon as one can take it,
since the leader talks to every node agent. Witnesses are not nodes and run
no servers.

## How a cluster behaves

### Logins and players

- **Duplicate logins** are decided by the cluster: every login claims the
  player's name and UUID, and exactly one of two simultaneous logins of the
  same player wins, wherever they connected. The other gets
  `login.already_connected` or `login.duplicate_name`, as on a single Rift.
- **A claim costs one commit**: a round trip to the leader and from it to a
  majority, typically a few milliseconds in one data centre.
- **`cluster.max_players`** caps the whole network when a login's claim is
  decided.
- **One player list.** Each peer tells the others which server its players
  are on, their skins, tab order and visibility. The tab list, the
  server-list count, the status page, BungeeCord `PlayerCount`,
  `PlayerList`, `IP` and `UUIDOther` and managed-server player counts
  cover the whole network on every peer; the `rift_players_online` metrics
  count each peer's own players. `rift admin status` lists
  every peer's players, each with its `peer`.
- **Player actions** work on players of any peer: `rift.players.*` and
  `rift.network.*`, the HTTP player routes, the plugin and Lua players API,
  BungeeCord `Message`, `KickPlayer`, `Connect`, `ConnectOther` and the
  forwarding channels, and `rift admin transfer`. A peer sends the action to
  the player's peer and returns its answer. Clicks on
  [dialogs](dialogs.md) and the events of [inventory menus](menus.md) go
  back to the plugin on the peer that showed them.
- **Session events** (login, chat, commands, transfer decisions,
  disconnect) run on the peer that holds the session, so plugin code sees
  each of them once.
- **Bans and permissions** apply on every peer as soon as it has applied the
  change, usually within milliseconds; the player's own peer kicks a banned
  player and pushes changed permissions to the backend.
- **Server queues** form one line per destination across all peers; see
  [queues in a cluster](queues.md#in-a-cluster).

### Message bus

Every peer runs a broker, and the brokers federate: a message reaches every
subscriber in the cluster that should see it, forwarded at most once to each
peer. Clients notice the differences listed in
[the messaging protocol](messaging-protocol.md#clusters).

#### Subject scopes

Each subject has a scope that decides whether and how it crosses peers:

| Scope | Subjects | Behavior |
| --- | --- | --- |
| `local` | `rift.control.*`, `rift.players.*`, `rift.network.*`, the permission request subjects, `rift.server.state`, the rest of `rift.node.*` | Answered by the peer the client is connected to; never sent to other peers. That peer passes the work on where needed: player actions to the player's peer, writes to the leader, server states to every peer |
| `applied` | `rift.events.instances`, `rift.events.permissions`, `rift.events.reload`, `rift.events.servers`, `rift.events.cluster`, `rift.perms.update`, `rift.perms.changes` | Every peer publishes them to its own subscribers when it applies the change, so each subscriber gets exactly one copy |
| `origin` | Every other `rift.events.*` subject (`players`, `control`, `lifecycle`, `plugins`, …) | Published on the peer where the event happens. Bus clients on other peers that subscribe receive them; Lua and plugins see only their own peer's |
| `owner` | `rift.node.<name>.cmd`, `rift.node.<name>.state` | Commands go to the peer holding the node's session; states go to the leader |
| `cluster` | Everything else: your own and your plugins' subjects | Delivered to subscribers on every peer |

- A subscription to a `cluster` subject can ask for `scope = "local"` to hear
  only its own peer: `messaging.subscriptions[].scope`, and
  `rift.subscribe(subject, { queue = ..., scope = "local" }, fn)` in
  `rift.lua` and in [Lua plugins](plugins-lua.md). WebAssembly plugins
  always subscribe with `cluster` scope.
- A queue group delivers each message to one member in the whole cluster,
  preferring a member on the publisher's peer.
- Requests and replies work across peers; inboxes are
  `_INBOX.<peer>.<random>`.
- A peer queues at most 16,384 messages or 32 MiB for another peer; beyond
  that it drops them and counts them in `rift_cluster_bus_dropped_total`.

#### Bus logins, streams and node sessions

- **[Bus logins](bus-logins.md)** work on every peer: the key that derives
  their tokens is part of the replicated state, and `RIFT_BUS_ADDRESSES`
  lists every live peer, the server's own peer first.
- **[Retained streams](messaging.md#retained-streams)** are served by the
  leader, whichever peer a client uses. Durable streams are replicated with
  their records and acknowledgements; after a leader change, records that
  were delivered but not acknowledged are delivered again (streams promise
  at-least-once delivery). Memory streams start empty after a leader change.
  Records that file-backed streams held before `rift cluster init` are not
  carried into the cluster.
- **The permission change stream** (`rift.perms.changes`) is kept by each
  peer and holds the same records everywhere; consumer positions on it are
  per peer.
- **Node agents** may connect to any peer. A node has one session in the
  whole cluster: a new session on another peer closes the old one.

### Work that runs on the leader

The leader runs everything that must happen once:

- the scaler, instance creation, removal and moves;
- managed servers: starts, stops, idle stops, restarts after a crash and
  recovery, and the record of each server's state that every peer routes
  by;
- node commands and node schedules (cordon, drain), with each agent
  [fenced](#failure-behavior) to the leader's term;
- scheduled backups and restores;
- recovery of half-done creations, removals and moves left by a previous
  leader;
- writes derived from the configuration, and the permission expiry sweep;
- the stream service, the queue service and git template syncs;
- `rift.every` jobs with the default `scope = "cluster"`.

Requests for this work may be made on any peer. A follower hands them to the
leader: starts and stops, start on connect, instance creation and removal,
moves (`rift admin move` and the HTTP move routes, with progress shown on the
follower) and cancels, backups and restores, forgetting backups, cache
warming and git syncs. The leader runs them for the operator who made them
and records them in the audit log as `forwarded.<kind>`. Writes to the
database (permission edits, node schedules and the like) are executed by the
leader too, at the cost of one round trip. Before stopping an idle server,
the leader asks every peer whether a player is on the way there.

### Servers on the peers' own hosts

Every peer that is not a witness is also a **node** named after itself. It
runs a node agent inside its own process, so a peer's host needs no
`rift node` and no `nodes.<peer>` entry:

- the node's address is the host of the peer's `advertise` address;
- its labels are every key of `[labels]` in its `cluster.toml` and every
  value that is a valid label (`region = "eu"` gives `region` and `eu`);
- its limits come from `[node]` in its `cluster.toml`;
- its servers live below the directory of `rift.lua`, in the
  [node layout](nodes.md#run-an-agent) (`local/services/`, `temp/services/`),
  and it reads templates from the peer's own `template_storage`;
- it runs any program the configuration names, as servers on a single
  Rift's host do.

Managed servers and service groups select peers like any other node
(`node = "eu-1"`, `nodes = { "eu-1", "eu-2" }` or
`nodes = { labels = { "eu" } }`), and `rift admin nodes` and `GET /api/nodes`
list them with `"peer": true`. A cluster has no `local`: configuration
validation refuses managed servers, service groups and instances without a
node. A `nodes.<peer>` entry in the configuration takes precedence; its
`rift node` agent then serves that host and the peer embeds none.

The servers on a peer's host stop when its proxy process stops, after the
sessions drained, except named servers with
[`keep_running`](kept-servers.md) on the peer's own node, which keep running
and are adopted when the peer comes back (under systemd this needs
`KillMode=process`, as on a single Rift). A peer-node is `unreachable` while
its peer is expired.

### Plugins and jobs

Every peer loads every plugin. In-memory plugin state is per peer, jobs run
on the leader unless they ask for `scope = "peer"`, and work that must run
once takes a lease. [Plugins in a cluster](plugins.md#plugins-in-a-cluster)
describes what to check in a plugin and the `cluster = true` declaration in
`plugin.toml`; [scheduled work](host-services.md#scheduled-work) and
[leases](host-services.md#leases-in-a-cluster) describe the APIs.

### One configuration

Every peer runs the same `rift.lua`. The script sees the peer's own name as
`rift.peer` (nil outside a cluster), so differences between hosts stay in the
one file:

```lua
local admin_ports = { ["eu-1"] = 9091, ["eu-2"] = 9092, ["eu-3"] = 9093 }
rift.config.listeners.public = "0.0.0.0:25565"
rift.config.admin = { listen = "127.0.0.1:" .. (admin_ports[rift.peer] or 9091) }
```

Only **peer sections** may depend on `rift.peer`: the listeners (with the
metrics, admin and status listeners and the PROXY protocol), routes (with
`on_route`), limits (with the rate limits), health checks, the server list
(with its cache and `on_status`), the tab list and the web listener. Every
other section is **shared** and must come out the same on every peer,
because it writes durable state or drives the leader's work: backends,
managed servers, templates, service groups, nodes, the instance database,
authentication, forwarding, extensions, plugins, messaging, permissions,
`cluster` and the rest.

#### Deployments and hand edits

A configuration change made through the dashboard, `PUT /api/config`, a
structured edit, a rollback, or a reload (`rift admin reload`, SIGHUP,
`POST /api/reload`) of an edited `rift.lua` becomes a **deployment**:

1. The peer that takes the change evaluates it as every member would, and
   refuses it if a shared section differs between members, naming the peers
   and sections.
2. The deployment is committed. Every peer writes it to its `rift.lua`
   atomically and reloads.
3. A peer whose reload fails keeps its previous configuration and reports
   why under `configuration` in `rift admin cluster` and `GET /api/cluster`.
   It does not retry until the next deployment or a manual reload.

A reload whose file is already the last deployment is a plain reload. The
deployment history (the last 64 deployments) and the operator audit trail
(the last 1,024 records) are replicated, so `GET /api/deployments`,
rollbacks and `GET /api/audit` give the same answer on every peer; see
[the dashboard](http.md#in-a-cluster).

When a peer starts, it compares its `rift.lua` with the deployments:

| Its `rift.lua` | What happens |
| --- | --- |
| Missing | It receives the last deployment once it has caught up |
| The last deployment | It starts with it |
| An older deployed revision (one of the last 64) | It adopts the last deployment and writes it to `rift.lua` |
| No deployed revision (edited by hand) | It refuses to start and keeps the file. Deploy the edit from a running peer instead, or start this peer with `--adopt-deployed`, which keeps the hand-edited file as `rift.lua.hand-edited` and runs the last deployment |

Lua modules (`require`) and plugin folders are files on each host and are not
replicated. A peer that evaluates a shared section differently because its
files differ is reported by `rift check --strict` and in the cluster status,
and logged when peers connect.

### Templates and other files

`template_storage` is the same on every peer. Each peer writes it from a
replicated list of every folder's files and fetches the contents from other
peers by their SHA-256 (stored under `cache/blobs/` beside `rift.lua`).

- An upload, a delete through the Files page or `/api/files/templates`, and a
  git sync change the list on every peer. A write is answered once a
  majority of voters hold the new files, and fails while fewer are
  reachable. See [the dashboard](http.md#template-and-server-files) and
  [templates](templates.md#in-a-cluster).
- Files placed into `template_storage` by hand on one host are not
  replicated; they are replaced or removed when that folder changes.
  Top-level names starting with a dot are not replicated.
- Git syncs run on the leader, so every voter needs `git` and the
  credentials.
- The software cache stays per peer: each peer downloads server software
  when it first needs it.
- Server directories on nodes and peers stay where they are: they are the
  data of running servers.

### Key-value stores, secrets and tokens

- The extension key-value store must live in the instance database
  (`extensions.storage.database = true`), where it is replicated. A store
  in a file of its own is refused.
- A plugin's store write waits at most five seconds. During a leader
  election (1.5–3 s) writes wait; a longer outage makes them fail with a
  timeout.
- Secrets stay in each peer's environment and must be equal on all peers:
  the cluster token, node tokens, the forwarding secret and the web tokens.
- Web tokens come from the configuration, so a token works on every peer.
  The random token a Rift creates when none is configured is that process's
  own.

## What stays per peer

| State | Why |
| --- | --- |
| Connection admission: `rate_limit`, `login_rate_limit`, `limits.max_connections`, `limits.max_connections_per_address` | Each peer counts what it accepts ([hardening](hardening.md#in-a-cluster)); a global bucket is global to the peer. The network-wide cap is `cluster.max_players` |
| Backend health checks | Each peer measures its own path to the backends and routes by it |
| `rift admin maintenance` and `rift admin drain` | Runtime switches of the peer they run on; `maintenance` and `draining` in `rift.lua` apply to every peer |
| Metrics | Per process; aggregate them in Prometheus |
| Plugin and Lua in-memory state, `rift.timers`, WebAssembly timers | Each peer runs its own plugin instances; only `rift.every` jobs have a scope |
| The software cache | A cache of public downloads |
| Lua modules and plugin folders | Files on each host |
| Summaries of refused dashboard requests | Each process's own records |
| The attempts `rift admin backups` lists | Each peer shows the attempts it saw |

## Failure behavior

| Situation | What happens |
| --- | --- |
| A follower or learner dies | Its players are disconnected. When it restarts, its new incarnation releases their claims at once; otherwise the leader expires it after `peer_timeout` and they can log in elsewhere. Everything else continues |
| The leader dies | The voters elect a new leader within the election timeout (1.5–3 s). Meanwhile nothing commits: new logins and writes wait, players already on backends notice nothing. The new leader fences and reconciles every node agent, recovers half-done operations and resumes scaling |
| A peer loses contact with the leader | After half of `peer_timeout` it refuses new logins with `cluster.partition_message`. Its players stay connected for `partition_grace`; if contact has not returned by then, it disconnects them so they can log in through a healthy peer. The leader expires it after the full `peer_timeout` and holds back idle stops of the servers its players were on until `partition_grace` has passed |
| A minority of peers is partitioned | The majority side continues. The minority side behaves as above |
| No majority anywhere | No commits: no logins, no configuration or permission changes, no scaling. Connected players keep playing for `partition_grace`. Node agents keep their servers |
| A peer comes back after it was expired | It joins again and claims its players anew; a player who logged in elsewhere meanwhile is disconnected from this peer |
| A peer falls behind | It catches up from the log, or installs a snapshot (an interrupted transfer resumes). It refuses logins with `login.unavailable` until it has applied what the leader had applied a second earlier; players already connected keep playing |
| A replica diverges | Detected by the periodic checksum or a failed apply. The peer stops taking logins, repairs itself from a copy of the leader's state and reports `rift_cluster_diverged` meanwhile |
| A disk fills up | Writing the Raft log fails and the peer stops acknowledging entries; the cluster treats it as unavailable |

The leader acts only while a majority of voters has acknowledged it recently
(its lease), and everything it decides carries its term. A leader that is cut
off stops acting before another can be elected, and a deposed leader's late
writes and node commands are refused: node agents remember the highest term
they have seen in `.rift-fence` below their root and refuse lower ones. An
agent too old to be fenced gets no control commands in a cluster;
`rift check --strict` reports it.

## Rolling restarts

A peer **drains** on SIGTERM (`systemctl stop`), Ctrl-C,
`rift admin shutdown`, or `rift admin cluster leave <peer>` run on any
member:

1. Its game listeners stop taking connections.
2. If it leads, it hands the leadership to another voter.
3. It tells the other peers that it admits no more players, and picks
   **transfer targets**: every other peer that is up, not expired,
   a voter or learner (not a witness), has `public_address` in its
   `cluster.toml` and answers that it admits players (it is not cut off,
   catching up or draining itself). It waits up to five seconds for one.
4. Every 1.20.5+ player (in configuration or play) gets the Transfer packet
   with a target's `public_address`; the targets are taken in turn, so the
   players spread. Older clients, players who cannot be transferred at that
   moment, and everyone when there is no target are disconnected with
   `cluster.leaving` ("This proxy is restarting. Please reconnect.";
   [translatable](text.md)). The log says
   `rift: cluster: leaving: transferred N players to other peers and disconnected M`.
5. The sessions drain within `shutdown_timeout_ms`, releasing their claims
   as they end; the bus listener closes, so bus clients and node agents
   reconnect to other peers; the peer's own servers stop (except kept named
   servers); then the peer leaves Raft and exits.

A transferred client connects to the target with the transfer intent. Rift
accepts that from 1.20.5+ clients, and a cluster peer sends its backend a
normal login for it (a handoff between peers is no transfer from the
backend's point of view), so backends need no `accepts-transfers=true`. A
single Rift passes the transfer intent on as before.

To restart or upgrade a cluster, take one peer at a time:

```sh
# On the peer's host: drain, replace, start.
sudo systemctl stop rift
sudo install -m 0755 ./rift /usr/local/bin/rift
sudo systemctl start rift
# On any peer: wait until it is up again and not behind before the next.
rift admin cluster status --leader
```

Keep the supervisor's stop deadline above the five-second target wait plus
`shutdown_timeout_ms`. Restart voters one at a time so a majority stays up.
After `rift admin cluster leave`, the peer's process exits; start it again
the same way.

## Upgrades

Peers of different releases work together during a rolling upgrade, with the
features of the oldest member:

- A new kind of log entry or a schema migration is used only after every
  member announced that it supports it. A member that is down still counts,
  so it holds new features back until it is upgraded or removed.
- Once every member is upgraded, the leader enables the new features and
  migrates the schemas, on every peer at the same point in the log.
- A peer that cannot apply an enabled feature is refused when it connects,
  so a downgraded peer cannot rejoin once its successor's features are in
  use. **Going back after a schema migration means restoring a backup**
  taken before the upgrade, as for a single Rift (see
  [the database only moves forward](upgrading.md#the-database-only-moves-forward)).

`rift check --strict` and `rift admin cluster` show each peer's version until
the upgrade is finished. Take a [backup](#backup-and-recovery) before each
upgrade.

## Backup and recovery

**Back up** from any peer while the cluster runs:

```sh
rift admin cluster backup /var/backups/rift/cluster-$(date +%F).sqlite3
```

The command needs the `cluster` permission. The path is made absolute where
the command runs, and the file is written on the host of the peer whose admin
endpoint you use; it must not exist yet. The copy is consistent: the peer
first catches up with everything committed (a linearizable read), then copies
its replica. The result is one plain SQLite file without `-wal` or `-shm`.
It holds the whole replicated state, including the deployment history, the
audit trail, the permission store and the bus-login key, so keep it as safe
as the hosts.

**Start a new cluster from a backup** on a host with a `cluster.toml` and no
instance database yet:

```sh
rift cluster init --peer eu-1 --from-backup /var/backups/rift/cluster-2026-11-19.sqlite3
```

The new cluster has a new id, one voter and Raft ids from 1; the old
cluster's peers, player claims, node sessions, bus addresses and per-peer
reports are cleared. Since the backup holds the deployment history, put the
last deployed revision beside `cluster.toml` as `rift.lua` (or start the
peer with `--adopt-deployed`). Init reads the configuration as the new peer
does, with `rift.peer` set to its name. Once running, a peer needs every
node its configuration names, so when servers or groups are placed on
peer-nodes, restore onto peers named like the old ones. Then start Rift and
add peers as on a first setup.

**Recover from the loss of a majority.** When the other voters are gone for
good, a surviving peer can continue alone:

```sh
sudo systemctl stop rift
rift cluster recover --peer eu-1 --i-understand-data-loss
sudo systemctl start rift
rift admin cluster remove eu-2 --force
rift admin cluster remove eu-3 --force
rift admin cluster add eu-4 10.0.0.4:7444    # then promote, as on a first setup
```

Writes that only the lost peers had acknowledged are lost, hence the flag. A
peer that lost its own disk while the others survive is re-added with
`rift admin cluster replace` instead.

## Limits

- **255 Raft ids** per cluster, because connection ids carry the peer in 8
  bits. Ids are never reused. A peer whose id does not fit admits no
  players.
- **Connections do not move between peers.** Only a draining peer hands its
  1.20.5+ players over; a peer that crashes drops its players.
- **Voters must be close.** Every commit waits for a majority of voters, so
  their round-trip time adds to every login.
- **Admission limits are per peer**, see
  [what stays per peer](#what-stays-per-peer).
- **`rift.timers` and WebAssembly timers tick on every peer**, WebAssembly
  subscriptions always have `cluster` scope, and WebAssembly plugins cannot
  take leases. Use `rift.every` with `scope = "cluster"` or a lease from Lua
  for work that must run once.
- **Writes wait during elections.** Logins, store writes and other writes
  wait while there is no leader; plugin store writes give up after five
  seconds.
- **Memory streams** start empty after a leader change.
- **Moves forwarded from a follower** show progress only on the follower
  that forwarded them, and fail there if the leader changes while the move
  runs; check the move on the new leader.
- **`keep_running`** works for named servers on a peer's own node, not for
  service groups.
- **Byzantine peers** are out of scope: peers trust each other completely.

## Operating a cluster

### Status

`rift admin cluster` and `GET /api/cluster` (permission `read`) return the
same object; over HTTP it also has `"enabled": true`, and a Rift without
`cluster.toml` answers `{"enabled": false}`.

| Field | Meaning |
| --- | --- |
| `peer`, `cluster_id`, `raft_id`, `incarnation` | This peer, its cluster, its Raft id and the incarnation it runs as (a new one on every start) |
| `joined` | Whether this peer belongs to a cluster yet |
| `witness` | Whether this peer is a witness |
| `state`, `term`, `leader`, `is_leader`, `lease` | Its Raft state (`leader`, `follower`, `learner`, `candidate`), the term, the leader's name, whether this peer leads and whether it holds the leader lease |
| `last_log_index`, `commit_index`, `applied_index`, `snapshot_index`, `log_bytes` | Its log and replica |
| `diverged` | Its replica diverged and is being repaired |
| `membership_changing` | A membership change is still being committed |
| `peers` | One entry per peer: `name`, `raft_id`, `incarnation`, `role` (`voter`, `witness`, `learner`, `pending`), `address`, `public_address`, `leader`, `this_peer`, `up`, `expired`, `last_heard_ms`, `lag` (entries behind the leader; only on the leader, see `status --leader`), `version`, `labels` (including the announced `node.*` limits) and `players` (connected to that peer) |
| `configuration` | The last `deployment` (`id`, `revision`, `actor`, `action`, `timestamp_unix_ms`, `shared_hash`), each peer's report (`peer`, `reported`, `deployment`, `in_sync`, `shared_hash`, `shared_differs`, `reload_error`) and the replicated `templates` (`folders`, `files`, `bytes`) |

`rift check --strict` on a peer reports, besides the checks of a single Rift:

- `cluster.toml` errors, a missing or short token, a certificate, key or CA
  that does not load, and a missing `public_address` (a warning);
- PostgreSQL or MariaDB combined with a cluster, and a key-value store in a
  file of its own;
- shared sections that differ between members, whether `rift.lua` is the
  last, an older or no deployed revision, and peers whose reload failed;
- through the running peer's admin endpoint: a peer not joined yet, a
  diverged replica, no leader, peers that are unreachable or refused this
  peer (another token, other secrets, another cluster), expired peers, peers
  on another version, peers (this one included) more than 1,000 entries
  behind the leader, and node agents that cannot be fenced.

Plain `rift check` also flags persistent plugins without `cluster = true`.

On a peer, `GET /api/players`, `GET /api/servers`, `rift.players.list`,
`rift.control.servers`, `rift admin status` and `rift admin servers` give
every player and server a `peer`: the peer holding the player's connection,
and the peer whose own node runs the server (`null` on a `rift node` agent).
A single Rift's output has no `peer`.

### Dashboard

The [dashboard](http.md#the-cluster-panel)'s Overview page shows a Cluster
panel on every peer, from the same status: each peer's role, reachability,
last heard, incarnation, lag, players, load, version and configuration
state, its labels and limits, the term, leader and indexes, and a warning
while the replica diverged or there is no leader. Its Servers table gets a
Peer column. Operators with the web `cluster` permission can hand the
leadership to another voter and drain a peer from the panel
(`POST /api/cluster/transfer-leader`, `POST /api/cluster/peers/{name}/leave`;
see the [HTTP API](http-api.md#status-and-metrics)), which run as
`rift admin cluster transfer-leader` and `leave` do.

### Metrics

Each peer serves its own series on the [metrics listener](monitoring.md#metrics):

| Metric | Type | Meaning |
| --- | --- | --- |
| `rift_cluster_term` | gauge | The current Raft term |
| `rift_cluster_is_leader` | gauge | 1 on the leader |
| `rift_cluster_lease` | gauge | 1 while this peer leads and holds the leader lease |
| `rift_cluster_commit_index` | gauge | The highest entry this peer knows is committed |
| `rift_cluster_applied_index` | gauge | The highest entry applied to this peer's replica |
| `rift_cluster_log_bytes` | gauge | Size of this peer's Raft log |
| `rift_cluster_diverged` | gauge | 1 while this peer's replica is diverged |
| `rift_cluster_peer_up{peer, role}` | gauge | 1 for each peer this peer reaches |
| `rift_cluster_replication_lag{peer}` | gauge | Entries each peer is behind; served by the leader only |
| `rift_cluster_proposal_seconds` | histogram | Time from proposing an entry on this peer until it applied here |
| `rift_cluster_operations_total{kind, outcome}` | counter | Proposals made on this peer by entry kind (`claim`, `operation`, `kv`, …) and outcome (`ok`, `refused`, `failed`) |
| `rift_cluster_bus_forwarded_total{peer}` | counter | Bus messages forwarded to each peer |
| `rift_cluster_bus_dropped_total{peer}` | counter | Bus messages dropped for each peer (queue full or a failed batch) |
| `rift_cluster_config_in_sync{peer}` | gauge | 1 when the peer runs the last deployment |
| `rift_cluster_config_reload_failed{peer}` | gauge | 1 when the peer's last reload failed |
| `rift_cluster_config_shared_mismatch{peer}` | gauge | 1 when the peer runs the last deployment but evaluates a shared section differently |

Alert on no leader (`max(rift_cluster_is_leader) == 0`), on
`rift_cluster_peer_up == 0`, on `rift_cluster_diverged == 1`, on a growing
`rift_cluster_replication_lag`, and on `rift_cluster_config_reload_failed`.

### Events

`rift.events.cluster` (`applied` scope: every peer publishes to its own
subscribers) carries:

- `{"event":"leader","term":4,"leader":"eu-2","peer":"eu-1"}` when this peer
  sees a new leader or term;
- `{"event":"membership","voters":[…],"learners":[…]}` after a membership
  change;
- `{"event":"peer_added"|"peer_joined"|"peer_expired","name":"eu-3","incarnation":7}`
  and `{"event":"peer_removed","name":"eu-3"}` as the peer table changes.

[`rift.control.cluster`](messaging-subjects.md) answers status requests and
runs membership commands for principals with the `control` permission.

### Log lines

| Line | Meaning |
| --- | --- |
| `cluster: <peer> leads in term <n>`, `no longer leads` | Leadership changed on this peer |
| `cluster: <peer> serves players as peer <id>, incarnation <n>` | The peer has its identity and may admit players |
| `cluster: added <name> at <address> as learner`, `promoted`, `demoted`, `removed` | Membership changes, on the leader |
| `cluster: refused a link from <peer> at <address>: <reason>` | Another token, other secrets, another cluster or an unusable certificate |
| `cluster: <peer> evaluates the shared configuration sections differently` | Files on the hosts differ; `rift admin cluster` shows which sections |
| `cluster: <peer> lost contact with the leader; it refuses logins` | The peer fenced itself |
| `cluster: <peer> is still cut off after <n> ms; disconnecting its players` | The partition grace ran out |
| `cluster: <peer> is in contact with the leader again` | Fencing ended |
| `cluster: <n> players logged in elsewhere while this peer was expired; disconnected here` | A rejoining peer lost claims |
| `cluster: this replica diverged at entry <n>` | Divergence; the peer repairs itself |
| `cluster: replica repaired from a copy at entry <n>` | The repair finished |
| `cluster: raft id <n> of <peer> does not fit in a connection id` | The peer admits no players; start a new cluster from a backup |
| `cluster: leaving: transferred <n> players to other peers and disconnected <m>` | A drain handed its players over |
| `cluster: peer <name> is a node; its servers live below <dir>` | The embedded node agent started |
