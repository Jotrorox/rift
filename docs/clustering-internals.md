# How clustering works

For contributors: how [clustering](clustering.md) is built, why it is built
that way, and where the implementation differs from the first design. The
[user guide](clustering.md) says how to set up and run a cluster.

This page started as the design that the clustering branch was built from.
The design sections describe the shape of the system and the reasons for it.
Each plan step then recorded what it actually implemented under
[implementation notes](#implementation-notes). **Where a note and a design
section disagree, the note describes the code.** The choices the design rests
on, with the options that were weighed, are under [decisions](#decisions).

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

## Why clustering exists

Before clustering, exactly one Rift process accepted players. That process
also held the message broker, the player registry, the node controller and
the instance database. A [standby](database.md#standby-controllers) could
take over when it stopped, but a standby is a cold spare: players on the
active proxy are disconnected, and the network can never take more players
than one process can.

Velocity networks get several proxies from RedisBungee, which keeps a shared
player list in Redis. A shared list alone is not enough for Rift, because Rift
also owns the database, the bus, scaling and the nodes. Clustering makes every
Rift process an equal **peer**. The peers replicate their durable state with
the Raft consensus algorithm, share one message bus and one player list, and
elect one of themselves to run the work that must happen exactly once.

### Goals

1. **Many active proxies.** Any number of peers accept players at the same
   time. Players see one network: one player list, one tab list,
   network-wide counts, `/server`, messages, kicks and transfers between
   players on different peers.
2. **No single point of failure.** Losing any minority of voting peers keeps
   the network running: logins continue, scaling continues, and nodes keep
   their servers and are adopted by the new leader. Only the players
   connected to a peer that dies are disconnected. They can log in again
   through another peer as soon as the dead peer restarts or is expired, at
   most `cluster.peer_timeout` (default 10 s) later.
3. **One source of truth, replicated.** The instance database, the player
   directory, the permission store, the extension key-value store,
   configuration deployments and the operator audit trail are replicated by
   Raft to every peer. No external database, Redis or shared file system is
   needed.
4. **Same features, same configuration.** Plugins, Lua, the dashboard, the
   HTTP API, `rift admin` and bus clients work against any peer, with the
   same semantics as on a single Rift, except where the guide says
   otherwise. A single Rift remains a valid deployment with no cluster
   settings and no extra cost.

### Non-goals

- Moving a live player connection between peers. A TCP connection ends on
  one peer. A peer that shuts down cleanly sends 1.20.5+ clients to another
  peer with the Transfer packet, but a peer that crashes drops its players.
- Clusters spread across continents. Raft commits need a round trip to a
  majority, so voters should be within a few tens of milliseconds of each
  other. Distant proxies join as learners.
- Byzantine peers. Peers trust each other completely, as the proxy trusts
  its node agents.

### Overview

| Concern | One process | In a cluster |
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

Rift uses [openraft](https://github.com/databendlabs/openraft) 0.10. It is
async, runs on tokio (which Rift already uses), supports joint-consensus
membership changes, learners, snapshots and linearizable reads, and leaves
storage and transport to the application. `raft-rs` (TiKV) was considered: it
is well proven, but synchronous, and needs more scaffolding for the parts
openraft provides. The openraft behaviors Rift relies on (leader step-down,
leases) were verified in openraft's source; where openraft falls short, Rift
wraps it (see the lease under [step 2 notes](#step-2-notes)).

Each peer keeps two SQLite files beside `rift.lua`:

| File | Contents | Durability |
| --- | --- | --- |
| `rift.raft.sqlite3` | The Raft log, the vote, the cluster id, this peer's Raft id and the last purged log id | WAL, `synchronous=FULL`: a vote or entry acknowledged to the leader must survive a crash |
| The instance database (`rift.sqlite3` by default) | The state machine: every table a single Rift holds, plus `cluster_applied` and the other `cluster_*` tables | WAL, `synchronous=NORMAL`: entries lost in a crash are re-applied from the log |

The applied log id is written in the same SQLite transaction as the entry it
records, so a crash never applies an entry twice or skips one. The last
snapshot lives in `rift.raft.snapshots/`.

A single Rift runs the instance database with one connection behind one
mutex, in `journal_mode=DELETE` with `synchronous=FULL` (`src/database.rs`).
In a cluster, applying entries would then block every read on the login path.
The replicated mode therefore uses WAL with one writer connection, used only
by apply and by the leader's execution of operations, and a small pool of
read connections.

Changesets come from SQLite's
[session extension](https://sqlite.org/sessionintro.html). rusqlite's
`session` feature needs bindgen and libclang at build time, so Rift declares
the few session functions itself (`src/database/changeset.rs`) and compiles
the bundled SQLite with `SQLITE_ENABLE_SESSION` and
`SQLITE_ENABLE_PREUPDATE_HOOK` (`LIBSQLITE3_FLAGS` in `.cargo/config.toml`).
There is no `cluster` cargo feature: every build includes clustering.

### Peer links

Peers talk over QUIC on a dedicated `listen` port with the ALPN
`rift-cluster/1` and TLS 1.3 (`src/cluster/quic.rs`). Both sides verify the
other's certificate against the cluster CA, prove that they hold the cluster
token (an HMAC bound to TLS keying material exported from the connection,
RFC 5705), and present the cluster id; a peer with another cluster id is
refused. Each pair keeps two connections, so bulk transfers never share a
congestion window with consensus traffic:

| Connection | Stream kinds | Purpose |
| --- | --- | --- |
| control | `raft` | AppendEntries, Vote, heartbeats, forwarded admin commands |
| control | `presence` | Player attributes, server moves and leader probes (see [presence](#presence)) |
| control | `player` | Actions for players connected to the receiving peer |
| control | `bus` | Interest and forwarded messages (see [federation](#federation)) |
| control | `load` | Reservations, stop vetoes, forwarded leader-only requests and leave requests (see [inputs from other peers](#inputs-from-other-peers)) |
| control | `queue` | Queue tickets for the leader's queue service and its turns (see [step 9 notes](#step-9-notes)) |
| bulk | `snapshot`, `blob` | Snapshots, template and file contents by SHA-256 |

**Liveness comes from peer links, not from log entries.** Peers do not write
heartbeats into the log; the leader tracks when it last heard from each peer
on the control connection. This keeps the log small (see
[snapshots](#snapshots-and-log-purging)).

During the handshake each peer also sends its version, the entry kinds it
supports, the hash of the [shared configuration](#one-configuration), and an
HMAC of each shared secret keyed by the cluster token: the forwarding secret
and the node tokens, as far as its environment holds them. A peer whose
secrets differ is refused, and its log names which secret differs.

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
A witness hands leadership to a player-serving voter whenever one is
available, since the leader talks to every node agent.

### Membership

Membership is changed by explicit commands (`rift admin cluster add`,
`promote`, `demote`, `remove`, `replace`), not by editing `rift.lua`, so a
typo in a deployment cannot remove two voters (decision 7).

- Each command is one membership change. The leader refuses a change that
  would leave fewer than three voters (a witness counts as one) unless it is
  given `--force`.
- A peer's Raft id is assigned when it is added and stored in its Raft file.
  A peer that comes back with an empty Raft file under a name that is still a
  member is refused, because it could vote twice in one term. `replace`
  removes and re-adds it under a new id.
- Raft ids are never reused, and a cluster assigns at most 255 of them:
  connection ids carry the peer's id in 8 bits (see
  [incarnations](#incarnations-and-connection-ids)). A peer whose id does not
  fit admits no players.
- A peer's own settings live in `cluster.toml` beside `rift.lua`. They are
  needed before the peer can reach the cluster that holds the replicated
  configuration, so they cannot come from it.

### The state machine

Every log entry is one of a small number of kinds, each versioned:

| Entry | Applied as |
| --- | --- |
| `operation` | A changeset produced by the leader executing typed operations, with each operation's result (see [writes](#writes-to-the-instance-database)) |
| `claim` | Player name and UUID claims and releases |
| `kv` | Key-value mutations, applied with their quota checks, returning a result |
| `directory` | Player directory upserts (name, last seen, language) |
| `stream` | Appends to retained streams and consumer acknowledgements |
| `peer` | Peer added, joined (with a new incarnation), expired, removed |
| `config` | A configuration deployment: revision, source and actor |
| `manifest` | A change to a template folder: paths, sizes, SHA-256 |
| `features` | The set of entry kinds and settings every member supports |
| `migrate` | Upgrade one component's schema to a version every member supports |
| `checksum` | A hash of the state machine at this index, compared by every peer |
| `bus` | Node session claims and releases, the bus-login key, the peers' bus addresses (see [step 5 notes](#step-5-notes)) |
| `lease` | Named plugin leases: grants, renewals and releases with fencing tokens (see [step 9 notes](#step-9-notes)) |

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
  entry lists it (see [step 2 notes](#step-2-notes) for how the set is
  computed).
- **Schema changes only through `migrate`.** DDL, `PRAGMA user_version` and
  startup inserts (such as the permission store's initial rows) are not
  captured by changesets. The migrations that run when a component opens on
  a single Rift (`src/instance_store.rs`, `src/permissions/authority.rs`) run
  instead when a `migrate` entry is applied, on every peer at the same index.
- **Divergence detection.** The leader starts a checksum every 10,000 entries
  (a hash over every table, ordered by primary key). A peer whose hash
  differs stops taking logins, reports `rift_cluster_diverged` and repairs
  itself from the leader.

### Snapshots and log purging

The state machine is durable on its own, so a peer can drop log entries a
snapshot covers; a member that falls behind installs a snapshot. A snapshot
file is built with SQLite's online backup API and sent over the bulk
connection in chunks.

The instance database can reach gigabytes: the key-value quota allows up to
1 GiB per owner (`src/extensions/storage.rs`), streams default to 64 MiB each,
and the player directory grows without bound. Snapshots are therefore rare
and resumable: the receiver appends a chunk only at the offset it holds and
answers with the offset it expects next, so a transfer cut by a lost
connection continues where it stopped (`src/cluster/snapshot.rs`). A peer
that is several snapshots behind is better replaced from a backup.

**Installing a snapshot while serving players:**

1. Pause apply and leader execution on this peer; stop taking logins.
2. Write the snapshot, check its integrity and schema versions.
3. Reopen the writer and read connections, which also drops their prepared
   statement caches.
4. Run every apply hook in *full reload* mode (see below), synthesizing the
   events that the difference between the old and new state implies.
5. Resume apply and logins.

### Apply hooks

On a single Rift several components load the database into memory once and
then trust their memory:

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
`src/instance_store.rs`). In a cluster, **the leader executes every write**
(decision 1):

1. Each write is a typed, serializable **operation**: a permission `Change`
   (`src/permissions/authority.rs`), `BeginCreation`, `AllocateName`,
   `SetGroupState`, `SetNodeSchedule`, `SetServerState` and so on. The
   closed set of call sites makes this feasible.
2. A peer sends the operation to the leader. The leader runs the existing
   code for it on its own replica, inside a transaction that SQLite's session
   extension records. The leader runs queued operations one after another in
   one open transaction, each in its own savepoint, so a failing operation
   rolls back alone and later operations see earlier ones.
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
  `perm_subject` (`src/permissions/store.rs`). This is a direct FFI call to
  `sqlite3changeset_apply_v2`.
- Any conflict aborts. Since the changeset was computed against the same
  state at the same index, a conflict means divergence and leads to a repair.
- At startup the replicated backend asserts that every table has a primary
  key (sessions do not record tables without one) and that the schema
  version of every component matches the last applied `migrate` entry. A
  changeset is never applied against a table SQLite would silently skip.

**Reads** stay local and never wait for the network. They see the replica as
of its last applied entry, which on a healthy cluster trails the leader by
milliseconds. A caller that needs the latest state uses a linearizable read
(`Cluster::read_barrier`: the leader confirms its leadership, then the peer
waits until it has applied that index). Ban checks on login use local reads;
a ban is enforced on every peer within the replication lag, and the session
that a ban targets is kicked by its owning peer when the ban applies.

A cluster uses the replicated SQLite backend only. `database.backend =
"postgres"` or `"mariadb"` together with `cluster.toml` or a `cluster`
section is a configuration error (decision 5).

## Players

### Presence

Presence is split by what needs consensus (decision 2):

| Part | Fields | How it travels |
| --- | --- | --- |
| **Claims** | lowercase name, UUID, peer, incarnation, connection id | `claim` entries in the Raft log |
| **Attributes** | server, display name, signed skin properties, tab order, visibility filter, IP address, protocol, locale, since | Broadcast on the `presence` stream from the owning peer, with a per-peer sequence number and a full resync on reconnect |

Only uniqueness needs consensus. Attributes change often (every server
switch) and are large (signed skin properties are 1–3 KB), and only the
owning peer changes them, so putting them in the log would cost snapshot and
log volume for nothing. Every peer combines its replica's claims with the
attributes it has received into the view that `PlayerRegistry`
(`src/players.rs`) serves. The tab list, the server-list count, the status
page, BungeeCord `PlayerCount`, `PlayerList`, `IP`/`IPOther` and
`UUIDOther` and managed-server player counts then cover the whole network
on every peer; the player metrics stay per process. Tab list rebuilds are rate-limited, so a burst of
joins does not rebuild every tab list once per join.

### Incarnations and connection ids

Every time a peer starts, it proposes `peer joined` with a new
**incarnation**. Applying it releases every claim of that peer's older
incarnations, so players of a peer that crashed and restarted can log in
again at once, and a full-cluster restart starts with no stale claims.

Connection ids are cluster-unique (`src/players/identity.rs`): their 53 bits
hold the peer's Raft id (8 bits, 1 to 255), the low 8 bits of its incarnation,
and the process-local counter. The owning peer of any connection id can be
read from the id, and an action addressed to a connection of an older
incarnation is refused instead of reaching another player's session. A single
Rift uses peer and incarnation 0 and keeps the ids it always had.

### Logins and duplicate logins

The name reservation and the UUID registration (`reserve_name` and
`register_visible`) are claims:

1. Before contacting a backend, the session proposes a claim for its
   lowercase name. Once the identity is confirmed, it proposes a claim for the
   UUID and name together. In offline mode the name claim follows the
   per-address rate limits and admission checks, so unauthenticated clients
   cannot flood the log.
2. Applying a claim succeeds only if no other live claim holds the name or
   UUID. Every peer applies the same entries in the same order, so exactly one
   of two simultaneous logins wins, wherever they connected. The loser gets
   `login.already_connected` or `login.duplicate_name`, as on a single Rift.
3. `cluster.max_players` (optional) caps the network: a claim beyond it fails
   deterministically unless it carries a bypass decided by the login hook.
4. Dropping the session's registration proposes a release.

A claim adds one commit to each login: a round trip to the leader and from it
to a majority, typically 1–5 ms in one data centre. Logins already wait far
longer on Mojang authentication. Claims are group-committed: while one
proposal is in flight, a peer collects the next claims and sends them
together.

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
  rates, not that they agree (decision 3).
- Existing players on a fenced peer stay connected for
  `cluster.partition_grace` (default 60 s), so a short blip disconnects
  nobody. If the peer is still cut off afterwards, it disconnects them with
  the partition message, so they can log in through a healthy peer, and their
  servers' counts on the majority side become true again (decision 4).
- After expiring a peer, the leader holds back idle stops and scale-down for
  servers that had its players, until `partition_grace` has passed.
- A peer that rejoins proposes `peer joined` and fresh claims for any players
  it still holds. A claim that now conflicts, because the player logged in
  elsewhere meanwhile, disconnects the session on the rejoining peer.

### Player actions on another peer

Every operation that sends a `PlayerAction` to a session first looks up the
player's claim:

- a local player gets the action through its channel, as on a single Rift;
- a player on another peer gets it as a request on the `player` stream to
  that peer, which runs it locally and replies with the same result type
  (`sent`, `offline`, `queued`, …).

Both paths sit behind the existing `Players` methods
(`src/extensions/players/routing.rs`), so these work across peers without
changes of their own: `rift.players.*` and `rift.network.*`,
`GET /api/players` and the message and kick routes, the plugin and Lua
players API, BungeeCord `Message`, `MessageRaw`, `KickPlayer`, `Connect`,
`ConnectOther`, `Forward` and `ForwardToPlayer`, and `rift admin transfer`.
Broadcasts go to every peer, and each delivers to its own players. Dialog
clicks arrive on the player's peer and are handled there, so a dialog opened
from another peer routes its callback back over the `player` stream.

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
- context reports that a backend sends are routed to the player's owning
  peer, like player actions (see [steps 4 and 6 notes](#steps-4-and-6-notes)).

## Message bus

### Subject scopes

Forwarding every message to every peer with matching interest is wrong for
Rift's own subjects: every peer subscribes to `rift.control.*`,
`rift.players.*`, `rift.network.*`, the permission request subjects and node
states without a queue group (`src/messaging_runtime.rs`,
`src/permissions/control.rs`, `src/node/registry.rs`). A request forwarded by
interest would run once per peer, and an instance would be created three
times. Every subject therefore has a **scope** (`src/messaging/scope.rs`):

| Scope | Subjects | Behavior |
| --- | --- | --- |
| `local` | `rift.control.*`, `rift.players.*`, `rift.network.*`, the permission request subjects, `rift.server.state`, the rest of `rift.node.*` | Answered by the peer the client is connected to; never exported. That peer forwards the work internally where needed: player actions to the owning peer, operations to the leader, service states to every peer |
| `applied` | `rift.events.instances`, `rift.events.permissions`, `rift.events.reload`, `rift.events.servers`, `rift.events.cluster`, `rift.perms.update`, `rift.perms.changes` | Published by every peer when it applies the entry, to its local subscribers only; never forwarded. Each subscriber gets exactly one copy |
| `origin` | Every other `rift.events.*` subject (`players`, `control`, `lifecycle`, `plugins`, …) | Published on the peer where they happen. Forwarded to peers whose **remote clients** have interest; in-process Lua and plugins receive only events of their own peer, so plugin code sees each event once |
| `owner` | `rift.node.<n>.cmd`, `rift.node.<n>.state` | Routed to the peer that holds the node's session claim (commands) or to the leader (states) |
| `cluster` | Everything else: user and plugin subjects, including `rift.perms.update.<uuid>` | Federated by interest, as below |

Lua and plugin subscriptions accept a `scope` (`"cluster"`, the default for
user subjects (decision 11), or `"local"`). The plugin host's delivery of
every `rift.events.>` message to every plugin (`src/plugins/mod.rs`) follows
the `origin` and `applied` rules.

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
- **Request and reply.** Inboxes are `_INBOX.<peer>.<random>`, so a reply is
  routed by its prefix without waiting for the inbox's interest to spread.
- **No responders.** A request with no local subscribers and no remote
  interest fails with `NoResponders` at once. A request forwarded to a peer
  whose subscribers left meanwhile gets a no-responders reply from that peer.
  Node requests use the `owner` scope and the session claim instead, so
  "unreachable" stays exact.
- **Identity.** Clients authenticate on the peer they connect to. Forwarded
  messages carry the publishing principal, so permission checks that depend
  on the publisher give the same answer on every peer.

Bus clients receive a list of peers. `crates/rift-messaging`'s client takes
an address list and reconnects to the next peer when one fails; the Java
client, the node agent (`--controller`, repeatable) and the bus-login files
written to servers get the same list.

### Bus logins

The key that derives bus-login tokens is a generated file on a single Rift,
`local/instance-key` (`src/instance_login.rs`), so a token derived on one
peer would fail on another. `rift cluster init` moves the key into the
replicated state, and every peer derives and checks tokens from it. Login
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

### Node sessions

A node principal has one session at a time: a new login closes the old one.
In a cluster, node sessions are claims: a node agent connecting to any peer
claims `node.<name>`, and the peer that held the older session closes it. The
claim also tells the leader which peer to route the node's commands through.

## Work that runs once: the leader

Some work must happen exactly once in the network. It runs on the current
Raft leader and nowhere else:

- the scaler (`src/scaling.rs`), instance creation, removal and moves
  (including resumed copies and moves of named servers);
- scheduled backups of persistent instances (`src/backups.rs`) and restores;
- managed server and instance lifecycle: starting, stopping, idle stops,
  restart retries and recovery;
- node control commands and node schedules (cordon, drain);
- recovery of half-done operations: a single Rift aborts `creating` records
  and finishes `removing` ones at every start; in a cluster only the leader
  does this, when it gains leadership;
- writes derived from configuration: mirroring `extensions.groups`, pruning
  removed groups and nodes, and the permission expiry sweeper;
- the stream service, the queue service, template source syncs (git);
- plugin jobs with `scope = "cluster"` (decision 8).

Running these on one peer, rather than letting every peer race for them
through the database, keeps each of them a plain single-process algorithm.
The price is that followers hand their requests to the leader.

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
   agent acknowledged its fence. Inspection commands are not fenced: any
   peer's web API may send them for any operator.
4. **Reconcile on leadership gain.** A new leader already has node states
   from its time as a follower, so the reconnect path that triggers adoption
   on a single Rift (`src/node/registry.rs`) would not fire. The leader
   therefore reconciles every agent after fencing it, exactly as a restarted
   controller adopts servers.

### Inputs from other peers

The leader decides from replicated state where it can: player counts per
server come from presence, instance records and node schedules from the
database. Reservations (players being sent to a server that is starting) and
queue tickets change too fast to replicate:

- **Start on connect.** A session that needs a stopped server asks the leader
  and waits until the server is running.
- **Idle stops.** On a single Rift one lock guards both reservations and idle
  detection (`src/managed.rs`), so a player can never connect to a server
  that is being stopped. In a cluster, before stopping a server for being
  idle or for scale-down, the leader asks every live peer over the `load`
  stream. A peer stops routing new players to that server and answers with a
  veto if it has a reservation or a player on its way there. The leader stops
  the server only if no peer vetoes and no peer is unreachable.
- **Server states** (`running`, `starting`, `unknown`, the last error) are
  written by the leader as `operation` entries, so every peer routes from the
  same view and a new leader starts with the old leader's view.
- **Service states** that backends report on `rift.server.state` (`LOBBY`,
  `INGAME`, drain flags; `src/service_state.rs`) arrive on whichever peer
  the backend's client uses. That peer broadcasts them to every peer, and a
  peer that starts asks the others for a full copy, so it never routes players
  into an `INGAME` or draining server.

### Servers on the peers' own hosts

`local` means the proxy's own host on a single Rift. In a cluster each peer
runs an embedded node agent and is a node named after itself, with labels and
limits from its `cluster.toml` (`src/cluster/peer_nodes.rs`). Managed servers
and service groups select peers like any node. The name `local` is refused in
a cluster. `rift cluster init` converts existing local registrations (stored
with an empty node and absolute plan directories) to the initializing peer's
node.

## Configuration and files

### One configuration

Every peer runs the same `rift.lua`. The peer's own name is visible to the
script as `rift.peer`, so per-host differences stay in the one file. Branching
on `rift.peer` is allowed only in **peer sections**; every other section is
**shared**: it writes durable state or drives leader tasks. Each peer hashes
the shared sections of its evaluated configuration, and a deployment whose
shared sections differ between members is refused.

Configuration **deployments** (the dashboard, `PUT /api/config`, `rift admin
reload` after an edit) are `config` entries. Every peer writes the deployed
source to its `rift.lua` atomically and reloads. The deployment history
(`deployments.json`) and the operator audit trail (`audit.jsonl`, both in
`src/operator.rs`) are replicated tables, so rollback and audit work from any
peer. A peer that starts with a `rift.lua` that differs from the last
deployment adopts the deployment if its file matches an older deployed
revision. If it matches none, which means someone edited it by hand, the peer
refuses to start and keeps the file; `--adopt-deployed` overrides
(decision 6).

### Templates and other files

Template folders, plugin jars, maps and the software cache can be large, so
they never travel in the log:

- A `manifest` entry records a folder's files by path, size and SHA-256.
- Each peer stores contents by hash under `cache/blobs/` and fetches missing
  blobs from any peer that has them, over the bulk connection.
- An upload or a git sync stores the blobs, copies them to a majority of
  voters, then proposes the manifest, so a committed manifest never
  references a blob only one host has.
- `template_storage` on each peer is materialized from the manifest, so node
  agents keep fetching templates over `--web` from whichever peer they reach.

Server directories on hosts (nodes and peers) stay where they are: they are
the data of running servers, not shared configuration.

### Key-value stores

- A key-value store in the instance database (`extensions.storage.database =
  true`) is replicated with `kv` entries.
- A store kept in its own file (`path`) cannot be replicated as a file. In a
  cluster it is a configuration error; `rift cluster init` copies its entries
  into the instance database.
- Writes from plugin hooks wait at most `STORE_WAIT` (5 s,
  `src/plugins/services.rs`). An election takes 1.5–3 s, so hook writes
  during an election usually finish late rather than fail, but a longer
  outage makes them fail with a timeout, as a stalled disk would.

### Secrets

Secrets stay in each peer's environment: the cluster token, node tokens, the
forwarding secret and the web tokens. They must be equal on all peers; the handshake checks the forwarding secret and the node tokens without
revealing them. The bus-login key moves into the replicated state.

## Code map

| Area | What it holds |
| --- | --- |
| `src/cluster/mod.rs`, `node.rs` | The `Cluster` handle: Raft node, leadership, the lease, status and metrics |
| `src/cluster/settings.rs` | `cluster.toml` and `rift.peer` |
| `src/cluster/link.rs`, `quic.rs`, `transport.rs`, `network.rs` | Peer links, handshakes, the openraft network |
| `src/cluster/log_store.rs`, `state.rs`, `snapshot.rs`, `replicated.rs`, `operations.rs` | The Raft log, the state machine, snapshots, the instance database entry kinds and leader execution |
| `src/cluster/membership.rs`, `cli.rs` | `rift admin cluster`, `rift cluster init\|recover`, `rift witness` |
| `src/cluster/players/` | Claims, attributes, fencing, routed player actions |
| `src/cluster/bus*` | Federation, node sessions, the bus-login key, bus addresses, the stream service, `rift.events.cluster` |
| `src/cluster/leader.rs`, `src/cluster_runtime.rs` | Leader tasks, managed-server coordination, forwarded requests, leaving |
| `src/cluster/peer_nodes.rs` | Peers as nodes |
| `src/cluster/files*` | Deployments, history and audit, shared-section hashes, template manifests and blobs |
| `src/cluster/queues*` | The network-wide queue service, leases and job scopes |
| `src/cluster/attach.rs` | The one place every component is registered |
| `src/cluster/sim*` | The deterministic simulation (simulated network and clock) and its tests |
| `src/database/changeset.rs` | Session-extension FFI, changeset apply, checksums, primary-key assertions |
| `src/messaging/scope.rs` | Subject scopes |
| `src/players/identity.rs` | Cluster-unique connection ids |
| `tests/cluster.rs` | Three real peers in CI (see [step 10 notes](#step-10-notes)) |

## Implementation notes

The plan had ten steps: the design, the cluster core with its simulation
harness, the replicated database, presence, the bus, routed player actions
and permissions, leader tasks, configuration and files, queues, and
hardening. Each step recorded where it differs from the design above.

### Step 2 notes

What the cluster core does differently from the design, and why:

- **Encoding.** Log entries, Raft messages and handshakes are JSON: a newer
  peer reads an older peer's entries, and an unknown kind fails loudly. Each
  entry kind carries its own version `v`.
- **The state machine's file.** Step 2 kept the state machine in a file of
  its own, `rift.cluster.sqlite3`. Since step 3 it lives in the instance
  database (see [step 3 notes](#step-3-notes)); that file is gone.
- **Log purging.** openraft purges only entries a snapshot covers. A peer
  therefore builds a snapshot once its log passes 64 MiB (or when a member
  needs one) and openraft purges behind it, keeping 10,000 entries for
  followers that lag a little. "Purge once every member replicated" is not a
  separate rule.
- **Building a snapshot holds apply** for the length of the copy, so the copy
  matches the log id it is labelled with.
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
  `LeaderContext::has_lease` themselves. Production timing is a 250 ms
  heartbeat and a 1.5–3 s election timeout.
- **The shared-configuration hash** in handshakes was the SHA-256 of the
  `rift.lua` source text until step 8; it now hashes the shared sections
  (see [step 8 notes](#step-8-notes)).
- **`replace`** removes the old Raft id and allocates a new one at once; if
  the peer is not running yet, `rift admin cluster add <name> <address>`
  finishes the job once it is (it reuses the allocated id).
- **`leave`** first only moved leadership away; draining came with step 7 and
  the Transfer packet handoff with step 10.
- **Peer certificates** are used both ways, so each must name the host of
  its `advertise` address and allow server and client authentication.
- `rift.control.cluster` and `rift.events.cluster` came with the bus
  (step 5).

### Step 3 notes

- **One database.** The state machine is the instance database, opened in
  replicated mode (WAL, `synchronous=NORMAL`, one writer and a pool of read
  connections). Entries are applied on the writer; the apply hooks of the
  stores run after each entry commits, while apply still holds the writer.
  `cluster_applied`, the stored results of proposals and the other
  `cluster_*` tables are tables of the instance database.
- **Startup order.** `cluster::attach::database` opens the database, starts
  the peer with it, installs the cluster's executor and waits until the
  replica holds the cluster's state at this build's schema; only then do the
  stores open. A new peer waits there until a member adds it and it has
  installed a snapshot.
- **Schemas.** In a cluster a store never migrates when it opens; it checks
  that its schema is the one this build writes. `rift cluster init` records
  every component's version with a `migrate` entry (creating what the
  database lacks). Each peer announces the schema versions it can run
  against as features (`schema:<component>:<version>`), and the leader
  proposes a `migrate` entry once a committed `features` entry lists the new
  version, so every member supports it.
- **The first state is not in the log.** It is the instance database as
  `rift cluster init` found it, so init builds a snapshot and purges the log
  behind it: every peer added later starts from a snapshot.
- **Hot paths.** `kv` and `directory` entries carry the operation, which every
  peer executes when the entry applies; they bypass the leader's batches.
  They touch tables no batch touches, so their order relative to batches in
  flight does not matter. A `migrate` entry waits until no batch is in flight.
- **Group commit.** The leader queues the operations it receives and takes
  everything queued into one entry; while entries are in flight it executes
  the next batch on top of their changesets (`capture_on`). An entry carries
  the term it was computed in, so a leader that lost and regained leadership
  never appends a batch computed against an older state. A new leader first
  waits until it applied everything committed before its term; outcomes of
  operations that committed under the old leader are then found in the
  replica, which is how a retry gets the stored outcome.
- **Snapshot install while serving** happens under the writer, which pauses
  apply and leader execution. The snapshot is written into the live file
  through the writer with the backup API, so no file is renamed or replaced
  while handles are open (Windows refuses that); the writer and read
  connections are then reopened and every hook runs in full reload mode.
  The permission authority's reload first raises the events of the
  revisions it missed, then reloads; join and leave events come with
  presence (step 4), and `StateMachine::is_installing` tells logins to wait.
- **Leader-only writes** such as startup recovery of half-done creations ran
  on every peer at startup until step 7 moved them to the leader; they go
  through the leader like any other write.
- **File-backed key-value stores** are refused in a cluster;
  `rift cluster init` copies their entries into the instance database.

### Steps 4 and 6 notes

Presence and routed player actions live in `src/cluster/players/`, on top
of seams that a single Rift fills with its in-process implementations
(`src/players/`, `src/extensions/players/routing.rs`,
`src/service_state.rs`). Where they differ from the design:

- **Claim entries are batches.** One `claim` entry carries the operations a
  peer collected while its previous entry was in flight, and its result is
  one outcome per operation. The claims are a table of the replicated
  database (`cluster_claims`), so they survive restarts, travel in
  snapshots and are part of every checksum. A login's claim is refused
  when its holder is not the current, unexpired incarnation of a member, so
  a claim proposed just before the leader expired its peer cannot land
  after the expiry. `peer removed` releases the claims of every peer that
  is no member any more.
- **Rejoining keeps the incarnation.** `peer joined` with the incarnation a
  peer already announced is accepted once that incarnation was expired;
  any other repeat is still refused.
- **Contact means a lease.** A follower counts as in contact with the
  leader only while the leader answers its probes (on the `presence`
  stream) and still holds its lease. A peer that reaches a leader cut off
  from the majority therefore fences itself too.
- **Catching up** means: this peer has not yet applied an index the leader
  had applied more than a second ago, has not joined with its current
  incarnation, `claim` entries are not enabled yet, or its replica
  diverged. Logins wait with `login.unavailable` meanwhile.
- **Expired peers stay quiet.** An expired peer sends no player attributes
  until it has joined again and claimed its players anew, and a peer that
  claimed a player never announces them once their claim is lost: other
  peers never list a player twice. When two peers' attributes name one
  player anyway, the claim decides which record the roster keeps.
- **Expiry and stop holds.** The leader's maintenance loop expires peers;
  applying `peer expired` records the servers the expired peer's players
  were on, held back from idle stops until `partition_grace` has passed
  (`Presence::stop_holds`).
- **Context reports** that a backend sends to a peer which does not hold
  the player go to the holding peer over the `player` stream; that peer
  accepts them as it would a local report. Writes and console commands
  that a backend makes on behalf of a player (`actor_uuid`) are still
  checked against the peer the backend's client is connected to.
- **Expected arrivals need no routing:** the session's own peer publishes
  the player's permissions and records the arrival before it forwards the
  connection.

### Step 5 notes

The bus (`src/cluster/bus*`) differs from the design as follows:

- **Node session claims are `bus` entries, not `claim` entries.** One
  component applies each entry kind, and the `claim` kind belongs to player
  presence. A `bus` entry claims `node.<name>` for `(peer, incarnation,
  session)`; the claim applied last holds the node. A peer closes its own
  session of a node only once that session's claim had applied and another
  peer's claim applied after it, so two quick reconnects never close the
  newer session. A claim counts while its peer runs that incarnation and has
  not expired.
- **The bus-login key is a `bus` entry too.** `rift cluster init` moves
  `local/instance-key` into it and removes the file. A cluster that has no
  key yet takes the key of the first peer that attaches its bus, so the
  tokens of the peer that was the single Rift stay valid. Every peer derives
  and checks tokens from the replicated key; without a cluster the file is
  used as before.
- **Bus addresses.** Each peer records its messaging address and certificate
  name in a `bus` entry; `RIFT_BUS_ADDRESSES` lists every live peer, this
  peer first. When the peers' certificate names differ, `RIFT_BUS_SERVER_NAME`
  is left unset and clients verify each address for its own host. Node agents
  get the list as repeated `--controller` flags.
- **Federation** sends frames to a peer in batches on the `bus` stream, each
  batch answered before the next one goes, which keeps them in order. A
  peer's queue holds at most 16,384 frames or 32 MiB; an overflow or a failed
  batch drops frames, counted in `rift_cluster_bus_dropped_total`, and the
  peer then gets this peer's whole interest again. Peers that do not serve
  players (witnesses) are not federated.
- **Retained streams.** Delivery state (pending records, redelivery deadlines,
  delivery counts) stays in the leader's memory; the replicated state holds
  the records, the consumers and, per consumer, an acknowledged floor and the
  acknowledgements above it. A new leader starts each consumer at its floor
  and skips acknowledged records, so pending records are delivered again
  with a delivery count starting at 1. An acknowledgement that reaches a new
  leader for a record it has not delivered is accepted while the record is
  retained, matches the consumer and is unacknowledged, since the previous
  leader may have delivered it. Records that file-backed streams held before
  `rift cluster init` are not carried into the cluster.
- **The permission change log** (`rift.perms.changes`, stored in
  `rift.perms-changes/`) stays a stream of each peer: every peer appends each
  permission revision when its permission store commits or applies it, so
  the records are the same on every peer. Consumer positions on it are per
  peer.
- **Scopes for subscriptions**: `messaging.subscriptions[].scope` and
  `rift.subscribe(subject, { queue = ..., scope = "local" }, fn)` in
  `rift.lua`, and `scope` in plugin Lua (`rift.subscribe`). WebAssembly
  plugins keep `cluster` scope: a scoped `subscribe` in `wit/rift-plugin.wit`
  needs the C, Go and Rust SDKs regenerated and is left for later.

### Step 7 notes

Leader tasks live in `src/cluster/leader.rs` (the library part) and
`src/cluster_runtime.rs` (the proxy's part). Where they differ from the
design, or stopped short of it:

- **Wiring.** `cluster::attach::cluster_components` is the one place every
  step registers its state machine components; `main`, `rift cluster init`
  and `rift witness` all start their peer with it, so every peer, the
  initializing one and witnesses included, announces the same entry kinds
  (a witness without `claim` or `bus` support would hold their activation
  back for good). The player registry the presence component serves is
  made there and taken by the first proxy generation's players. `serve`
  attaches presence (with the process-wide identity) and the bus once the
  broker, the streams and the QUIC listener exist and before the first
  login; reloads keep the broker, the players, the nodes and the managed
  servers, so the attachments stay valid without re-attaching. `/metrics`
  adds the bus series. Witnesses are not federated although they apply
  `bus` entries: the peer table says which peers are witnesses.
- **Leader tasks** start from `on_leader_gain` once the new leader holds its
  lease and has passed a read barrier, so it applied everything committed
  before its term. They run in the term they gained (`leader::in_term`, and
  `leader::with_thread_term` on blocking threads): the cluster's executor
  tags every write such a task makes through the stores with that term, so
  a deposed leader's writes are refused by the new leader. Once per term the
  new leader recovers half-done creations, removals and moves
  (`Store::recover`: unfinished creations are aborted, removals finished,
  stale moves of named servers forgotten), prunes group states, node
  schedules and server states, runs the permission upgrade
  (`migrate_once`) and mirrors `extensions.groups`. For as long as it leads
  it fences and reconciles the node agents, sweeps expired permissions and
  runs scheduled backups. The scaler and instance creations, removals and
  moves run in the control loop only while `Coordination::acting_term`
  says this peer leads, in that term. Every other peer restores the
  registry without recovering anything (`Store::restore_with`).
- **Managed servers** follow a `managed::Coordinator`. Only the leader's
  workers start, stop, adopt, recover, restart and idle-stop servers; the
  leader records each server's phase, last error, administrator intent and
  restart attempts in the `server_states` table (a schema component of its
  own, written with `InstanceOp::SetServerState`), and every other peer's
  workers mirror those records, so all peers route from one view. A peer
  that starts to lead takes every server over from the record and adopts
  processes from what the agents report, never from what it owned during an
  earlier leadership; a server that looks stopped then is not recorded until
  its agent reported or ten seconds passed.
- **Followers hand requests to the leader** over the `load` stream:
  administrator starts and stops and start on connect (through
  `ManagedServers`, so the admin CLI, the HTTP API and `rift.control.*` all
  follow), and instance creations and removals (through the control loop).
  A start on connect waits for the leader's start, then until the replica
  shows the server running (at most five seconds more). Step 7 refused moves
  and other node commands that change something on a follower; they are
  forwarded to the leader now (see [step 10 notes](#step-10-notes)).
- **Fencing.** Agents advertise `fence`. `Command::Fence { term }` stores
  the highest term in `.rift-fence` below `--root`; every control command a
  cluster sends carries the leader's term in a `term` field beside `op`
  (agents of earlier releases ignore it), and the agent refuses control
  commands of a lower term, or without one once fenced. `logs`, `console`,
  `files`, `upload`, `tree` and `read` are inspection and never fenced; on a
  follower they go out without checking the node's reachability, which only
  the leader observes. `Nodes::request` fences an agent on demand before its
  first control command of a term; an agent without `fence` gets no control
  command in a cluster and is reported when it connects, by
  `rift check --strict` and in the leader's log. The leader fences and
  reconciles every online agent once per term and again after it
  reconnects, so adoption does not depend on the reconnect path.
- **Inputs from other peers.** Every peer reports its reservations to the
  leader each second; the scaler adds the reports younger than five seconds.
  Queue lengths are not reported: queues are the leader's service (step 9).
  Before an idle stop, and before a scale-down removal the scaler decided,
  the leader asks every live peer that serves players; a peer vetoes while
  it has a reservation for the server, and otherwise routes no new player
  there for fifteen seconds (`ManagedServers::quiesce`), by which time the
  leader's record of the stop takes over. A peer that does not answer counts
  as a veto, as do `Presence::stop_holds` after a peer expiry. A vetoed
  server is asked about again after another idle period.
- **`local` in a cluster.** Configuration validation on a peer (and so
  `rift check`) refuses managed servers, service groups and instances
  without a node. `rift cluster init` moves the ready instances of this host
  to the initializing peer's node, with the directory a node gives an
  instance of its group, and prints which `--root` keeps each folder in
  place, and says what else must change before the peer starts. Step 7 left
  out the embedded node agent and asked for a `rift node --name <peer>`
  agent on each peer's host; peers are nodes by themselves now (see
  [step 10 notes](#step-10-notes)).
- **`rift cluster init`** also moves the bus-login key into the cluster
  (`bus::import_key`).
- **Leaving.** `rift admin cluster leave <peer>` runs on the leader, which
  hands its leadership away first if it is the one leaving, and asks the
  leaving peer (over `load`) to drain. Draining is the shutdown that SIGTERM
  starts: the listeners stop taking logins, the peer hands the leadership to
  another voter if it has it, the bus listener closes (its clients
  reconnect elsewhere), the sessions drain (releasing their claims), and
  presence, the bus, the leader tasks and Raft stop. Step 10 added the
  Transfer packet handoff.
- **`rift check --strict`** on a peer also reports a key-value store in a
  file of its own, and, through the running peer's admin listener, a peer
  that is not joined or diverged, a cluster without a leader, peers that
  are unreachable or refused (another cluster token, other shared secrets),
  expired, behind by more than 1,000 entries or on another version, and
  node agents that cannot be fenced.
- **Tests.** Step 7 first left the scaler across a leader change, a fake
  agent fenced and reconciled by a new leader, and stop holds after a peer
  expiry untested, and `rift.events.instances` was published by the peer
  that made the change only. All of these were done later; see
  [step 7 notes (continued)](#step-7-notes-continued).

### Step 7 notes (continued)

What the second pass of step 7 added, and where it differs from the text
above. It replaces the parts of the [step 7 notes](#step-7-notes) that said
peers are not nodes, that followers refuse moves, backups, restores and
warming, and what was not covered by tests.

- **Peers are nodes** (`src/cluster/peer_nodes.rs`). Every peer that is not
  a witness runs an embedded node agent in its own process and is a node
  named after itself; no `rift node` process, no `nodes.<peer>` entry and
  no token are needed. The agent (`Agent::serve_broker`) subscribes to
  `rift.node.<peer>.cmd` on the peer's own broker and publishes its state
  there, with the fencing, reconciliation and state reports of any agent.
  Owner routing sends `rift.node.<n>.cmd` to the peer holding node `n`'s
  session claim as before, and otherwise to peer `n` when it is a peer
  that serves players; states go to the leader as before. A peer's servers
  live below the directory of `rift.lua` (the agent's root), where servers
  on the proxy's host lived before the cluster, so the instances
  `rift cluster init` converts stay in place: `init` gives them to the
  peer's own node without needing `nodes.<peer>` and says whether a folder
  must move. The agent reads templates from the peer's materialized
  `template_storage` instead of `--web`, runs any program the shared
  configuration names (as servers on the proxy's host do, without an
  `--allow-program` list), and gets the peer's forwarding secret directly
  instead of a sealed one.
- **What a peer-node is** comes from the replicated peer table: its address
  is the host of the peer's `advertise` address; its labels are every key
  of `[labels]` in its `cluster.toml` plus every value that is a valid label
  (`region = "eu"` gives `region` and `eu`), at most 32; its limits come
  from the `[node]` table (`memory_mb`, `max_instances`, `min_free_disk_mb`,
  meaning what they mean in `rift.config.nodes.*`). A peer announces the
  limits in `peer joined` under the reserved label keys `node.memory_mb`,
  `node.max_instances` and `node.min_free_disk_mb` (so `[labels]` may not
  use keys starting with `node.`), which keeps the peer table's schema as
  it was; `GET /api/cluster` shows them among the labels. Witnesses are no
  nodes. A `nodes.<peer>` entry in the configuration still wins: then that
  peer runs no embedded agent and its `rift node` agent serves the host, as
  the first pass required.
- **Peer-nodes in the configuration.** Each process keeps a registry of
  the peer-nodes, loaded once the replica holds the cluster's state and
  updated whenever the peer table changes. A configuration evaluated on a
  peer gets them in `Config.nodes`, marked `NodeConfig::peer`, so
  placement (`scaling::place`), node commands, moves, backups, warming,
  `/api/nodes` (with `"peer": true`) and the leader's fence-and-reconcile
  loop treat them like any node; they have no token, no QUIC login and no
  messaging requirement, and the shared-section hash leaves them out. When
  the peer table changes, every peer's control loop takes the new
  peer-nodes into its configuration (`Update::PeerNodes`, through
  `cluster_runtime::follow`) and the node registry. Before the registry is
  loaded (the first evaluation before the peer attached, `rift check` and
  `rift witness`), validation accepts any valid peer name as a node and
  does not check labels or a named server's node address yet; the
  configuration is evaluated again once the peer is attached, strictly.
  So servers and groups that name a peer as their node pass
  `check_cluster_placement` and node validation.
- **Reachability.** A peer-node is `unreachable` while its peer is expired,
  whatever its agent last reported, and otherwise follows its state
  reports like any node. A peer that leaves the membership stops being a
  node; instances still registered on it then make the configuration
  update fail, as removing a node with servers does.
- **Stopping with the peer.** The servers on a peer's host stop when its
  proxy process stops (the embedded agent stops them after the sessions
  drained). Named servers with `keep_running` on a peer's own node keep
  running: the agent starts them as kept servers (own session, console FIFO,
  `rift-process.json`; see [kept servers](kept-servers.md)) and the next
  agent of that peer adopts the recorded process when the leader starts the
  server again. `keep_running` in a service group with `nodes` is still
  refused, also in a cluster, because an instance placed on a `rift node`
  agent's node cannot keep running that way; only named servers on a peer's
  own node support it.
- **Followers forward the remaining leader-only requests** instead of
  refusing them (`leader::ForwardedRequest` on the `load` stream; the
  leader's one handler dispatches on the kind and runs it in its term):
  `move` (`rift admin move`, the dashboard and `POST` move routes),
  `move_status`, `move_cancel`, `backup`, `backups_forget`, `restore` and
  `warm`. The follower checks permissions as before and sends the
  operator's name; the leader runs the request for that operator and adds
  an audit record `forwarded.<kind>` under it (in a cluster both records
  land in the shared audit log). A forwarded move's request waits for the
  whole move; meanwhile the follower polls `move_status` every second, so
  `servers` and `GET /api/servers` show the leader's progress, and a
  cancel is a request of its own that works from any peer. Template source
  syncs were already forwarded.
- **Applied events on every peer.** `rift.events.instances` is published by
  every peer from the instance store's change hooks
  (`instance_store::events::InstanceEvents`, installed by
  `cluster_runtime::prepare`), once per applied write, to that peer's own
  subscribers; in a cluster the control loop no longer publishes it, so
  the originating peer does not publish twice. The payloads are those of a
  single Rift, except that fields only the deciding process knows
  (`files_removed`, `files_copied`, `source_removed`, `cleanup_error`) are
  left out on every peer, a creation that registered and was then undone
  shows as `created` then `removed`, and a forgotten move of a named server
  is announced as `moved` back to its configured node. `permissions`,
  `servers` and `reload` already followed the `applied` rule (each peer
  publishes when it applies the revision, the report or the deployment).
- **Witnesses hold blobs.** `rift witness` attaches the configuration and
  file state without a configuration (`files::attach_witness`): it serves,
  stores and fetches template blobs and counts toward the voters an upload
  must reach, and records deployments without applying them.
- **`rift --config <missing file>`** on a peer with a `cluster.toml` starts
  like `rift` without a file: once attached, it receives the deployed
  configuration at that path.
- **Deciding in the library.** The leader's "decide, then change" step of
  the control loop is `leader::decide` and `leader::Acting` (alone, leads
  in a term, or follows): a peer decides only while it leads and has
  caught up in its term (`Coordination::deciding_term`, set by a
  leader-gain hook once the read barrier passed), and the decided change
  runs in the term it was decided in. A new leader therefore never scales
  from a replica that has not applied everything committed before its term.
  The fence-and-reconcile loop is `leader::fence_and_reconcile`; the proxy
  calls both.
- **Tests** (`src/cluster/sim/`): one demand creates exactly one instance
  across a leader change, the old leader's commit being refused in its
  term and its half-done creation recovered (`agent_tests`); a fake agent is
  fenced and reconciled by each new leader, refuses commands of the old
  term or without one, and still answers `logs`, `tree` and `read`
  (`agent_tests`); an expired peer's servers are held from idle stops until
  `partition_grace` ends (`agent_tests`); labels of `cluster.toml` reach
  every peer and select a peer-node, and the embedded agent starts a server
  there for a fenced leader and refuses a lower term (`peer_node_tests`);
  a follower's forwarded move or backup reaches the leader's handler with
  its operator (`forward_tests`); and applied instance events arrive once on
  every peer (`events_tests`). The creation in the first test is the store
  sequence the proxy runs, not the proxy's `create_remote_instance`, which
  needs the binary's snapshot.

### Step 8 notes

Configuration and files live in `src/cluster/files*`. Where they differ
from the design:

- **Deployments.** A `config` entry carries the source, its revision, the
  actor and the action (`save`, `rollback`, `reload`, `init`, `startup`);
  the leader stamps its time. Applying it only records it in
  `config_deployments` (the last 64, as `deployments.json` kept). Each
  peer's deployer then writes the source to `rift.lua` atomically (a
  flushed sibling file renamed over it, with its permissions) and reloads
  through the configuration service's ordinary reload, off the apply path.
  The peer that proposed the deployment applies it in the configuration
  transaction that proposed it; if that transaction fails after the commit,
  the deployer reloads after 30 s. A peer whose reload fails keeps running
  its previous configuration and reports why (`config_peers`), which
  `rift admin cluster` and `GET /api/cluster` show under `configuration`,
  with the metrics `rift_cluster_config_in_sync`,
  `rift_cluster_config_reload_failed` and
  `rift_cluster_config_shared_mismatch`. A failed reload is not retried
  until the next deployment or a manual reload.
- **What becomes a deployment.** `PUT /api/config`, structured edits,
  rollbacks, and a reload (`rift admin reload`, SIGHUP, `POST /api/reload`)
  whose `rift.lua` is not a deployed revision. A reload whose file is an
  older deployed revision applies the last deployment instead (the deployer
  is about to rewrite the file); one whose file is the last deployment is a
  plain reload, as on a single Rift.
- **History and audit** are `config_deployments` and `config_audit` (the
  last 1,024 operator records). The summaries of refused requests stay with
  each peer. `rift cluster init` imports `deployments.json` and the
  operator records of `audit.jsonl` into a cluster that has no deployment
  yet, and records the current `rift.lua` as the last deployment; a
  cluster created before this step records the first peer's file when it
  starts.
- **Peer and shared sections.** The peer sections are the listeners (with
  the metrics, admin and status listeners and the PROXY protocol), routes
  (with `on_route`), limits (with the rate limits), health checks, the
  server list (with its cache and `on_status`), the tab list and the web
  listener. The others are shared, except `instances` and
  `moved_servers`, which come from the instance database, and peer-nodes,
  which come from the peer table. A section's hash is the SHA-256 of the
  `Debug` rendering of its parsed structures (ordered maps throughout), with
  the directory of `rift.lua` and its file name replaced, so peers installed
  under different paths agree.
- **Activation.** The proposing peer evaluates the candidate once per
  member name (evaluation is local Lua) and refuses it, naming the peers
  and sections, if a shared section differs; the entry carries every
  member's hash, and applying it refuses a deployment that was not
  evaluated for every current member (the proposer evaluates again and
  retries) or whose members disagree. Peers do not refuse a deployment
  because another peer's *running* configuration differs: that would block
  the deployment that fixes it. A peer that runs the last deployment but
  evaluates a shared section differently (other Lua modules or plugin
  folders on its host) is reported by section.
- **Handshakes** carry the shared hash of what the peer runs and follow
  reloads; a difference is logged, not refused, since peers reload one
  after the other during a deployment.
- **`rift check --strict`** on a peer evaluates the file as every member in
  the peer's replica would and names the peers and sections that differ,
  says whether the file is the last, an older or no deployed revision, and
  reports peers whose reports show another shared hash for the last
  deployment or a failed reload.
- **Startup (decision 6)** runs after the replica caught up (a linearizable
  read, or the local replica with a warning when there is no quorum) and
  before the configuration is loaded again: `--adopt-deployed` keeps the
  hand-edited file as `rift.lua.hand-edited`. Only the 64 retained
  deployments count as deployed revisions. A new peer started without a
  `rift.lua` receives the deployed one. Its `rift.config.cluster` settings
  are the defaults until it restarts with the file.
- **Templates.** A folder is a top-level entry of `template_storage`; a
  `manifest` entry replaces a folder's listing (paths, sizes, SHA-256 and
  empty directories) or removes it, optionally only if the folder is still
  at the version the change was computed against (uploads and deletes
  retry on a conflict). Top-level names starting with a dot are not
  replicated. Blobs live in `cache/blobs/` beside `rift.lua`. An upload or
  a git sync stores its blobs, has a majority of voters fetch them (each
  voter pulls them over the `blob` stream and answers once stored) and
  only then proposes the manifest. Transfers go in 1 MiB chunks into
  `cache/blobs/tmp/<sha256>.part`, resume from its length (from any peer)
  and are checked against the SHA-256 before the blob is placed; a copy
  with another hash is deleted and refused. Each peer writes the folders
  of the manifest whose blobs it holds (a staged copy renamed over each
  changed file, links removed and never followed, unlisted entries
  removed) and retries the others every 30 s. It removes only folders it
  wrote itself (`cache/materialized.json`), so folders a peer had before it
  joined stay until they are deleted through the API. Unreferenced blobs
  go after an hour.
- **The software cache** (`software`, `POST /api/templates/{name}/download`)
  stays per peer: it is a cache of public downloads, which each peer fetches
  when it first needs them.
- **Git syncs** run on the leader: `POST /api/template-sources/{name}/sync`
  and the check-out before an instance is created are forwarded to it over
  the `blob` stream. A new leader checks out and publishes the configured
  sources the manifest lacks. The record of which commit a source was synced
  from stays in the leader's software cache, so a leader that never synced a
  source fetches it again when it is next needed.
- **Web tokens** come from the configuration, so they work on every peer;
  the random token a process creates when none is configured stays with
  that process. The web API keeps no sessions.
- **Witnesses** register the components (they apply every entry) but do
  not attach the configuration deployer, so they write no `rift.lua`. They
  do hold template blobs (`files::attach_witness`), so they count toward the
  majority of voters an upload must reach before its manifest commits.

### Step 9 notes

Queues and the plugin-facing cluster API live in `src/cluster/queues*`, on
top of `crate::extensions::queue`, which holds the ordering and admission
rules a single Rift and the leader share. Where they differ from the design:

- **Tickets are not replicated.** A ticket lives in the memory of the peer
  that holds the player's session. Each peer sends the leader its whole
  queue state (its live tickets with priorities and the slots its sessions
  hold, by destination) on the `queue` stream, at once after a change and
  at least every second; the report is also the heartbeat. The leader
  answers each report with that peer's view: the network-wide order (as
  priorities), the head of each queue, the slots the other peers hold and
  the positions of the peer's own tickets.
- **Arrivals.** The leader stamps a ticket it sees for the first time with
  its wall clock (milliseconds times 1024, plus a sequence that keeps them
  unique and increasing), and orders by priority, then arrival, then peer and
  connection id. Every peer keeps the stamps of its tickets and sends them
  again after a leader change; the new leader places a stamped ticket where
  the old one had it and stamps new ones after every stamp it has seen. Order
  across a leader change therefore assumes that the peers' clocks roughly
  agree, as entry stamps do. A new leader collects reports for two seconds
  (`SETTLE`) before peers admit by its views and it lets heads in.
- **Capacity.** A destination's used slots are the sum of every live peer's
  reported slots, or the players the merged player registry puts on the
  destination and its instances, whichever is more. A peer adds its own
  current leases to the other peers' last reported ones, so its own
  admissions count at once.
- **Turns.** Four times a second the leader drops peers that stopped
  reporting for `cluster.peer_timeout`, expired or restarted (their tickets
  and slots go), then tells each queue head that may enter, once per head,
  with a fresh view over the `queue` stream. The head's peer admits the
  player through its usual queue check (`queued_target`), which needs a view
  less than five seconds old; direct entries use the last view's slot counts
  and the local head when no fresh view exists. The consistency this gives
  is written up in [queues](queues.md#in-a-cluster).
- **Lengths.** `Extensions::queue_lengths` on the leader returns the
  network-wide lengths once it settled; elsewhere the lengths of the last
  view.
- **Admission limits** (`rate_limit`, `login_rate_limit`,
  `limits.max_connections` and the per-address cap) stay per peer; nothing
  in them assumed a single process.
- **Leases** (decision 8) are `lease` entries applied by a component of their
  own (`cluster_leases`, keyed by owner and name). A grant succeeds when the
  lease has no live holder: none, its time-to-live passed on the leader's
  entry clock, or its peer's incarnation expired or was replaced. The token
  is a per-lease counter that every grant raises and a release keeps, so it
  only grows, across leader changes and restarts. Asking again while
  holding extends and keeps the token; a renewal needs the current holder
  and token. Owners are `config` for the configuration script and
  `plugin:<name>` for each plugin. The lease API waits for the proposal on
  the caller's thread (plugin threads and the script's blocking workers, at
  most the callback's deadline); without a cluster it grants at once.
- **Job scopes.** `rift.every` in `rift.lua` and in Lua plugins, and
  `extensions.jobs`, take `scope = "cluster"` (the default) or `"peer"`.
  Every peer keeps its timers and checks `Cluster::is_leader_with_lease`
  before each tick of a `cluster` job. `rift.timers.*` and WebAssembly timers
  stay per peer: a scoped timer and the lease calls in `wit/rift-plugin.wit`
  would need a contract release with regenerated C, Go and Rust SDKs, which
  is left for later, like scoped subscriptions in step 5.
- **Plugins that are not cluster-aware** (decision 9): `plugin.toml` takes
  `cluster = true` (or `"aware"`). In a cluster, persistent plugins without
  it are flagged in `rift check` (from a `cluster.toml` beside the
  configuration or a `cluster` section), in the plugin status of `rift admin
  plugins list` and `GET /api/plugins` (`cluster_warning`) and in the
  dashboard; every plugin loads either way.

### Step 10 notes

Hardening: rolling restarts, backup and recovery, strict checks and a
cluster of real processes in CI.

Peers as nodes and forwarded leader-only requests, which this step's plan
also named, came with the second pass of step 7 (see
[step 7 notes (continued)](#step-7-notes-continued)).

- **Draining hands players over.** A peer drains on SIGTERM, Ctrl-C,
  `rift admin shutdown` or a leave request from `rift admin cluster leave`.
  Its listeners stop taking connections; it hands the leadership to another
  voter if it leads (`transfer-leader` picks a reachable, caught-up voter, a
  player-serving one before a witness). Then `Presence::hand_off` marks it
  draining, so its answer to other peers' presence probes now says it admits
  nobody, and picks transfer targets: every other peer that is up
  (its link is up), not expired, a voter or learner (not pending, not a
  witness), has `public_address` in its `cluster.toml`, and answers a
  presence probe saying that it admits logins (not fenced, not catching up,
  not draining). A peer of an earlier release, whose probe answer has no
  admission field, counts if this peer heard from it within half of
  `cluster.peer_timeout`. While some candidate has a public address but none
  qualifies yet, it probes again every 250 ms for up to five seconds. Every
  local player then gets a session action: a 1.20.5+ client (protocol 766
  or newer) in configuration or play gets the clientbound Transfer packet
  with a target's public address (host and port, IPv6 brackets stripped),
  the targets taken in turn so players spread over them; an older client, a
  client that cannot be transferred at that moment, and everyone when there
  is no target are disconnected with `cluster.leaving`. The peer logs
  `cluster: leaving: transferred N players to other peers and disconnected
  M`. Then sessions drain as on a single Rift (claims are released as they
  end, within `shutdown_timeout_ms`), the bus listener closes so bus clients
  reconnect to other peers, the embedded agent stops its servers (except
  kept named servers), and presence, the bus, the leader tasks and Raft
  stop.
- **The backend always sees a login.** A transferred client reconnects with
  handshake intent 3 (transfer). Rift accepts it from 1.20.5+ clients (and
  refuses it from older ones with `login.transfer_version`), as before. Rift
  now always sends the backend intent 2 (login), whatever the client sent,
  so backends need no `accepts-transfers=true`; previously only online-mode
  and forwarded logins were rewritten.
- **`rift cluster init --from-backup`** also clears what the old cluster's
  peers held while they ran: player name and UUID claims
  (`cluster_claims`), node session claims (`bus_node_sessions`), bus
  addresses (`bus_addresses`) and per-peer configuration reports
  (`config_peers`), besides the peer table, stored proposal results,
  features, checksums and the next Raft id. A claim of a peer that is no
  member would otherwise keep its player out. Since `rift.peer` is not set
  while init reads the configuration, `rift.peer` is nil there.
- **`rift admin cluster backup`** writes the copy after a linearizable read
  (`read_barrier`), with SQLite's online backup API from the replica, and
  switches the copy to `journal_mode=DELETE`, so it is one plain file
  without `-wal` or `-shm`. The file must not exist. The admin client makes
  the path absolute where it runs; the file is written on the host of the
  peer the admin endpoint belongs to.
- **`rift admin cluster status --leader`** returns the leader's status,
  forwarded over the peer link, since only the leader knows every peer's
  lag. `rift check --strict` on a follower uses it to report peers more than
  1,000 entries behind the leader, this peer included.
- **`rift check --strict`** warns when this peer's `cluster.toml` has no
  `public_address`: peers that drain cannot send players here, and when no
  other peer has one either, this peer's own players get the reconnect
  message when it drains.
- **CI.** `tests/cluster.rs` runs three real `rift` processes on loopback
  with certificates generated for the test. It initializes the first peer,
  adds and promotes the others, reads `GET /api/cluster`, checks that a
  permission change made on one peer reaches the others and that a
  configuration deployment reaches every peer's `rift.lua`. It kills the
  leader with SIGKILL and asserts that a new leader is elected within a
  bound, that earlier writes are kept and a new write is accepted, and that
  exactly one peer leads; it restarts the killed peer and waits for it to
  catch up. It logs in the same player on two peers at once and asserts
  that exactly one login is accepted. It takes a backup and starts a new
  cluster from it with `rift cluster init --from-backup` in a fresh
  directory. Finally it makes a peer leave while a 1.20.5 client and a 1.8
  client are connected to it, and asserts that the 1.20.5 client is
  transferred to another peer and the 1.8 client disconnected.

### Not done

- **WebAssembly plugins** keep `cluster` scope for every subscription, have
  no scoped timer (their timers tick on every peer) and cannot reach the
  leases. Each needs a contract release with regenerated C, Go and Rust
  SDKs.
- **No dashboard panel.** The design's Cluster panel with each peer's role,
  lag, players, load and configuration hash was not built; the dashboard
  shows only `cluster_warning` for plugins. `GET /api/cluster` and
  `rift admin cluster` serve the data.
- **`keep_running` in service groups.** Only named servers on a peer's own
  node keep running through a restart of their peer; `keep_running` in a
  service group with `nodes` is refused, in a cluster as elsewhere.
- **Forwarded moves.** A follower shows the progress only of moves it
  forwarded itself. A forwarded move whose leader changes fails on the
  follower, while the old leader's move continues or aborts there.
- **`rift admin backups`** (the list) runs on the peer asked, so its "last
  attempt" column shows only that peer's attempts.
- **The messaging section is shared.** `messaging.listen` and
  `messaging.advertise` cannot depend on `rift.peer`, so every peer records
  the same bus address for `RIFT_BUS_ADDRESSES`. A DNS name that resolves to
  every peer works around it; node agents take one `--controller` per peer
  regardless.
- **The scaler across a leader change** is covered by the simulation for one
  demand and one instance
  (`one_demand_makes_one_instance_across_a_leader_change`), not for scaling
  decisions over time, and not through the proxy's
  `create_remote_instance`.

## Decisions

Each question was decided on 2026-11-19. The chosen option comes first and is
marked **(chosen)**; the others are kept with their trade-offs as the record
of what was weighed. Reopening a decision means changing this section and
every part of the documentation that depends on it.

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
