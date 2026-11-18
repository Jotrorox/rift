# Clustering: several active proxies

> [!NOTE]
> **Status: design proposal.** Nothing described here is implemented yet.
> This document is the plan for the branch that adds clustering. It grows
> with the branch and becomes the user guide when the branch merges. Sections
> marked *Open question* still need a decision.

Today exactly one Rift process accepts players. That process also holds the
message broker, the player registry, the node controller and the instance
database. A [standby](database.md#standby-controllers) can take over when it
stops, but the standby is a cold spare: players on the active proxy are
disconnected, and the network can never take more players than one process
can. `src/node/mod.rs` states this limit directly: "Multiple proxies or proxy
failover are out of scope: the broker and the player registry both live in the
controller process."

This is the most common reason large networks do not adopt Rift. Velocity
networks get several proxies from RedisBungee, which keeps a shared player
list in Redis. A shared list alone is not enough for Rift, because Rift also
owns the database, the bus, scaling and the nodes. This design makes every
Rift process an equal **peer** in a **cluster**. The peers replicate their
durable state with the Raft consensus algorithm, share one message bus and one
player list, and elect one of themselves to run the work that must happen
exactly once.

```text
                     players (DNS round robin, SRV records or an L4 load balancer)
          ┌──────────────────────┼──────────────────────┐
   ┌──────▼──────┐        ┌──────▼──────┐        ┌──────▼──────┐
   │ rift  eu-1  │◄──────►│ rift  eu-2  │◄──────►│ rift  eu-3  │   peer links: QUIC, mutual TLS
   │  (leader)   │  Raft  │ (follower)  │  Raft  │ (follower)  │   Raft log, bus federation,
   │ sqlite rep. │◄──────────────────────────────►│ sqlite rep. │   player actions, file blobs
   └──────┬──────┘        └──────┬──────┘        └──────┬──────┘
          │ game traffic, bus clients and node agents connect to any peer
          ▼                      ▼                      ▼
     backends · node agents · server plugins on the bus
```

## Goals

1. **Many active proxies.** Any number of peers accept players at the same
   time. Players see one network: one player list, one tab list, network-wide
   counts, `/server`, messages, kicks and transfers between players on
   different peers.
2. **No single point of failure.** Losing any minority of voting peers keeps
   the network running: logins continue, scaling continues, and nodes keep
   their servers and are adopted by the new leader. Only the players connected
   to a peer that dies are disconnected, and they can reconnect to another
   peer at once.
3. **One source of truth, replicated.** The instance database, the player
   directory, the permission store, the extension key-value store,
   configuration deployments and the operator audit trail are replicated by
   Raft to every peer. No external database, Redis or shared file system is
   needed.
4. **Same features, same configuration.** Plugins, Lua, the dashboard, the
   HTTP API, `rift admin` and bus clients work against any peer, with the
   same semantics as on a single Rift. A single Rift remains a valid
   deployment with no cluster settings and no extra cost.

### Non-goals

- Moving a live player connection between peers. A TCP connection ends on
  one peer. A peer that shuts down cleanly can send 1.20.5+ clients to another
  peer with the Transfer packet (see [rolling restarts](#rolling-restarts)),
  but a peer that crashes drops its players.
- Clusters spread across continents. Raft commits need a round trip to a
  majority, so voters should be within a few tens of milliseconds of each
  other. Distant proxies join as [learners](#voters-and-learners).
- Byzantine peers. Peers trust each other completely, as the proxy trusts
  its node agents today.

## Overview

| Concern | Today (one process) | In a cluster |
| --- | --- | --- |
| Durable state | SQLite file or PostgreSQL, one writer | A SQLite replica on every peer, changed only through the Raft log |
| Online players | `PlayerRegistry`, in memory | Replicated **presence** table, one record per player, tagged with its peer |
| Duplicate logins | Checked in process | Decided by a Raft entry, so exactly one peer wins |
| Message bus | One in-process `Broker` behind QUIC | One broker per peer; peers forward messages by subscription interest |
| Player actions | `mpsc` channel to the session | Local channel, or a request to the owning peer over the peer link |
| Scaling, nodes, managed servers | The proxy | The Raft **leader**, with fencing so a deposed leader cannot act |
| Servers on the proxy host | `local` | Each peer is also a node named after itself |
| Configuration and templates | Files beside `rift.lua` | Deployments and template manifests in the log; file contents as content-addressed blobs |

The rest of this document explains each row.

## Raft

### Library and storage

Rift uses [openraft](https://github.com/databendlabs/openraft). It is async,
runs on tokio (which Rift already uses), supports joint-consensus membership
changes, learners, snapshots, leader leases and linearizable reads, and leaves
storage and transport to the application. `raft-rs` (TiKV) was considered: it
is well proven, but synchronous, and needs more scaffolding for the parts
openraft provides.

Each peer keeps two SQLite files:

| File | Contents |
| --- | --- |
| `rift.raft.sqlite3` | The Raft log, the vote and the last purged log id. WAL mode, `synchronous=FULL`, because a vote or log entry acknowledged to the leader must survive a crash |
| `rift.sqlite3` (the instance database) | The state machine. Every table it holds today, plus `cluster_applied` (the last applied log id) and the cluster tables below |

The applied log id is written in the same SQLite transaction as the entry it
records. A crash therefore never applies an entry twice or skips one.

### Peer links

Peers talk over one QUIC connection per pair, on a dedicated
`cluster.listen` port with the ALPN `rift-cluster/1`. Both sides verify the
other's certificate against `cluster.ca` and prove that they hold the cluster
token (`RIFT_CLUSTER_TOKEN`) in the handshake. Each kind of traffic gets its
own streams on that connection, so a large snapshot never delays a heartbeat:

| Stream kind | Purpose |
| --- | --- |
| `raft` | AppendEntries, Vote, InstallSnapshot (chunked) |
| `bus` | Subscription interest and forwarded messages (see [message bus](#message-bus)) |
| `player` | Actions for players connected to the receiving peer |
| `blob` | Template and file contents by SHA-256 |
| `load` | Unreplicated, best-effort per-peer load reports, once per second |

During the handshake each peer also sends its version, the cluster features
it supports, and an HMAC of each shared secret (the forwarding secret, node
tokens, the bus-login key) keyed by the cluster token. A peer whose secrets
differ is refused, and its log names which secret differs, so a misconfigured
peer cannot reject players or nodes.

### Voters and learners

Raft needs a majority of **voters** for every commit. Three voters tolerate one
failure, five tolerate two. Every peer that is not a voter is a **learner**:
it replicates the log, takes players, serves the bus and forwards its writes
to the leader, but it does not vote. This keeps commit latency flat while a
network adds proxies: a network with twelve proxies might run three voters
and nine learners.

A two-proxy network adds a **witness**: `rift witness` is a voter that opens no
game listener and takes no bus clients. It runs on any small host and gives
the two proxies a third vote.

### The state machine

Every log entry is one of a small number of kinds, each versioned. An entry
kind is used only once every voter and learner advertises it, as node
features work today, so a rolling upgrade never sends an entry a peer cannot
apply.

| Entry | Applied as |
| --- | --- |
| `changeset` | A SQLite session changeset for transactional writes (see [writes](#writes-to-the-instance-database)) |
| `presence` | Claims, releases and server moves of online players, batched |
| `kv` | Puts, deletes and compare-and-set in the extension store |
| `directory` | Player directory upserts (name, last seen, language) |
| `stream` | Appends to retained streams and consumer acknowledgements |
| `peer` | Peer joined, peer expired, load summary, leader task term |
| `config` | A configuration deployment: revision, source and actor |
| `manifest` | A change to a template folder or file tree: paths, sizes, SHA-256 |
| `migrate` | Upgrade one component's schema to a version every peer supports |

Applying an entry must be deterministic. Entries never read the clock or a
random source while being applied: the proposer puts timestamps and generated
values into the entry.

### Snapshots

A snapshot is a consistent copy of the instance database, made with SQLite's
online backup API (or `VACUUM INTO`) and sent in chunks over the `raft`
stream. A new peer, or one that fell behind the purged log, installs the
snapshot into a temporary file, checks it, swaps it in and continues from the
snapshot's log id. Peers snapshot and purge their log after a configurable
number of entries (default 10,000) or 64 MiB of log, whichever comes first.

## Writes to the instance database

Rift's database code today takes the connection lock, opens a transaction,
reads, decides in Rust and writes (for example
`InstanceStore::allocate_name` in `src/instance_store.rs` or
`Authority::apply` in `src/permissions/authority.rs`). That logic cannot be
shipped to another peer, so writes in a cluster use **optimistic replication
of changesets**.

1. Every write becomes a closure, `database.write(|tx| ...)`, which may run
   more than once. This replaces the `lock()` plus `transaction()` pattern at
   the roughly dozen call sites. The single-process backends run the closure
   once, as today.
2. In a cluster the closure runs against the local replica inside a
   transaction that SQLite's
   [session extension](https://sqlite.org/sessionintro.html) records.
   rusqlite exposes it through its `session` feature. An authorizer callback
   records which tables the closure read. Afterwards the transaction is rolled
   back. The local replica changes only when the log says so.
3. The peer sends the leader the changeset, the set of tables it read and its
   **base**, the last log index it had applied.
4. The leader accepts the write if no entry after the base changed a row that
   the changeset also changes, and no entry after the base changed a table the
   closure read. The leader tracks the last index that changed each table and
   each primary key. Otherwise it rejects the write with its current index.
5. On rejection the peer waits until it has applied that index, then runs the
   closure again, up to a limit (default 5 attempts, then an error).
6. On acceptance the write returns once the entry is committed **and applied
   on the writing peer**, so a caller always reads its own write.

When the writing peer is the leader, it runs the closure under its write lock
against the latest applied state, so step 4 cannot reject it.

This gives serializable writes: the leader rejects every write whose reads or
writes overlap with a write it missed. Conflicts are rare because the
transactional writers are cold paths: permission edits, instance
registrations, group states, node schedules and name sequences. The hot paths
(player presence, directory upserts, key-value puts) are not changesets but
their own deterministic entry kinds, which never conflict.

Applying a changeset uses `sqlite3changeset_apply` with a conflict handler
that aborts. Because the leader validated the write, a conflict during apply
means the replica has diverged. The peer then stops applying, reports the
divergence (`rift_cluster_diverged`) and reinstalls a snapshot from the
leader.

Reads stay local and never wait for the network. They see the replica as of
its last applied entry, which on a healthy cluster trails the leader by
milliseconds. A caller that needs the latest state, such as `rift check
--strict` asking a running cluster, uses a linearizable read: openraft's
read-index confirms leadership, then the peer waits until it has applied that
index.

> [!IMPORTANT]
> A cluster uses the replicated SQLite backend. `database.backend =
> "postgres"` with a [standby](database.md#standby-controllers) stays
> available for networks that want one active proxy and an existing
> PostgreSQL server. A cluster on PostgreSQL would reintroduce an external
> single point of failure and is not planned. *Open question: is that the
> right call for networks that already run highly available PostgreSQL?*

## Players

### Presence

Every online player has one presence record, replicated to every peer:

| Field | Meaning |
| --- | --- |
| `uuid`, `name` | Identity; the name is unique case-insensitively, as today |
| `peer`, `connection_id` | The peer that holds the connection |
| `server` | The backend the player is on, or none while connecting |
| `visible_to` | The visibility filter used by `Viewer` today (vanish) |
| `protocol`, `locale`, `since` | What tab lists, the players API and the dashboard show |

`PlayerRegistry` (`src/players.rs`) keeps its API and its generation counter,
but reads from the replicated table, so the tab list (`{online}`,
`{server_online}`, network entries), the server-list count, the status page,
BungeeCord `PlayerCount`/`PlayerList`, managed-server player counts and
metrics count the whole network on every peer.

Connection ids become cluster-unique: the upper 16 bits hold the peer's
numeric id, which is assigned when it joins and stored in the membership.
`src/events.rs` keeps its process-local counter for the lower 48 bits. The
owning peer of any connection id can therefore be read from the id itself.

### Logins and duplicate logins

The name reservation and the UUID registration (`reserve_name` and
`register_visible`) become **claims**:

1. Before contacting a backend, the session proposes a `presence` claim for
   its lowercase name. Once the identity is confirmed, it proposes a claim for
   the UUID and name together.
2. Applying a claim succeeds only if no other live record holds the name or
   UUID. Every peer applies the same entries in the same order, so exactly one
   of two simultaneous logins wins, wherever they connected. The loser gets
   `LoginAlreadyConnected` or `LoginDuplicateName`, as today.
3. `cluster.max_players` (optional) caps the network: a claim beyond it fails
   deterministically, unless the claim carries a bypass decided by the
   login hook.
4. Dropping the session's registration proposes a release.

Claims add one commit (one round trip to a voter majority, typically 1–5 ms
in one data centre) to each login. Logins already wait far longer on Mojang
authentication. Claims from one peer are batched: a peer sends at most one
`presence` entry per 10 ms, with every claim, release and server move since
the last one.

### When a peer disappears

Every peer proposes a heartbeat entry (a `peer` entry, batched with presence)
at least once per second. If the leader sees none from a peer for
`cluster.peer_timeout` (default 10 s), it proposes **peer expired**, which
releases every presence record of that peer. Its players disappear from tab
lists and their names become free.

A peer that cannot reach a majority (a network partition) knows it, because
its own heartbeats stop committing. After `cluster.peer_timeout` it assumes it
has been expired:

- it refuses new logins, with `cluster.partition_message`;
- it keeps existing players connected and routed, because their backends may
  still be reachable;
- when it rejoins, it proposes fresh claims for its players. A claim that now
  conflicts, because the player logged in elsewhere meanwhile, disconnects the
  older session.

*Open question: should a partitioned peer also be able to admit logins in a
degraded "local only" mode, accepting a possible duplicate until it
rejoins?* The default above prefers correctness.

### Player actions on another peer

Every operation that today does `entry.actions.try_send(PlayerAction::…)` in
`src/extensions/players.rs` first looks up the player's presence:

- a local player gets the action through its channel, as today;
- a player on another peer gets it as a request on the `player` stream to
  that peer, which runs it locally and replies with the same result type
  (`sent`, `offline`, `queued`, …).

`PlayerAction` becomes serializable. Both paths sit behind the existing
`Players` methods, so these work across peers without changes of their own:
`rift.players.*` and `rift.network.*` subjects, `GET /api/players` and the
message and kick routes, the plugin and Lua players API, BungeeCord
`Message`, `MessageRaw`, `KickPlayer`, `Connect`, `ConnectOther` and
`ForwardToPlayer`, and `rift admin transfer`. Broadcasts go to every peer,
and each delivers to its own players.

Events that belong to a session (login, chat, commands, transfer decisions,
disconnect) are raised on the peer that holds the session. Every peer loads
the same plugins from the same configuration, so plugin code sees each
player exactly once. Network-wide `rift.events.players` is published on the
federated bus, so a subscriber on any peer sees every player event.

## Message bus

### Federation

Every peer runs its own `Broker` (`src/messaging/mod.rs`), and local Lua,
plugins and bus clients use it exactly as today. Peers federate in a full mesh
using the `bus` stream:

- **Interest.** Each peer tells every other peer which subject patterns its
  local subscribers use, and how many members each queue group has.
  Interest is sent incrementally with a sequence number, and in full after a
  reconnect. It is not stored in Raft.
- **Publish.** A message is delivered to local subscribers and forwarded once
  to each peer with matching interest. A message received from a peer is
  never forwarded again, so a full mesh needs one hop and has no loops.
- **Queue groups** pick one member across the cluster: a local member if
  there is one, otherwise a peer chosen by its member count.
- **Request and reply.** Inboxes become `_INBOX.<peer>.<random>` (today
  `_INBOX.<random>`, `src/messaging/mod.rs`), so a reply is routed by its
  prefix without waiting for the inbox's interest to spread.
- **Identity.** Clients authenticate on the peer they connect to. Forwarded
  messages carry the publishing principal, so subjects whose permissions depend
  on who publishes (`rift.node.*`, `rift.perms.*`, bus logins) are checked the
  same on every peer.

Bus clients receive a list of peers. `crates/rift-messaging`'s
`Client::connect` gains an address list and reconnects to the next peer when
one fails. The Java client and the bus-login files written to servers
(`rift-bus.pem` today) get the same list. `rift node --controller` becomes
repeatable.

### Retained streams

Retained streams (`src/messaging/stream.rs`) are bounded and low-volume, so
their appends and consumer acknowledgements become `stream` entries, and
every peer keeps the same stream contents. Retention is applied
deterministically when entries are applied. Acknowledgements are batched per
consumer. The permission change stream, which today lives in
`rift.perms-changes/` beside the configuration, is derived from permission
revisions in the replicated database and is identical on every peer.

### Sessions that must be unique

A node principal has one session at a time today: a new login closes the old
one. In a cluster, node sessions are claims like player names: a node agent
connecting to any peer claims `node.<name>`, and the peer that held the older
session closes it.

## Work that runs once: the leader

Some work must happen exactly once in the network. It runs on the current
Raft leader and nowhere else:

- the scaler (`src/scaling.rs`), instance creation, removal and moves;
- managed server and instance lifecycle: starting, stopping, idle stops,
  restart retries and recovery;
- node commands (`rift.node.<n>.cmd`) and node schedules (cordon, drain);
- template source syncs (git), cache warming;
- leader-only timers that plugins request (*open question: plugins need an
  API to say "run this on one peer only"; `rift.cluster.is_leader()` and a
  `leader` flag on timers are the candidates*).

The leader starts these tasks once it has applied every entry of its term, and
stops them the moment openraft reports it is no longer leader.

**Fencing.** Every node command carries the leader's Raft term. An agent
remembers the highest term it has seen and refuses commands with a lower one.
A deposed leader that has not yet noticed cannot start or stop servers.

**Inputs from other peers.** The leader decides from replicated state wherever
it can: player counts per server come from presence, instance records and
node schedules from the database. Reservations (players being sent to a server
that is starting) and queue lengths change too fast to replicate, so each
peer sends them on the `load` stream every second. The leader sums them,
and treats a peer whose report is older than 5 s as reporting nothing.

**Start on connect.** A session that needs a stopped server asks the leader
(a request to `rift.cluster.leader.start`) and waits until the server's
replicated state turns `running`, with the same timeout as today.

**Server states** (`running`, `starting`, `unknown`, the last error) are
written by the leader as batched `changeset` entries. Every peer routes from
the same view, and a new leader starts with the old leader's view instead of
an empty one.

### Servers on the peers' own hosts

Today `local` means the proxy's own host. In a cluster each peer runs an
embedded node agent and is a node named after itself, with labels from
`cluster.peers.<name>.labels`. Managed servers and service groups select peers
like any node. A server on a peer's host keeps running when that peer's proxy
process restarts if it uses [`keep_running`](kept-servers.md); otherwise it
stops with its peer, as local servers stop today. The name `local` is refused
in a cluster.

## Configuration and files

### One configuration

Every peer runs the same `rift.lua`. The peer's own name comes from
`--peer <name>` or `RIFT_PEER`, and is visible to the script as `rift.peer`,
so per-host settings stay in the one file:

```lua
rift.config.cluster = {
    listen = "0.0.0.0:7444",
    certificate = "certs/cluster.pem",
    private_key = "certs/cluster-key.pem",
    ca = "certs/cluster-ca.pem",
    token_env = "RIFT_CLUSTER_TOKEN",          -- the default
    peers = {
        ["eu-1"] = { address = "10.0.0.1:7444", voter = true },
        ["eu-2"] = { address = "10.0.0.2:7444", voter = true },
        ["eu-3"] = { address = "10.0.0.3:7444", voter = true },
        ["us-1"] = { address = "10.1.0.1:7444" }, -- a learner
    },
    peer_timeout_ms = 10000,
    max_players = 20000,
}

rift.config.listeners.public = "0.0.0.0:25565"
if rift.peer == "us-1" then
    rift.config.routes.public = "lobby-us"
end
```

Configuration **deployments** (the dashboard, `PUT /api/config`, `rift admin
reload` after an edit) become `config` entries. Every peer writes the
deployed source to its `rift.lua` atomically and reloads. The deployment
history (`deployments.json` today) and the operator audit trail
(`audit.jsonl`, both in `src/operator.rs`) become replicated tables, so
rollback and audit work from any peer. A peer that starts with a different
`rift.lua` than the last deployment logs which revision it differs from and
adopts the deployed one. *Open question: or refuse to start, so a hand edit
on one host is never silently overwritten?*

Membership follows the configuration. After a deployment changes
`cluster.peers`, the leader adds new peers as learners, waits for them to
catch up, promotes those marked `voter`, and removes dropped peers, one
membership change at a time. It refuses to remove voters if fewer than three
would remain, unless a witness is configured.

### Templates and other files

Template folders, plugin jars, maps and the software cache can be large, so
they never travel in the log. Instead:

- A `manifest` entry records a folder's files by path, size and SHA-256.
- Each peer stores contents by hash under `cache/blobs/` and fetches missing
  blobs from any peer that has them, over the `blob` stream.
- An upload or a git sync on any peer stores the blobs locally, copies them to
  a majority of voters, then proposes the manifest. A committed manifest
  therefore never references a blob that only one host has.
- `template_storage` on each peer is materialized from the manifest, so node
  agents keep fetching templates over `--web` from whichever peer they reach,
  with the same manifest endpoint as today.

Server directories on hosts (nodes and peers) stay where they are: they are
the data of running servers, not shared configuration.

### Secrets

Secrets stay in each peer's environment, as today: the cluster token, node
tokens, the forwarding secret, the web token and the bus-login key. They must
be equal on all peers; the handshake checks this without revealing them. The
web API's operator tokens and sessions move into replicated tables, so a
dashboard session works against any peer behind a load balancer.

## Failure behavior

| Situation | What happens |
| --- | --- |
| A follower or learner dies | Its players are disconnected and can reconnect to another peer at once. After `peer_timeout` their presence is released. Everything else continues. |
| The leader dies | The voters elect a new leader within the election timeout (default 1.5–3 s). Commits pause meanwhile: new logins wait, players already on backends notice nothing. The new leader resumes scaling and adopts every node's servers, as a restarted controller does today. |
| A minority of voters is partitioned | The majority side continues. The minority side keeps its players playing, refuses new logins after `peer_timeout` and reconciles its claims when it rejoins. |
| No majority anywhere | No commits: no logins, no configuration or permission changes, no scaling. Connected players keep playing on their backends, and node agents keep their servers. |
| A peer falls behind | It catches up from the log, or installs a snapshot if the log was purged. It takes players while catching up, but refuses logins until it is within 1 s of the leader, so its tab list and counts are not stale. |
| A replica diverges | Detected when applying a changeset. The peer stops taking logins, reinstalls a snapshot and reports `rift_cluster_diverged`. |
| A disk fills up | Writing the Raft log fails, so the peer stops acknowledging entries; Raft treats it as unavailable. |

### Rolling restarts

`rift admin cluster leave <peer>` (and SIGTERM in a cluster) drains a peer
gracefully:

1. it stops taking new logins and transfers leadership if it has it;
2. it sends 1.20.5+ clients to another peer with the Transfer packet, using
   `cluster.peers.<name>.public_address` of a healthy peer, and gives older
   clients a reconnect message;
3. it releases its presence records and closes its bus clients, which
   reconnect to other peers.

Upgrades run one peer at a time. Entry kinds and schema migrations are
enabled only when every peer advertises support (see
[the state machine](#the-state-machine)), so a mixed-version cluster always
works with the features of its oldest peer.

## Getting there from one Rift

1. `rift cluster init` on the existing Rift creates the Raft log and a
   one-voter cluster whose first snapshot is the current instance database.
   It also moves `deployments.json`, `audit.jsonl` and the template folders
   into the replicated store. A PostgreSQL installation first runs `rift
   database copy --to rift.sqlite3`.
2. Add `rift.config.cluster` with the new peers and deploy it. The leader adds
   them as learners as they come up, and promotes the voters once they catch
   up.
3. Put the peers behind DNS or a load balancer. Give bus clients and node
   agents the peer list.

`rift check --strict` against a cluster reports peers that are unreachable,
behind, on another version, or holding different secrets.

## Operating a cluster

| Where | What |
| --- | --- |
| CLI | `rift admin cluster` (members, roles, leader, term, applied index, lag), `rift admin cluster leave\|transfer-leader\|snapshot` |
| HTTP | `GET /api/cluster`, the same data; every server and player in the API gains a `peer` field |
| Dashboard | A Cluster panel with each peer's role, lag, players and load |
| Metrics | `rift_cluster_term`, `rift_cluster_is_leader`, `rift_cluster_commit_index`, `rift_cluster_applied_index`, per-peer `rift_cluster_replication_lag`, `rift_cluster_proposal_seconds`, `rift_cluster_write_retries_total`, `rift_cluster_bus_forwarded_total` |
| Control subject | `rift.control.cluster` and `rift.events.cluster` (leader changes, membership, peer expired) |

## Code changes

The seam is `Snapshot::build` in `src/runtime.rs`, which today constructs the
broker, extensions, player registry, nodes and managed servers together.

| Area | Change |
| --- | --- |
| `src/cluster/` (new) | Peer links, openraft network and storage, the state machine, membership reconciliation, leader tasks, the blob store |
| `src/database.rs` | `Database::write` closures; a `Replicated` backend that records changesets and waits for apply; per-table and per-key change tracking on the leader |
| `src/players.rs`, `src/extensions/players.rs`, `src/events.rs` | Presence-backed registry, claims, routed `PlayerAction`s, cluster-unique connection ids |
| `src/messaging/` | Interest exchange and forwarding in `Broker`; peer-prefixed inboxes; principals on forwarded messages |
| `crates/rift-messaging`, Java client | Address lists and failover |
| `src/managed.rs`, `src/scaling.rs`, `src/node/` | Leader-only start; fencing terms in node commands; peers as embedded nodes; load reports |
| `src/control.rs`, `src/operator.rs` | Deployments, history and audit as replicated entries |
| `src/extensions/storage.rs`, `src/directory.rs`, `src/messaging/stream.rs` | Their own entry kinds instead of direct SQL |
| `src/config.rs`, `src/check.rs` | `cluster` settings, `rift.peer`, strict checks against a running cluster |

## Plan

This branch merges once everything below is done. Each step lands as its own
commits with tests, and the single-process behavior stays unchanged
throughout.

1. **Design** (this document).
2. **Cluster core.** Peer links, openraft with SQLite log storage, membership
   from configuration, learners, witness, snapshots, `rift admin cluster`,
   `/api/cluster`, metrics. No application state yet.
3. **Replicated database.** `Database::write` at every call site; changeset
   replication with optimistic validation; `directory` and `kv` entries;
   `rift cluster init`. Every database-backed feature works on any peer.
4. **Presence.** Claims, cluster-wide duplicate-login checks,
   `cluster.max_players`, peer expiry, partition behavior, cluster-unique
   connection ids, network-wide counts.
5. **Bus federation.** Interest, forwarding, queue groups, prefixed inboxes,
   replicated streams, node session claims, multi-address clients.
6. **Routed player actions.** `PlayerAction` over the `player` stream, and
   every API, subject and BungeeCord channel on top of it.
7. **Leader tasks.** Scaling, managed servers, nodes and template syncs on the
   leader only, with fencing, load reports, start-on-connect through the
   leader, peers as embedded nodes.
8. **Configuration and files.** Replicated deployments, history, audit, web
   sessions; template manifests and the blob store.
9. **Queues and admission.** [Server queues](queues.md) become network-wide:
   the leader holds the queue, and peers enqueue, dequeue and receive their
   turn over the bus.
10. **Hardening.** Rolling restarts with the Transfer packet, `rift check
    --strict` for clusters, and failure tests: a three-peer cluster in CI
    whose leader is killed with SIGKILL, partitions simulated by a
    fault-injecting peer transport, and a deterministic simulation of the
    state machine and the optimistic write path that asserts no duplicate
    logins and no lost acknowledged writes.

## Open questions

- PostgreSQL in clusters: never, or as an alternative state store for networks
  that already run it highly available?
- Logins on a partitioned minority: refuse (proposed), or allow with a later
  reconciliation?
- A peer whose `rift.lua` differs from the deployed revision: adopt the
  deployed one (proposed), or refuse to start?
- The plugin API for leader-only work.
- Whether learners should accept writes (forwarded to the leader, as
  proposed) or be read-only.
