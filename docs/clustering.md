# Clustering: several active proxies

> [!NOTE]
> **Status: design accepted, revision 2.** Step 2 of the [plan](#plan), the
> cluster core, is implemented (`src/cluster/`); where it had to differ from
> this design, [step 2 notes](#step-2-notes) say so. The rest is the plan
> for the branch that adds clustering. It grows with the branch and becomes the user guide when the
> branch merges. Revision 2 follows a review of revision 1. The choices it
> rests on were decided by the owner and are recorded under
> [decisions](#decisions), with the options that were weighed.

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
   │ sqlite rep. │◄──────────────────────────────►│ sqlite rep. │   presence, player actions, blobs
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
   to a peer that dies are disconnected. They can log in again through another
   peer as soon as the dead peer restarts or is expired, at most
   `cluster.peer_timeout` (default 10 s) later.
3. **One source of truth, replicated.** The instance database, the player
   directory, the permission store, the extension key-value store,
   configuration deployments and the operator audit trail are replicated by
   Raft to every peer. No external database, Redis or shared file system is
   needed.
4. **Same features, same configuration.** Plugins, Lua, the dashboard, the
   HTTP API, `rift admin` and bus clients work against any peer, with the
   same semantics as on a single Rift, except where this document says
   otherwise. A single Rift remains a valid deployment with no cluster
   settings and no extra cost.

### Non-goals

- Moving a live player connection between peers. A TCP connection ends on
  one peer. A peer that shuts down cleanly can send 1.20.5+ clients to another
  peer with the Transfer packet (see [rolling restarts](#rolling-restarts)),
  but a peer that crashes drops its players.
- Clusters spread across continents. Raft commits need a round trip to a
  majority, so voters should be within a few tens of milliseconds of each
  other. Distant proxies join as [learners](#voters-learners-and-witnesses).
- Byzantine peers. Peers trust each other completely, as the proxy trusts
  its node agents today.

## Overview

| Concern | Today (one process) | In a cluster |
| --- | --- | --- |
| Durable state | SQLite file, PostgreSQL or MariaDB, one writer | A SQLite replica on every peer, changed only by applying the Raft log |
| Writes | A closure under the connection lock | A typed operation, executed by the leader; the resulting changeset is replicated |
| Online players | `PlayerRegistry`, in memory | Name and UUID **claims** in the log; player attributes broadcast between peers |
| Duplicate logins | Checked in process | Decided by applying a claim, so exactly one peer wins |
| Message bus | One in-process `Broker` behind QUIC | One broker per peer; every subject has a [scope](#subject-scopes) that says whether and how it crosses peers |
| Player actions | `mpsc` channel to the session | Local channel, or a request to the owning peer over the peer link |
| Scaling, nodes, managed servers | The proxy | The Raft **leader**, gated by a quorum lease and fenced by term |
| In-memory mirrors of the database | Loaded at startup | Refreshed by **apply hooks** after every applied entry |
| Servers on the proxy host | `local` | Each peer is also a node named after itself |
| Configuration and templates | Files beside `rift.lua` | Deployments and template manifests in the log; file contents as content-addressed blobs |

## Raft

### Library and storage

Rift uses [openraft](https://github.com/databendlabs/openraft). It is async,
runs on tokio (which Rift already uses), supports joint-consensus membership
changes, learners, snapshots and linearizable reads, and leaves storage and
transport to the application. `raft-rs` (TiKV) was considered: it is well
proven, but synchronous, and needs more scaffolding for the parts openraft
provides. Where this document relies on a specific openraft behavior (leader
step-down, leases), the behavior is verified in the first implementation step
and wrapped where it falls short.

Each peer keeps two SQLite files:

| File | Contents | Durability |
| --- | --- | --- |
| `rift.raft.sqlite3` | The Raft log, the vote, the cluster id, this peer's Raft id and the last purged log id | WAL, `synchronous=FULL`: a vote or entry acknowledged to the leader must survive a crash |
| `rift.sqlite3` (the instance database) | The state machine: every table it holds today, plus `cluster_applied` and the cluster tables below | WAL, `synchronous=NORMAL`: entries lost in a crash are re-applied from the log |

The applied log id is written in the same SQLite transaction as the entry it
records, so a crash never applies an entry twice or skips one.

The instance database today runs with one connection behind one mutex, in
`journal_mode=DELETE` with `synchronous=FULL` (`src/database.rs`). In a
cluster, applying entries would then block every read on the login path. The
replicated backend therefore switches to WAL with one writer connection,
used only by apply and by the leader's execution of operations, and a small
pool of read connections.

### Peer links

Peers talk over QUIC on a dedicated `listen` port with the ALPN
`rift-cluster/1`. Both sides verify the other's certificate against the
cluster CA, prove that they hold the cluster token, and present the cluster
id; a peer with another cluster id is refused. Each pair keeps two
connections, so bulk transfers never share a congestion window with
consensus traffic:

| Connection | Stream kinds | Purpose |
| --- | --- | --- |
| control | `raft` | AppendEntries, Vote, heartbeats |
| control | `presence` | Player attributes and server moves (see [presence](#presence)) |
| control | `player` | Actions for players connected to the receiving peer |
| control | `bus` | Interest and forwarded messages (see [message bus](#message-bus)) |
| control | `load` | Reservations and stop vetoes (see [inputs from other peers](#inputs-from-other-peers)) |
| bulk | `snapshot`, `blob` | Snapshots, template and file contents by SHA-256 |

**Liveness comes from peer links, not from log entries.** Peers do not write
heartbeats into the log; the leader tracks when it last heard from each peer
on the control connection. This keeps the log small (see
[snapshots](#snapshots-and-log-purging)).

During the handshake each peer also sends its version, the entry kinds it
supports, the hash of the [shared configuration](#one-configuration), and an
HMAC of each shared secret (the forwarding secret, node tokens) keyed by the
cluster token. A peer whose secrets differ is refused, and its log names which
secret differs.

### Voters, learners and witnesses

Raft needs a majority of **voters** for every commit. Three voters tolerate one
failure, five tolerate two. Every other peer is a **learner**: it replicates
the log, takes players, serves the bus and sends its operations to the leader,
but it does not vote. This keeps commit latency flat while a network adds
proxies: a network with twelve proxies might run three voters and nine
learners. Learners must be able to send operations, because every login
writes claims.

A two-proxy network adds a **witness**: `rift witness` is a voter that opens no
game listener and takes no bus clients. It holds the full state machine, so it
holds everything the instance database holds, including the bus-login key.
Treat its host like a proxy host. A witness hands leadership to a
player-serving voter whenever one is available, since the leader talks to
every node agent.

### Membership

Membership is changed by explicit commands, not by editing `rift.lua`, so a
typo in a deployment cannot remove two voters:

```sh
rift cluster init --peer eu-1            # once: a one-voter cluster from this Rift's data
rift admin cluster add eu-2 10.0.0.2:7444            # joins as a learner and catches up
rift admin cluster promote eu-2                      # becomes a voter once caught up
rift admin cluster demote eu-3 | remove eu-3
rift admin cluster replace eu-3                      # after a peer lost its disk
```

- Each command is one membership change. The leader refuses a change that
  would leave fewer than three voters (or two plus a witness) unless it is
  given `--force`.
- A peer's Raft id is assigned when it is added and stored in its Raft file.
  A peer that comes back with an empty Raft file under a name that is still a
  member is refused, because it could vote twice in one term. `replace`
  removes and re-adds it under a new id.
- A peer's own settings (its name, listen address, certificate, key, CA,
  token variable and the addresses of other peers to contact first) live in
  a small per-host file, `cluster.toml` beside `rift.lua`, or the equivalent
  command line flags. They are needed before the peer can reach the cluster
  that holds the replicated configuration, so they cannot come from it.

### The state machine

Every log entry is one of a small number of kinds, each versioned:

| Entry | Applied as |
| --- | --- |
| `operation` | A changeset produced by the leader executing typed operations, with each operation's result (see [writes](#writes-to-the-instance-database)) |
| `claim` | Player name and UUID claims and releases, node session claims |
| `kv` | Key-value mutations, applied with their quota checks, returning a result |
| `directory` | Player directory upserts (name, last seen, language) |
| `stream` | Appends to retained streams and consumer acknowledgements |
| `peer` | Peer joined (with a new incarnation), peer expired |
| `config` | A configuration deployment: revision, source and actor |
| `manifest` | A change to a template folder: paths, sizes, SHA-256 |
| `features` | The set of entry kinds and settings every member supports |
| `migrate` | Upgrade one component's schema to a version every member supports |
| `checksum` | A hash of the state machine at this index, compared by every peer |

Rules for every kind:

- **Deterministic apply.** Applying never reads the clock or a random source.
  The leader stamps each entry with its time, so timestamps are monotonic
  across the cluster; generated values come from the leader's execution.
- **Idempotent proposals.** Each operation carries `(peer, incarnation,
  serial)`. The state machine keeps the last serial and its result per peer
  incarnation, so an operation retried after a leader change returns the
  stored result instead of failing (for example `begin_creation` on a primary
  key it already inserted).
- **Feature gating.** An entry kind is used only after a committed `features`
  entry lists it. The leader computes that set over all members, including
  ones that are down: a member that is down blocks activation until it comes
  back or is removed.
- **Schema changes only through `migrate`.** DDL, `PRAGMA user_version` and
  startup inserts (such as the permission store's initial rows) are not
  captured by changesets. The migrations that run when a component opens
  today (`src/instance_store.rs`, `src/permissions/authority.rs`) run instead
  when a `migrate` entry is applied, on every peer at the same index.
- **Divergence detection.** The leader proposes a `checksum` entry every
  10,000 entries (a hash over every table, ordered by primary key). A peer
  whose hash differs stops taking logins, reports `rift_cluster_diverged` and
  reinstalls a snapshot.

### Snapshots and log purging

The state machine is durable on its own, so a peer can purge its log up to
its applied index once every member has replicated those entries, or
regardless after 64 MiB of log, since a member that falls behind can install a
snapshot. A snapshot file is built only when a member needs one, with SQLite's
online backup API from a read connection, and is sent over the bulk
connection in chunks.

The instance database can reach gigabytes: the key-value quota allows up to
1 GiB per owner (`src/extensions/storage.rs`), streams default to 64 MiB each,
and the player directory grows without bound. Snapshots are therefore rare and
resumable, and a peer that is several snapshots behind is better replaced
from a backup (see [backup and restore](#backup-and-restore)).

**Installing a snapshot while serving players:**

1. Pause apply and leader execution on this peer; stop taking logins.
2. Write the snapshot to a temporary file, check its integrity and schema
   versions, and swap it in.
3. Reopen the writer and read connections, which also drops their prepared
   statement caches.
4. Run every apply hook in *full reload* mode (see below), synthesizing the
   join, leave and permission-update events that the difference between the
   old and new state implies.
5. Resume apply and logins.

### Apply hooks

Today several components load the database into memory once and then trust
their memory:

- group states and node schedules (`src/runtime.rs`, `serve`);
- registered instances in `Config.instances` (`src/instance_store.rs`);
- named servers that `rift admin move` moved, in `Config.moved_servers`
  (`src/instance_store.rs`);
- the permission authority's groups, revision and compiled cache
  (`src/permissions/authority.rs`);
- the key-value mirror (`src/extensions/storage.rs`).

In a cluster another peer's operation changes the database under them. Each
of these components registers an **apply hook**. After every applied entry,
the state machine calls the hooks whose tables changed, with the changed keys,
and each hook refreshes its memory and raises the events it would have raised
had the change been local. The same hooks run in full reload mode after a
snapshot install. The single-process backends call the same hooks after each
local write, so one code path serves both.

## Writes to the instance database

Rift's database code takes the connection lock, opens a transaction, reads,
decides in Rust and writes (for example `InstanceStore::allocate_name` in
`src/instance_store.rs`). Writes are spread over roughly 8 transactions and
about 27 lock sites, many of them single statements (`src/directory.rs`,
`src/instance_store.rs`).

In a cluster, **the leader executes every write**:

1. Each write becomes a typed, serializable **operation**: a permission
   `Change` (already normalized and serializable in
   `src/permissions/authority.rs`), `BeginCreation`, `AllocateName`,
   `SetGroupState`, `SetNodeSchedule`, and so on. The closed set of call sites
   makes this feasible.
2. A peer sends the operation to the leader. The leader runs the existing
   code for it on its own replica, inside a transaction that SQLite's
   [session extension](https://sqlite.org/sessionintro.html) records. The
   leader runs queued operations one after another in one open transaction,
   each in its own savepoint, so a failing operation rolls back alone and
   later operations see earlier ones.
3. The leader takes the combined changeset and each operation's result, rolls
   the transaction back, and proposes one `operation` entry.
4. Every peer, the leader included, applies the changeset when the entry
   commits. The caller's peer returns the result once the entry is applied
   **locally**, so a caller always reads its own write.
5. While an entry is in flight, the leader keeps executing new operations on
   top of a transaction that already contains the in-flight changesets
   (group commit), so throughput is not limited to one operation per round
   trip.

Because only the leader executes, against the latest state, under one write
lock, every write is serializable without read sets, retries or conflict
detection. The cost is one round trip to the leader for writes made on
followers. Those writes are cold paths: permission edits, instance
registrations, group states and node schedules.

The hot paths are not changesets but their own deterministic entry kinds:
claims (logins), `directory` (joins), `kv` (plugin storage) and `stream`.
`kv` covers the full multi-key `mutate` with its quota checks, and its apply
returns the result the caller expects.

**Applying a changeset:**

- Foreign key actions are disabled during apply
  (`SQLITE_CHANGESETAPPLY_FKNOACTION`), because the changeset already
  contains the rows that a cascade deleted: `perm_node` cascades from
  `perm_subject` (`src/permissions/store.rs`). rusqlite 0.42's `apply` passes
  no flags, so this is a small FFI call to `sqlite3changeset_apply_v2`.
- Any conflict aborts. Since the changeset was computed against the same
  state at the same index, a conflict means divergence and leads to a
  snapshot install.
- At startup the replicated backend asserts that every table has a primary
  key (sessions do not record tables without one) and that the schema
  version of every component matches the last applied `migrate` entry. A
  changeset is never applied against a table SQLite would silently skip.

**Reads** stay local and never wait for the network. They see the replica as
of its last applied entry, which on a healthy cluster trails the leader by
milliseconds. A caller that needs the latest state uses a linearizable read
(the leader confirms its leadership, then the peer waits until it has applied
that index). Ban checks on login use local reads; a ban is enforced on every
peer within the replication lag, and the session that a ban targets is
kicked by its owning peer when the ban applies.

> [!IMPORTANT]
> A cluster uses the replicated SQLite backend. `database.backend =
> "postgres"` or `"mariadb"` with a
> [standby](database.md#standby-controllers) stays available for networks
> that want one active proxy; combining either with `cluster` is a
> configuration error (decision 5).

## Players

### Presence

Presence is split by what needs consensus:

| Part | Fields | How it travels |
| --- | --- | --- |
| **Claims** | lowercase name, UUID, peer, incarnation, connection id | `claim` entries in the Raft log |
| **Attributes** | server, display name, signed skin properties, tab order, visibility filter, IP address, protocol, locale, since | Broadcast on the `presence` stream from the owning peer, with a per-peer sequence number and a full resync on reconnect |

Only uniqueness needs consensus. Attributes change often (every server
switch) and are large (signed skin properties are 1–3 KB), and only the
owning peer changes them, so putting them in the log would cost snapshot and
log volume for nothing. Every peer combines its replica's claims with the
attributes it has received into the view that `PlayerRegistry`
(`src/players.rs`) serves. The tab list (`{online}`, `{server_online}`,
network entries with skins), the server-list count, the status page,
BungeeCord `PlayerCount`, `PlayerList`, `IP`/`IPOther` and `UUIDOther`,
managed-server player counts and metrics then count the whole network on
every peer. Tab list rebuilds are rate-limited, so a burst of joins does not
rebuild every tab list once per join.

### Incarnations and connection ids

Every time a peer starts, it proposes `peer joined` with a new
**incarnation**. Applying it releases every claim of that peer's older
incarnations, so players of a peer that crashed and restarted can log in
again at once, and a full-cluster restart starts with no stale claims.

Connection ids become cluster-unique: the upper bits hold the peer's numeric
id and its incarnation, and the lower bits the process-local counter from
`src/events.rs`. The owning peer of any connection id can be read from the
id, and an action addressed to a connection of an older incarnation is
refused instead of reaching another player's session.

### Logins and duplicate logins

The name reservation and the UUID registration (`reserve_name` and
`register_visible`) become claims:

1. Before contacting a backend, the session proposes a claim for its
   lowercase name. Once the identity is confirmed, it proposes a claim for the
   UUID and name together. In offline mode the name claim follows the
   per-address rate limits and admission checks, so unauthenticated clients
   cannot flood the log.
2. Applying a claim succeeds only if no other live claim holds the name or
   UUID. Every peer applies the same entries in the same order, so exactly one
   of two simultaneous logins wins, wherever they connected. The loser gets
   `LoginAlreadyConnected` or `LoginDuplicateName`, as today.
3. `cluster.max_players` (optional) caps the network: a claim beyond it fails
   deterministically unless it carries a bypass decided by the login hook.
4. Dropping the session's registration proposes a release.

A claim adds one commit to each login: a round trip to the leader and from it
to a majority, typically 1–5 ms in one data centre. Logins already wait far
longer on Mojang authentication. Claims are group-committed: while one
proposal is in flight, a peer collects the next claims and sends them
together. At 20,000 players this is on the order of ten logins and a few
hundred claim operations per second, well within what Raft handles.

### When a peer disappears

- The leader expires a peer that it has not heard from for
  `cluster.peer_timeout` (default 10 s) by proposing `peer expired`, which
  releases its claims. A new leader starts every peer's timer afresh, so an
  election never expires every peer at once.
- A peer that has not heard from the leader for half of `peer_timeout`
  **fences itself**: it refuses new logins with `cluster.partition_message`.
  Because it fences after half the timeout and the leader expires after the
  full timeout, a peer has always stopped admitting players before its claims
  are released. This assumes only that clocks on the peers advance at similar
  rates, not that they agree.
- Existing players on a fenced peer stay connected for
  `cluster.partition_grace` (default 60 s), so a short blip disconnects
  nobody. If the peer is still cut off afterwards, it disconnects them with
  the partition message, so they can log in through a healthy peer, and their
  servers' counts on the majority side become true again.
- After expiring a peer, the leader holds back idle stops and scale-down for
  servers that had its players, until `partition_grace` has passed.
- A peer that rejoins proposes `peer joined` with its current incarnation and
  fresh claims for any players it still holds. A claim that now conflicts,
  because the player logged in elsewhere meanwhile, disconnects the session
  on the rejoining peer.

### Player actions on another peer

Every operation that today does `entry.actions.try_send(PlayerAction::…)` in
`src/extensions/players.rs` first looks up the player's claim:

- a local player gets the action through its channel, as today;
- a player on another peer gets it as a request on the `player` stream to
  that peer, which runs it locally and replies with the same result type
  (`sent`, `offline`, `queued`, …).

`PlayerAction` becomes serializable. Both paths sit behind the existing
`Players` methods, so these work across peers without changes of their own:
`rift.players.*` and `rift.network.*`, `GET /api/players` and the message and
kick routes, the plugin and Lua players API, BungeeCord `Message`,
`MessageRaw`, `KickPlayer`, `Connect`, `ConnectOther`, `Forward` and
`ForwardToPlayer`, and `rift admin transfer`. Broadcasts go to every peer, and
each delivers to its own players. Dialog clicks arrive on the player's peer
and are handled there, so a dialog opened from another peer routes its
callback back over the `player` stream.

Events that belong to a session (login, chat, commands, transfer decisions,
disconnect) are raised on the peer that holds the session. Every peer loads
the same plugins, so plugin code across the cluster sees each of these events
exactly once.

### Permissions for online players

The permission authority keeps per-session state in memory: who is online,
their contexts and expected arrivals (`src/permissions/authority.rs`). A
commit recompiles only players online on that process. In a cluster a
backend's bus client is connected to one peer while its players arrive
through every peer, so:

- each peer keeps session state for the players it holds and recompiles them
  when a permission entry is applied (through the apply hook);
- permission pushes for a player are published by the owning peer with
  `cluster` scope, so they reach the backend wherever its client is connected;
- context reports and expected arrivals that a backend sends are routed to the
  player's owning peer, like player actions.

## Message bus

### Subject scopes

Revision 1 forwarded every message to every peer with matching interest.
That is wrong for Rift's own subjects: every peer subscribes to
`rift.control.*`, `rift.players.*`, `rift.network.*`, the permission request
subjects and node states without a queue group
(`src/messaging_runtime.rs`, `src/permissions/control.rs`,
`src/node/registry.rs`). A request forwarded by interest would run once per
peer, and an instance would be created three times. Every subject therefore
has a **scope**:

| Scope | Subjects | Behavior |
| --- | --- | --- |
| `local` | Built-in request subjects: `rift.control.*`, `rift.players.*`, `rift.network.*`, permission requests | Answered by the peer the client is connected to; never exported. That peer forwards the work internally where needed: player actions to the owning peer, operations to the leader |
| `applied` | Events about replicated state: `rift.events.instances`, `permissions`, `reload`, `servers` | Published by every peer when it applies the entry, to its local subscribers only; never forwarded. Each subscriber gets exactly one copy |
| `origin` | Events about sessions: `rift.events.players`, `rift.events.control` | Published on the peer where they happen. Forwarded to peers whose **remote clients** have interest; in-process Lua and plugins receive only events of their own peer, so plugin code sees each event once |
| `owner` | `rift.node.<n>.cmd`, `rift.node.<n>.state` | Routed to the peer that holds the node's session claim (commands) or to the leader (states) |
| `cluster` | Everything else: user and plugin subjects | Federated by interest, as below |

Lua and plugin subscriptions accept a `scope` (`"cluster"`, the default for
user subjects, or `"local"`). The plugin host's delivery of every
`rift.events.>` message to every plugin (`src/plugins/mod.rs`) follows the
`origin` and `applied` rules.

### Federation

For `cluster` subjects, every peer runs its own `Broker`
(`src/messaging/mod.rs`) and federates in a full mesh:

- **Interest.** Each peer tells every other peer which subject patterns its
  local subscribers use, and how many members each queue group has.
  Interest is sent incrementally with a sequence number, and in full after a
  reconnect. It is not stored in Raft.
- **Publish.** A message is delivered to local subscribers and forwarded once
  to each peer with matching interest. A message received from a peer is
  never forwarded again: one hop, no loops.
- **Queue groups** pick one member across the cluster: a local member if
  there is one, otherwise a peer chosen by its member count.
- **Request and reply.** Inboxes become `_INBOX.<peer>.<random>` (today
  `_INBOX.<random>`), so a reply is routed by its prefix without waiting for
  the inbox's interest to spread.
- **No responders.** Today `NoResponders` comes from a synchronous count of
  local deliveries, and node requests report it as "unreachable"
  (`src/node/registry.rs`). With federation, a request with no local
  subscribers and no remote interest fails with `NoResponders` at once. A
  request forwarded to a peer whose subscribers left meanwhile gets a
  no-responders reply from that peer. Node requests use the `owner` scope and
  the session claim instead, so "unreachable" stays exact.
- **Identity.** Clients authenticate on the peer they connect to. Forwarded
  messages carry the publishing principal, so permission checks that depend
  on the publisher give the same answer on every peer.

Bus clients receive a list of peers. `crates/rift-messaging`'s
`Client::connect` gains an address list and reconnects to the next peer when
one fails; the Java client, the node agent (`--controller`, repeatable) and
the bus-login files written to servers get the same list. The certificate
must name every peer address, or each address carries its own TLS name.

### Bus logins

The key that derives bus-login tokens is a generated file today,
`local/instance-key` (`src/instance_login.rs`), so a token derived on one
peer would fail on another. `rift cluster init` moves the key into a
replicated table, and every peer derives and checks tokens from it. Login
generations are already in the database.

### Retained streams

Retained streams (`src/messaging/stream.rs`) keep consumer state that `fetch`
changes: pending records and redelivery deadlines. Replicating appends and
acknowledgements alone would let two peers hand the same records to one
durable consumer. So:

- the **stream service runs on the leader**. `fetch`, `publish` and
  acknowledgements on any peer are forwarded to it;
- appends and acknowledgements are `stream` entries, so a new leader rebuilds
  every stream and its acknowledged positions;
- pending records that were delivered but not acknowledged when the leader
  changed are delivered again. Streams already promise at-least-once
  delivery, so this is within their contract;
- streams without storage (memory streams) live on the leader and start empty
  after a leader change.

The permission change stream, which today lives in `rift.perms-changes/`
beside the configuration, is derived from permission revisions in the
replicated database and is identical on every peer.

### Node sessions

A node principal has one session at a time today: a new login closes the old
one. In a cluster, node sessions are claims: a node agent connecting to any
peer claims `node.<name>`, and the peer that held the older session closes
it. The claim also tells the leader which peer to route the node's commands
through.

## Work that runs once: the leader

Some work must happen exactly once in the network. It runs on the current
Raft leader and nowhere else:

- the scaler (`src/scaling.rs`), instance creation, removal and moves
  (including resumed copies and moves of named servers);
- scheduled backups of persistent instances (`src/backups.rs`) and restores;
- managed server and instance lifecycle: starting, stopping, idle stops,
  restart retries and recovery;
- node control commands and node schedules (cordon, drain);
- startup recovery of half-done operations (today `Store::restore` aborts
  `creating` records and finishes `removing` ones at every start; in a
  cluster only the leader does this, when it gains leadership, and continues
  or aborts the create, remove or move that the previous leader left);
- writes derived from configuration: `mirror_config`, pruning removed groups
  and nodes, and the permission expiry sweeper;
- the stream service, the queue service, template source syncs (git) and
  cache warming;
- plugin work marked as leader-only (see [decisions](#decisions)).

### Leader exclusivity

1. **Quorum lease.** The leader acts only while a majority of voters has
   acknowledged it within the lease period (shorter than the election
   timeout). Every side effect checks the lease first. A leader cut off from
   the majority stops acting within one lease period, before another leader
   can be elected.
2. **Term-tagged operations.** Operations proposed by leader tasks carry the
   term they were decided in. The leader executing operations refuses one
   with another term, so a deposed leader's scaler cannot get its decisions
   applied by forwarding them to the new leader like an ordinary write.
3. **Fenced node agents.** When it gains leadership, the leader first sends
   every node agent a `fence` command with its term. The agent stores the
   highest term it has seen below its `--root` and refuses control commands
   with a lower one. The leader sends an agent other commands only after the
   agent acknowledged its fence. Inspection commands (`logs`, `console`,
   `files`, `tree`, `read`) are not fenced: any peer's web API may send them
   for any operator.
4. **Reconcile on leadership gain.** A new leader already has node states
   from its time as a follower, so the reconnect path that triggers adoption
   today (`src/node/registry.rs`) would not fire. The leader therefore
   reconciles every agent after fencing it, exactly as a restarted controller
   adopts servers today.

### Inputs from other peers

The leader decides from replicated state where it can: player counts per
server come from presence, instance records and node schedules from the
database. Reservations (players being sent to a server that is starting) and
queue tickets change too fast to replicate:

- **Start on connect.** A session that needs a stopped server asks the leader
  and waits until the server is running, with the same timeout as today.
- **Idle stops.** Today one lock guards both reservations and idle detection
  (`src/managed.rs`), so a player can never connect to a server that is being
  stopped. In a cluster, before stopping a server for being idle or for
  scale-down, the leader asks every live peer over the `load` stream. A peer
  stops routing new players to that server and answers with a veto if it has
  a reservation or a player on its way there. The leader stops the server
  only if no peer vetoes and no peer is unreachable.
- **Server states** (`running`, `starting`, `unknown`, the last error) are
  written by the leader as `operation` entries, so every peer routes from the
  same view and a new leader starts with the old leader's view.
- **Service states** that backends report on `rift.server.state` (`LOBBY`,
  `INGAME`, drain flags; `src/service_state.rs`) arrive on whichever peer
  the backend's client uses. That peer broadcasts them to every peer, and a
  peer that starts asks the others for a full copy, so it never routes players
  into an `INGAME` or draining server.

### Servers on the peers' own hosts

Today `local` means the proxy's own host. In a cluster each peer runs an
embedded node agent and is a node named after itself, with labels from its
`cluster.toml`. Managed servers and service groups select peers like any
node. A server on a peer's host keeps running when that peer's proxy process
restarts if it uses [`keep_running`](kept-servers.md); otherwise it stops with
its peer, as local servers stop today. The name `local` is refused in a
cluster. `rift cluster init` converts existing local registrations (stored
with an empty node and absolute plan directories) to the initializing peer's
node.

## Configuration and files

### One configuration

Every peer runs the same `rift.lua`. The peer's own name is visible to the
script as `rift.peer`, so per-host differences stay in the one file:

```lua
rift.config.cluster = {
    peer_timeout_ms = 10000,
    partition_grace_ms = 60000,
    max_players = 20000,
}

rift.config.listeners.public = "0.0.0.0:25565"
if rift.peer == "us-1" then
    rift.config.routes.public = "lobby-us"
end
```

Branching on `rift.peer` is allowed only in **peer sections**: listeners,
routes, limits, health checks, the server list, the tab list and the web
listener. Every other section is **shared**: it writes durable state or
drives leader tasks (service groups, managed servers, backends, nodes,
permissions, extensions, messaging principals). Each peer hashes the shared
sections of its evaluated configuration and sends the hash to the leader. The
leader refuses to activate a deployment while any peer's hash differs, and the
dashboard shows which peer and section differ.

Configuration **deployments** (the dashboard, `PUT /api/config`, `rift admin
reload` after an edit) become `config` entries. Every peer writes the
deployed source to its `rift.lua` atomically and reloads. The deployment
history (`deployments.json`) and the operator audit trail (`audit.jsonl`,
both in `src/operator.rs`) become replicated tables, so rollback and audit
work from any peer. A peer that starts with a `rift.lua` that differs from the
last deployment adopts the deployment if its file matches an older deployed
revision. If it matches none, which means someone edited it by hand, the peer
refuses to start and keeps the file; `--adopt-deployed` overrides.

The web API has no sessions: tokens come from the configuration, which is
replicated, so a token works on every peer. The random token for local
access that each process creates (`src/web.rs`) stays per peer.

### Templates and other files

Template folders, plugin jars, maps and the software cache can be large, so
they never travel in the log:

- A `manifest` entry records a folder's files by path, size and SHA-256.
- Each peer stores contents by hash under `cache/blobs/` and fetches missing
  blobs from any peer that has them, over the bulk connection.
- An upload or a git sync stores the blobs, copies them to a majority of
  voters, then proposes the manifest, so a committed manifest never references
  a blob only one host has. Git syncs run on the leader, so every voter needs
  the git credentials.
- `template_storage` on each peer is materialized from the manifest, so node
  agents keep fetching templates over `--web` from whichever peer they reach.

Server directories on hosts (nodes and peers) stay where they are: they are
the data of running servers, not shared configuration.

### Key-value stores and other local files

- A key-value store in the instance database (`extensions.storage.database =
  true`) is replicated with `kv` entries.
- A store kept in its own file (`path`) cannot be replicated as a file. In a
  cluster it is a configuration error; `rift cluster init` copies its entries
  into the instance database.
- Writes from plugin hooks wait at most `STORE_WAIT`
  (`src/plugins/services.rs`). An election (1.5–3 s) can exceed it, so hook
  writes during an election fail with a timeout, as they would if the disk
  stalled today. The documentation will say so.

### Secrets

Secrets stay in each peer's environment: the cluster token, node tokens, the
forwarding secret and the web token. They must be equal on all peers; the
handshake checks this without revealing them. The bus-login key moves into the
replicated state (see [bus logins](#bus-logins)).

## What stays per peer

Some state stays deliberately per peer, and the documentation will say so:

| State | Why |
| --- | --- |
| Connection rate limits, per-address counts, `limits.max_connections` | They protect one process; a network-wide cap is `cluster.max_players` |
| Backend health checks | Each peer measures its own path to the backends and routes by it |
| Metrics | Per process; aggregate them in Prometheus |
| Plugin and Lua in-memory state, `rift.every` timers | Each peer runs its own plugin instances; see [decisions](#decisions) for leader-only work |
| Managed server logs and consoles | Served by the node agent that runs the server, from any peer |

## Failure behavior

| Situation | What happens |
| --- | --- |
| A follower or learner dies | Its players are disconnected. If it restarts, its new incarnation releases their claims at once; otherwise the leader expires it after `peer_timeout`. Everything else continues. |
| The leader dies | The voters elect a new leader within the election timeout (default 1.5–3 s). Meanwhile no entry commits: new logins and writes wait, players already on backends notice nothing. The new leader fences and reconciles every node agent, resumes half-done operations and scaling. |
| A minority of peers is partitioned | The majority side continues. The minority side fences itself after half of `peer_timeout`, keeps its players for `partition_grace`, then disconnects them. |
| No majority anywhere | No commits: no logins, no configuration or permission changes, no scaling. Connected players keep playing for `partition_grace`. Node agents keep their servers. |
| A peer falls behind | It catches up from the log, or installs a snapshot if the log was purged. It refuses logins until it is within 1 s of the leader, so its tab list and counts are not stale; players already connected keep playing. |
| A replica diverges | Detected by a `checksum` entry or a changeset conflict. The peer stops taking logins, reinstalls a snapshot and reports `rift_cluster_diverged`. |
| A disk fills up | Writing the Raft log fails, so the peer stops acknowledging entries; Raft treats it as unavailable. |

### Rolling restarts

`rift admin cluster leave <peer>` (and SIGTERM in a cluster) drains a peer
gracefully:

1. it stops taking new logins and transfers leadership if it has it;
2. it sends 1.20.5+ clients to another peer with the Transfer packet, using a
   healthy peer's public address from its `cluster.toml`, and gives older
   clients a reconnect message;
3. it releases its claims and closes its bus clients, which reconnect to
   other peers.

Upgrades run one peer at a time. Entry kinds and schema migrations are
enabled only after a committed `features` entry says every member supports
them, so a mixed-version cluster works with the features of its oldest
member. A peer older than the oldest version a committed `features` entry
requires is refused when it joins. After a `migrate` entry has committed,
downgrading means restoring a backup, as for a single Rift today.

### Backup and restore

- `rift admin cluster backup <file>` writes a consistent copy of the state
  machine from any peer, through a linearizable read.
- `rift cluster init --from-backup <file>` starts a new one-voter cluster
  from a backup; add the other peers as on a first setup.
- `rift cluster recover --peer <name>` forces a one-voter cluster from the
  data of one surviving peer when the others are gone for good. It can lose
  writes that only the lost peers acknowledged, so it requires
  `--i-understand-data-loss`.

## Getting there from one Rift

1. Write `cluster.toml` on the existing host and run `rift cluster init`. It
   creates the Raft log and a one-voter cluster whose state is the current
   instance database, moves `deployments.json`, `audit.jsonl`, the bus-login
   key, file-backed key-value stores and the template folders into the
   replicated store, and converts `local` registrations to this peer's node.
   A PostgreSQL or MariaDB installation first runs `rift database copy --to
   rift.sqlite3`.
2. Start the other peers with their `cluster.toml`, add them with `rift admin
   cluster add`, and promote the voters once they have caught up.
3. Put the peers behind DNS or a load balancer. Give bus clients and node
   agents the peer list.

`rift check --strict` against a cluster reports peers that are unreachable,
behind, on another version, holding different secrets, or evaluating the
shared configuration differently.

## Operating a cluster

| Where | What |
| --- | --- |
| CLI | `rift admin cluster` (members, roles, leader, term, applied index, lag), `add`, `promote`, `demote`, `remove`, `replace`, `leave`, `transfer-leader`, `backup` |
| HTTP | `GET /api/cluster`, the same data; every server and player in the API gains a `peer` field |
| Dashboard | A Cluster panel with each peer's role, lag, players, load and configuration hash |
| Metrics | `rift_cluster_term`, `rift_cluster_is_leader`, `rift_cluster_commit_index`, `rift_cluster_applied_index`, per-peer `rift_cluster_replication_lag`, `rift_cluster_proposal_seconds`, `rift_cluster_operations_total`, `rift_cluster_bus_forwarded_total`, `rift_cluster_diverged` |
| Control subject | `rift.control.cluster` and `rift.events.cluster` (leader changes, membership, peer expired) |

## Code changes

The seam is `Snapshot::build` in `src/runtime.rs`, which today constructs the
broker, extensions, player registry, nodes and managed servers together.

| Area | Change |
| --- | --- |
| `src/cluster/` (new) | Peer links, openraft network and storage, the state machine, apply hooks, membership commands, leader tasks and leases, the blob store |
| `src/database.rs` | WAL and read connections for the replicated backend; operation execution with session recording; changeset apply through FFI; primary key and schema assertions |
| Every write site | A typed operation per write, executed through one entry point that runs locally on a single Rift and on the leader in a cluster |
| `src/players.rs`, `src/extensions/players.rs`, `src/events.rs` | Claims, broadcast attributes, routed `PlayerAction`s, connection ids with peer and incarnation |
| `src/permissions/authority.rs` | Apply hooks; per-peer sessions; pushes with `cluster` scope; routed context reports |
| `src/messaging/` | Subject scopes; interest exchange and forwarding; peer-prefixed inboxes; principals on forwarded messages |
| `crates/rift-messaging`, Java client | Address lists and failover |
| `src/managed.rs`, `src/scaling.rs`, `src/node/` | Leader-only start; fencing; reconcile on leadership gain; stop vetoes; peers as embedded nodes |
| `src/instance_store.rs` | Recovery as a leader task; apply hooks for `Config.instances` and `Config.moved_servers` |
| `src/control.rs`, `src/operator.rs` | Deployments, history and audit as replicated entries; shared-section hashes |
| `src/extensions/storage.rs`, `src/directory.rs`, `src/messaging/stream.rs`, `src/instance_login.rs` | Their own entry kinds; the stream service on the leader; the replicated bus-login key |
| `src/config.rs`, `src/check.rs` | `cluster` settings, `cluster.toml`, `rift.peer`, peer and shared sections, strict checks against a running cluster |

The rusqlite `session` feature needs bindgen and therefore libclang at build
time. It sits behind a `cluster` cargo feature that release builds enable, so
other builds keep their current requirements.

## Plan

This branch merges once everything below is done. Each step lands as its own
commits with tests, and the single-process behavior stays unchanged
throughout.

1. **Design** (this document).
2. **Cluster core and its test harness.** Peer links, openraft with SQLite log
   storage, membership commands, learners, witness, incarnations, leases,
   `rift admin cluster`, `/api/cluster`, metrics. A deterministic simulation
   (simulated network, clock and disk) that later steps extend, and a
   fault-injecting peer transport for partitions.
3. **Replicated database.** Typed operations at every write site, leader
   execution with group commit, changeset apply, apply hooks, `directory`,
   `kv`, `migrate`, `checksum`, snapshots and install while serving,
   idempotent proposals, `rift cluster init`.
4. **Presence.** Claims, broadcast attributes, cluster-wide duplicate-login
   checks, `cluster.max_players`, expiry, fencing and partition grace,
   connection ids, network-wide counts.
5. **Bus.** Subject scopes, federation, prefixed inboxes, node session claims,
   the replicated bus-login key, the stream service on the leader,
   multi-address clients.
6. **Routed player actions and permissions.** `PlayerAction` over the
   `player` stream with every API, subject and BungeeCord channel on top;
   permission sessions per peer.
7. **Leader tasks.** Scaling, managed servers, nodes, recovery and
   configuration-derived writes on the leader, with leases, term-tagged
   operations, agent fencing, reconcile on leadership gain, stop vetoes and
   peers as embedded nodes.
8. **Configuration and files.** Replicated deployments, history and audit;
   peer and shared sections; template manifests and the blob store.
9. **Queues.** [Server queues](queues.md) become network-wide: the queue
   service runs on the leader; each peer keeps its own players' tickets and
   submits them again to a new leader, which orders them by their
   leader-stamped time.
10. **Hardening.** Rolling restarts with the Transfer packet, backup and
    recovery commands, `rift check --strict` for clusters, and a three-peer
    cluster in CI whose leader is killed with SIGKILL, asserting no duplicate
    logins, no lost acknowledged writes, no doubly executed leader work.

### Step 2 notes

What the cluster core does differently from the text above, and why:

- **Encoding.** Log entries, Raft messages and handshakes are JSON: a newer
  peer reads an older peer's entries, and an unknown kind fails loudly. Each
  entry kind carries its own version `v`.
- **The state machine's file.** Until step 3 replicates the instance
  database, the state machine is its own file, `rift.cluster.sqlite3` beside
  `rift.lua`: the instance database is locked by its single owner. The state
  machine takes its connections from a `Database` trait, so step 3 hands in
  the instance database without other changes.
- **Log purging.** openraft purges only entries a snapshot covers. A peer
  therefore builds a snapshot once its log passes 64 MiB (or when a member
  needs one) and openraft purges behind it, keeping 10,000 entries for
  followers that lag a little. "Purge once every member replicated" is not a
  separate rule.
- **Building a snapshot holds apply** for the length of the copy, so the copy
  matches the log id it is labelled with. Step 3, which adds read
  connections, can copy from one of them instead.
- **Checksums take two entries.** `checksum mark` makes every peer hash its
  replica at the same index and keep the hash locally; the leader then
  proposes `checksum verify` with its own hash for that index. The result of
  both is empty, since results are stored for retries and must not differ
  between replicas.
- **Repair after a divergence.** A diverged peer asks the leader for a copy
  taken at or after its own applied index and swaps it in like a snapshot
  install, below openraft: entries the copy already holds are skipped when
  they are applied again.
- **Features only grow.** A `features` entry adds the kinds every member
  supports (a member's last `peer joined`, or its handshake before it ever
  joined; a member never heard of does not hold activation back). A peer
  that cannot apply an enabled kind is refused when it links.
- **The lease** is three quarters of `election_timeout_max` after the last
  quorum acknowledgement (openraft refuses votes for a full
  `election_timeout_max` after a follower last heard from the leader), and
  ends at once when the leader starts a transfer. openraft does not step a
  leader down when its lease runs out, so leader tasks check
  `LeaderContext::has_lease` themselves.
- **The shared-configuration hash** in handshakes is the SHA-256 of the
  `rift.lua` source text until step 8 hashes the shared sections.
- **`replace`** removes the old Raft id and allocates a new one at once; if
  the peer is not running yet, `rift admin cluster add <name> <address>`
  finishes the job once it is (it reuses the allocated id).
- **`leave`** moves leadership away; stopping logins and transferring
  players come with steps 4 and 10.
- **Peer certificates** are used both ways, so each must name the host of
  its `advertise` address and allow server and client authentication.
- `rift.control.cluster` and `rift.events.cluster` come with the bus (step 5).

## Decisions

Each question was decided on 2026-11-19. The chosen option comes first and is
marked **(chosen)**; the others are kept with their trade-offs as the record
of what was weighed. Reopening a decision means changing this section and
every part of the document that depends on it.

### 1. How writes reach the database

| Option | For | Against |
| --- | --- | --- |
| **Leader executes typed operations, replicates changesets** (chosen) | Serializable by construction; no read sets, retries or conflict detection; a closed set of operations is easy to test | Every write site becomes a typed operation; follower writes cost a round trip to the leader |
| Optimistic changesets from any peer (revision 1) | Writes run on the peer that makes them; closures stay as they are | Not serializable as specified: cached statements skip the authorizer, unique constraints are invisible, a new leader has no change index; retries starve under load |
| Replicate SQL statements (rqlite style) | Small entries; no session extension | Rust logic between statements cannot be replicated; every statement must be deterministic |

### 2. What player presence puts in the log

| Option | For | Against |
| --- | --- | --- |
| **Claims in Raft, attributes broadcast by the owning peer** (chosen) | Uniqueness is linearizable; skins and server moves stay out of the log and snapshots | Two mechanisms; attributes can lag claims by milliseconds |
| Everything in Raft | One mechanism, one ordering | 1–3 KB per login and an entry per server switch, bigger snapshots, slower tab lists |
| Nothing in Raft (gossip only, RedisBungee style) | No commit on login | Duplicate logins across peers become possible again |

### 3. Logins on a partitioned minority

| Option | For | Against |
| --- | --- | --- |
| **Refuse** (chosen) | No duplicates, no stale bans, caps hold | A peer cut off from the majority admits nobody |
| Allow local-only logins, reconcile on rejoin | Players can still join a cut-off peer | Duplicate sessions, banned players admitted, `max_players` exceeded; the reconciliation kicks someone |

### 4. Existing players on a cut-off peer

| Option | For | Against |
| --- | --- | --- |
| **Keep for `partition_grace` (60 s), then disconnect** (chosen) | Short blips disconnect nobody; long partitions end with true counts and players able to rejoin elsewhere | A partition longer than the grace disconnects players whose backends may still work |
| Keep indefinitely | Nobody is disconnected by a partition | The majority's counts understate, so idle stops and caps misbehave; the players cannot be messaged or moved from the majority side |
| Disconnect when fenced | Simplest; counts are true immediately | Every short network blip disconnects a whole proxy's players |

### 5. PostgreSQL or MariaDB in clusters

| Option | For | Against |
| --- | --- | --- |
| **Never: `cluster` with `backend = "postgres"` or `"mariadb"` is an error; keep standby mode** (chosen) | One replicated backend to test; no external single point of failure | Networks with highly available PostgreSQL or MariaDB cannot reuse it for a cluster |
| Later, as an alternative state store | Uses an existing HA database; no Raft for durable state | A third mode of every write path; the database server becomes the availability limit again |

### 6. A peer whose `rift.lua` differs from the deployed revision

| Option | For | Against |
| --- | --- | --- |
| **Adopt if it matches an older deployed revision, otherwise refuse to start** (chosen) | Stale hosts update themselves; hand edits are never silently lost | A hand-edited host needs an explicit `--adopt-deployed` |
| Always adopt the deployed revision | No operator action ever | Hand edits on one host vanish without notice |
| Always refuse to start | Nothing is ever overwritten | Every stale host needs manual work |

### 7. Where membership comes from

| Option | For | Against |
| --- | --- | --- |
| **Explicit `rift admin cluster` commands, bootstrap settings in `cluster.toml`** (chosen) | No circular dependency; a configuration typo cannot remove voters; each change is deliberate | A second place to configure; membership is not visible in `rift.lua` |
| `cluster.peers` in the replicated `rift.lua` | One file describes everything | A peer needs the file to join the cluster that holds the file; a typo can drop voters |

### 8. Leader-only work in plugins

| Option | For | Against |
| --- | --- | --- |
| **Named leases with a fencing token, plus `scope` on `rift.every` (default `cluster`, meaning one peer)** (chosen) | Safe under leader changes; existing broadcast timers do not suddenly run once per peer | A new API to learn; `scope = "peer"` must be chosen for per-peer jobs |
| `rift.cluster.is_leader()` only | Tiny API | Racy: a deposed leader still sees `true` until it notices |
| Run all plugins only on the leader | No plugin changes | Session events happen on every peer, so plugins must run everywhere |

### 9. Plugins that are not cluster-aware

| Option | For | Against |
| --- | --- | --- |
| **Load every plugin; flag ones without a `cluster` declaration in `plugin.toml`** (chosen) | Existing plugins keep working; operators see which ones to check | A plugin that keeps network-wide state in memory misbehaves until fixed |
| Refuse plugins without the declaration | No silent misbehavior | Every existing plugin breaks on the first cluster start |

### 10. Learners and writes

| Option | For | Against |
| --- | --- | --- |
| **Learners send operations to the leader** (chosen) | Learners can be full proxies; voters stay few | Writes from distant learners pay their distance to the leader |
| Read-only learners | Simpler | A learner cannot admit players, since logins write claims |

### 11. Default scope of user subjects

| Option | For | Against |
| --- | --- | --- |
| **`cluster`** (chosen) | Plugins and backends talk across the whole network, as they expect from one bus | Chatty subjects cross peer links; they need `scope = "local"` |
| `local`, opt in to `cluster` | No cross-peer traffic unless asked | Existing plugins stop seeing each other across peers |

### 12. Witnesses and controller-only voters

| Option | For | Against |
| --- | --- | --- |
| **Witnesses are full replicas that may lead but hand leadership to a player-serving voter** (chosen) | Simple, uses Raft as is; works on any host that can reach the nodes | The witness holds all state and the bus-login key |
| Log-only witnesses that never lead | Holds no state | Needs custom voting rules on top of openraft |
| Dedicated controller voters with no game listener | Proxies and control work separated; proxies scale as learners | More hosts to run for small networks |
