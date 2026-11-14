# Shared plugin messaging

Rift has one subject broker shared by routing hooks, HTTP hooks, Lua message
handlers, and external server plugins over QUIC. Native Rust components can also
use `rift::messaging::Broker` directly. Payloads are arbitrary bytes, so plugins
can choose JSON, protobuf, or their existing binary format.

Existing Bukkit/Paper plugins that use the BungeeCord plugin channel can enable
[BungeeCord compatibility](bungeecord.md) for player queries, transfers,
`Forward` and chat over Minecraft connections, independently of this broker.

The API follows NATS-style subjects, queue groups and request/reply, with a
separate retained-stream API for replay and acknowledgements. It uses Rift's own
[documented wire protocol](messaging-protocol.md); existing NATS clients do not
speak this protocol. This is a single-process broker, without clustering,
replication, transactions, or exactly-once processing. See the official
[NATS concepts](https://docs.nats.io/nats-concepts/overview) and
[JetStream documentation](https://docs.nats.io/nats-concepts/jetstream) for those
systems' full semantics.

## Local Rust plugins

Pass clones of the same broker to components. Creating a new broker creates an
independent bus. `publish` does not await network, disk or subscriber processing;
`recv` and `request` are asynchronous. Cloned messages share payload allocations.

```rust
use bytes::Bytes;
use rift::messaging::Broker;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let broker = Broker::default();
let mut events = broker.subscribe("game.player.*", None)?;
let result = broker.publish("game.player.join", Bytes::from_static(b"hello"))?;
assert_eq!(result.delivered, 1);
assert_eq!(events.recv().await?.payload, "hello");
# Ok(())
# }
```

`*` matches one token; terminal `>` matches one or more tokens. Publishers use
literal subjects. Without a queue group every matching subscription receives a
copy. With a queue group only one matching member receives a given publication.
Drop a subscription to unregister it. Subscribe before publishing or declaring a
responder ready; core pub/sub has no history.

The broker bounds payload size, subject size, subscription count and subscriber
queue capacity. A full subscriber queue disconnects that subscriber and reports
`SlowConsumer`; a publish report counts slow consumers separately from successful
deliveries. Publishers do not wait for slow subscribers. Consumers that cannot
lose messages should use retained streams with explicit acknowledgement.

## QUIC server plugins

Enable the listener explicitly in Lua configuration. It uses UDP, independent of
Minecraft's TCP listener. Configure a certificate with the DNS/IP name clients
will verify, and load token secrets through environment variables. Keep one
connection and publisher open instead of reconnecting for each message.

```lua
messaging = {
    listen = "127.0.0.1:4222",
    certificate = "messaging-cert.pem",
    private_key = "messaging-key.pem",
    principals = {
        {
            name = "paper",
            token_env = "RIFT_PAPER_TOKEN",
            publish = { "game.>", "_INBOX.>" },
            subscribe = { "game.>", "rift.events.>" },
        },
        {
            name = "operator",
            token_env = "RIFT_OPERATOR_TOKEN",
            publish = { "rift.control.*" },
            subscribe = { "rift.events.>" },
            control = true,
        },
    },
}
```

Set this field as `rift.config.messaging = { ... }` in a script, or inside a
legacy returned configuration table. Managed servers can get their own logins
without being listed here: see [bus logins for
instances](managed-servers.md#bus-logins-for-instances), which also describes
`advertise` and `advertise_server_name`, the address and certificate name
handed to them. Tokens must contain
32–1024 bytes without control characters. An external principal needs explicit
publish/subscribe grants. The [player API](#player-api-subjects) additionally
requires `players = true`, and control subjects `control = true`;
external clients cannot forge `rift.events` publications. Certificate and storage
paths are relative to Rift's working directory. Messaging configuration changes,
including credentials and stream limits, require a restart. Lua callback source
can reload while connections, subscriptions and queued messages remain intact.

### Backend permission principals

A backend that replicates [permissions](permissions.md) needs the
`rift.perms.*` subjects of the [permission protocol](permissions-protocol.md).
Instead of listing them, give its principal the `backend` preset:

```lua
principals = {
    { name = "server.survival-1", token_env = "RIFT_SURVIVAL_1_TOKEN", preset = "backend" },
},
```

The preset adds publish `rift.perms.hello`, `rift.perms.subject.*`,
`rift.perms.groups.*`, `rift.perms.apply`, `rift.perms.register`,
`rift.perms.command` and `rift.perms.context`, plus the
[server state](#server-state) subject `rift.server.state`, and subscribe
`rift.perms.update`, `rift.perms.update.>`, `rift.events.permissions` and
`rift.perms.changes` (the retained change log), and publish
`rift.network.*`, the [network API](#network-api-subjects), to whatever
`publish` and `subscribe` the entry also lists. Name the principal `server.<backend>`: Rift
trusts it to speak for that backend's players and nobody else's.

Rift records which principal sent each remote message, so permission handlers
know the caller without trusting the payload. Whatever the grants say, only
Rift itself publishes `rift.perms.update`, `rift.perms.update.>` and
`rift.perms.changes`, and only principals with `control = true` may subscribe
to the request subjects (`rift.perms.hello`, `rift.perms.subject.*`, …), so a
backend can neither forge updates nor read another backend's requests.

```rust
use bytes::Bytes;
use rift::messaging::quic::{Client, tls_client_config};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let client = Client::connect(
    "127.0.0.1:4222".parse()?, "localhost",
    tls_client_config("messaging-cert.pem")?,
    &std::env::var("RIFT_PAPER_TOKEN")?,
).await?;
let mut messages = client.subscribe("game.player.*", None).await?;
let mut publisher = client.publisher().await?;
publisher.publish("game.player.join", None, Bytes::from_static(b"hello")).await?;
publisher.flush().await?; // Barrier: preceding publications reached the broker.
let message = messages.recv().await?;
assert_eq!(message.payload, "hello");
# Ok(())
# }
```

`Client::publish` waits for broker acceptance. `Publisher::publish` pipelines
unconfirmed writes for throughput; `flush` checks acceptance of earlier writes.
Neither means a live subscriber has finished processing. Each subscription and
request has its own QUIC stream. Other languages can implement the small binary
protocol using a QUIC library; there is no Java/JavaScript SDK in this change.

## Lua plugins

Use `rift.publish` in `on_route`, `on_http`, and `on_message`. Async subscriptions
live outside Lua VMs; callbacks run on bounded blocking workers using the existing
Lua sandbox, instruction budget and execution deadline. See the
[Lua configuration and callback reference](messaging-lua.md) for a complete
example, or start from [`examples/messaging.lua`](../examples/messaging.lua).
VM globals do not persist between callbacks. Rust/QUIC clients provide
the lower-overhead path when Lua VM creation is too expensive.

## Player API subjects

The `players` interface of the [plugin contract](plugins.md#host-api) is
also served on the bus, so a service such as a ban list can message, kick and
transfer players without being a plugin and without `control`. Give its login
`players = true` and publish grants for exactly the operations it needs:

```lua
principals = {
    {
        name = "relaxoban",
        token_env = "RIFT_RELAXOBAN_TOKEN",
        publish = { "rift.players.kick-json", "rift.players.message-json", "_INBOX.>" },
        subscribe = { "_INBOX.>", "rift.events.players" },
        players = true,
    },
}
```

Send requests to `rift.players.<operation>`; answers are
`{"ok":true,"data":...}` or `{"ok":false,"error":"..."}`, as for control
subjects. Payloads are JSON objects whose fields are the WIT parameters in
snake_case; `target` is a player name, a UUID or a connection id.

| Subject | JSON payload | `data` |
| --- | --- | --- |
| `rift.players.list` | `{}` or `{"server":"lobby"}` (a server or service group) | `[{"connection_id":7,"uuid":"…","name":"Alice","server":"lobby-1","protocol":774,"visible_to":null,"locale":"en_us"}]` |
| `rift.players.get` | `{"target":"Alice"}` | one player, or `null` |
| `rift.players.names` | `{}` or `{"prefix":"Al"}` | sorted names |
| `rift.players.message` | `{"target":"Alice","text":"Hello"}` | whether the player was online |
| `rift.players.action-bar` | `{"target":"Alice","text":"…"}` | whether the player was online |
| `rift.players.title` | `{"target":"Alice","title":"Welcome","subtitle":"…","fade_in":10,"stay":70,"fade_out":20}`; only `target` and one of `title`/`subtitle` are required | whether the player was online |
| `rift.players.kick` | `{"target":"Alice","reason":"Bye"}` | whether the player was online |
| `rift.players.transfer` | `{"target":"Alice","server":"survival"}` (passes `before_transfer`) | whether the player was online |
| `rift.players.broadcast` | `{"text":"…"}`, optionally `"server":"lobby"` | how many players received it |
| `rift.players.message-json`, `action-bar-json`, `kick-json`, `broadcast-json`, `title-json` | as above, with `text`, `reason`, `title` or `subtitle` a JSON text component | as above |

Plain text is 1..=1024 bytes and is never read as JSON. The `-json`
operations take a component of at most 32 KiB, either embedded
(`"reason":{"text":"Banned","color":"red"}`) or as a JSON string, exactly as
the WIT functions take it, and may be written as markup
(`"reason":{"mini":"<red>Banned"}`, see [styled text](text.md)); line breaks make a multi-line kick screen, and
hover and click events are spelled for each player's client as described
under [rich text](extensions.md#rich-text). Dialogs, tab lists and boss bars
stay plugin-only, because their clicks and lifetimes belong to an owner.

Only principals with `control = true` may subscribe to `rift.players.*`: a
subscriber would see every caller's actions and could answer in Rift's place.
Requests run concurrently (up to 64 at once), need an `_INBOX.` reply subject
like control requests, and publish no `rift.events.control` event. Every
request that acts on a player (all but `list`, `get` and `names`) is written
to the [operator audit log](http.md#authentication-and-access) as
`players.<operation>` with the player as the target, never the text, by
`bus:<login>`, before the reply is sent. Persistent plugins calling these subjects through their own
`bus` interface appear as `plugin:<name>`, the configuration script as
`rift.players`. Plugins normally use the `players` interface directly, which
is not audited, like their other host calls.

## Network API subjects

A backend asks which servers are up, where players are, and for a player to
be moved, on `rift.network.<operation>`. This is what the [Java network
API](network-java.md) speaks. It needs only a publish grant for the subject,
which the `backend` preset includes: no `players`, no `control`. Requests and
answers have the shape of the player API's, with an `_INBOX.` reply subject.

| Subject | JSON payload | `data` |
| --- | --- | --- |
| `rift.network.servers` | `{}` or `{"server":"lobby"}` (a server or service group, ignoring ASCII case) | `[{"name":"lobby-1","group":"lobby","state":"running","service_state":"LOBBY","properties":{"map":"Hub"},"players":12,"max_players":50}]`: servers that are up, sorted by name |
| `rift.network.players` | `{}` or `{"server":"lobby"}` | players as `rift.players.list` reports them |
| `rift.network.player` | `{"player":"Alice"}` (a name or a UUID) | one player, or `null` |
| `rift.network.connect` | `{"player":"Alice","server":"survival"}` or `{"player":"Alice","group":"bedwars"}` | whether the player was online and the transfer queued |

`servers` lists the servers the BungeeCord bridge's
[`RiftServers`](bungeecord.md#rift-requests) lists, with the same rules for
"up", the player count and `max_players` (`0` when Rift knows no limit);
`group` is `null` outside a service group. `state` is `draining` or
`running`, whatever the reason a server drains. What the server reported
about itself on [`rift.server.state`](#server-state) comes beside it:
`service_state` is its label or `null`, and `properties` its properties. An
unknown server or group is an error, a group without running instances an
empty list.

`connect` with `server` takes a server or a service group, like
`rift.players.transfer`; with `group` it takes a service group only, and Rift
picks the instance. The destination is checked first, so a wrong name is an
error even when the player is offline. The transfer passes access rules and
`before_transfer` like any other, and the reply does not wait for its
outcome.

Who may move whom depends on the caller. A login named `server.<name>` moves
players who are on `<name>`. A login with `players = true` moves anyone, as
it can through `rift.players.transfer`, and so do persistent plugins and the
configuration script. Anything else is refused with an error. Every `connect`
is written to the [operator audit log](http.md#authentication-and-access) as
`network.connect`, with `player -> destination` as the target; reads are not
audited. Only principals with `control = true` may subscribe to
`rift.network.*`, as for `rift.players.*`.

## Rift control subjects

Send a JSON request using `Client::request(subject, payload, timeout)`. Responses
are `{"ok":true,"data":...}` or `{"ok":false,"error":"..."}`. They use the same
handlers as Rift's existing administration service, even if that service's TCP
listener is disabled. The control bridge requires an `_INBOX.` reply subject;
request helpers create one automatically. Control requests are serialized.
Requests that change something (all but `status`, `servers`, `groups`,
`nodes`, `templates` and the reading `plugins` actions) are written to the
[operator audit log](http.md#authentication-and-access) as
`control.<command>` by the caller: `bus:<login>` for a bus login,
`plugin:<name>` for a persistent plugin's `servers` calls, and `rift.control`
for the configuration script.

| Subject | JSON payload | Result |
| --- | --- | --- |
| `rift.control.status` | `{}` | Connections, players, backends and maintenance state |
| `rift.control.maintenance` | `{"enabled":true}` | Enable/disable new login maintenance |
| `rift.control.drain` | `{"backend":"lobby","enabled":true}` | Enable/disable draining for a backend |
| `rift.control.transfer` | `{"connection_id":123,"backend":"lobby"}` | Transfer an online player with existing protocol/access checks |
| `rift.control.reload` | `{}` | Reload the selected configuration file |
| `rift.control.servers` | `{}` | Managed servers with lifecycle state, usage, group, template, storage and node |
| `rift.control.groups` | `{}` | Service groups with port ranges, templates, storage, scaling, node selection and instances |
| `rift.control.nodes` | `{}` | [Nodes](nodes.md) with reachability, address, labels, instances and allocated memory |
| `rift.control.templates` | `{}` | Templates with their server software and download cache state |
| `rift.control.create` | `{"group":"lobby"}` | Create an instance from a service group, downloading template software first when needed |
| `rift.control.remove` | `{"name":"lobby-1"}` | Remove an unused instance; disposable storage is deleted |
| `rift.control.start` | `{"name":"lobby-1"}` | Request a managed server start (accepted asynchronously) |
| `rift.control.stop` | `{"name":"lobby-1"}` | Request an unused managed server stop and pause automatic wake |
| `rift.control.download` | `{"template":"paper","refresh":false}` | Download (or with `refresh`, update) a template's server software |
| `rift.control.plugins` | `{"action":"list"}`, or `{"action":"status"\|"logs"\|"restart","name":"hello-lua"}` | [Plugins](plugins.md#operator-surface) with state, restarts and calls; one plugin; its last 200 log lines; or restart a persistent plugin (accepted asynchronously) |

`create`, `remove` and `download` reply when the operation finishes, which can
include a first-time download of up to 15 minutes; they run alongside other
control requests (at most 16 at once), while Rift still applies instance changes
one at a time. Use a request timeout long enough for downloads, or subscribe to
`rift.events.instances` instead of waiting. That subject carries the creation
or removal result with `"event":"created"` or `"event":"removed"` after each
committed instance change, including automatic scaling, so plugins can follow
capacity the way CloudNet plugins follow service lifecycle events.

`rift.events.control` reports command name and success, excluding status/player
details. `rift.events.reload` reports the active revision after successful reload.
`rift.events.players` reports players as they join, switch servers and quit:
`{"event":"join","connection_id":"7","uuid":"…","name":"Alice","server":"lobby","visible_to":null}`.
A `switch` also carries the `previous` server, and a `quit` names the last one.
`visible_to` is the node a viewer needs to see the player
([visibility](permissions.md#visibility)), or `null`; a `visibility` event
reports a change of it, with the old value in `previous_visible_to`.
`rift.players.list` and `get` include `visible_to` for every player, and
`locale`, the language the client reported (`null` until its settings arrive;
see [online players](extensions.md#online-players)).
`uuid` is lowercase 32-digit hex without hyphens. Events for one player arrive in order.
`rift.events.permissions` carries every committed [permission](permissions.md)
change, the same JSON as `rift.perms.update`.
`rift.events.plugins` reports persistent plugins starting, failing, restarting
and stopping, and operator restarts, as described in [plugins](plugins.md#operator-surface).
`rift.events.servers` reports changes to what backends report about
themselves (see [server state](#server-state)).
`rift.events.lifecycle` reports `ready` and `draining`. These are ephemeral events;
query status to get current state. A timed-out command may already have executed;
check status before retrying a mutation. No automatic mutation retry is performed.

## Server state

A backend reports what it is doing on `rift.server.state`: a state label such
as `LOBBY` or `INGAME`, string properties such as the map or the round, and
whether it accepts new players. A minigame server can mark itself `INGAME` and
stop new players reaching it when a round starts, then reopen when the round
ends, without an operator login.

```json
{"state":"INGAME","properties":{"map":"Desert","round":"3"},"draining":true}
```

Every field is optional and the ones left out stay as they are:

| Field | Meaning |
| --- | --- |
| `state` | A label of 1–32 characters of `A-Z`, `a-z`, `0-9`, `_` or `-`, kept as sent; `null` clears it. Rift's own words (`running`, `draining`, `starting`, `stopping`, `stopped`, `failed`, `unknown`, in any case) are refused |
| `properties` | String values merged into the server's properties; a `null` value removes that key. Keys are 1–64 characters of `A-Z`, `a-z`, `0-9`, `_`, `-` or `.`; values at most 256 bytes without control characters; at most 32 properties per server |
| `replace` | `true` drops every existing property before `properties` is applied |
| `draining` | `true` accepts no new players, as the operator `drain` command does; players already there stay. `false` withdraws it |
| `server` | The server to report for; see below |

The report needs no reply subject, so a plain publish is enough. With an
`_INBOX.` reply subject (any request helper) the answer is
`{"ok":true,"data":{"server":"game-1","state":"INGAME","properties":{…},"self_draining":true,"draining":true}}`,
where `draining` is the server's effective drain from every source, or
`{"ok":false,"error":"…"}`. Refused reports are logged either way.

**Who may report.** A login named `server.<name>` reports for the configured
backend `<name>` only; it may repeat its own name in `server` but not name
another. [Instance logins](managed-servers.md#bus-logins-for-instances) get
publish access to `rift.server.state` automatically, and the `backend` preset
adds it for configured principals. A `control = true` principal and in-process
publishers (Lua message scripts and plugins) must name the server in `server`.
Any other login is refused. Only control principals may subscribe to
`rift.server.state`, so nobody else can answer in Rift's place.

**How long it lasts.** A report belongs to one run of a server. A managed
server forgets its label, properties and drain each time it starts, so a game
server that crashed mid-round comes back accepting players; an instance
forgets them when it is removed. An unmanaged backend keeps its report until
it changes it or Rift restarts. Reports are not stored on disk.

**Drain.** A server's own drain adds to the other reasons a server drains (the
`draining` setting, the operator `drain` command, a stopped service group). An
operator's `drain <server> on|off`, from any surface, also withdraws the
server's own drain, so `drain off` always reopens a server unless its group is
stopped; the server can ask again with another report.

**Where it shows.** `rift.control.servers` (also through a plugin's
`servers.control`), `GET /api/servers` and `rift admin servers` include
`service_state` (the label or `null`) and `properties` for every managed
server, and `rift.control.status` for every backend, beside `draining`. The BungeeCord
[`RiftServers`](bungeecord.md#rift-requests) entry state is `draining`, else
the label, else `running`. Every change, including a report forgotten on
restart, is published on `rift.events.servers`:
`{"event":"state","server":"game-1","state":"INGAME","properties":{"map":"Desert"},"draining":true}`,
where `draining` is the server's own drain only. The event carries the whole
report, so subscribers can replace what they hold.

A Java server plugin with the bundled client:

```java
MessagingClient bus = MessagingClient.builder().fromEnvironment().build();
bus.publish("rift.server.state",
    "{\"state\":\"INGAME\",\"properties\":{\"map\":\"Desert\"},\"draining\":true}"
        .getBytes(StandardCharsets.UTF_8));
```

## Retained streams

Streams are separate from live publication to keep storage out of the hot path.
Only explicit `stream_publish` / `Stream::publish` calls append a retained record;
ordinary broker publications do not persist, and stream publications do not
automatically fan out to live subscriptions. Publish once to each API when both
behaviors are desired; those two operations are not an atomic transaction.

```lua
-- Inside messaging:
streams = {
    jobs = {
        subjects = { "game.jobs.>" },
        storage_path = "data/jobs", -- Omit for memory-only storage.
        max_messages = 100000,
        max_bytes = 67108864,
        max_payload_bytes = 65536,
    },
},
```

The QUIC client exposes `stream_publish`, `consumer`, `fetch`, `ack` and
`delete_consumer`. One principal may hold a quarter of a stream's
`max_consumers`; delete consumers that are no longer used.
`ConsumerConfig` selects a filter, starting sequence, acknowledgement deadline
and maximum in-flight records. Fetch returns sequence numbers and delivery counts;
acknowledge only after processing. Unacknowledged records become available again
after the deadline, and after reopening a file store. Remote durable names are
scoped to authenticated principal names. Reopen the same name with the same
configuration to resume. Use a distinct consumer per independent subscription or
share a name among workers in the same principal.

Rift declares one stream itself: `rift.perms.changes`, every committed
permission change in revision order (see the
[protocol](permissions-protocol.md#revisions-and-ordering)). It is file-backed
in the directory beside the instance database (`rift.perms-changes/` next to
`rift.sqlite3`), or in memory without a configuration file, and holds the
latest 100,000 changes. Read it with a durable consumer on stream
`rift.perms.changes` and filter `rift.perms.changes`; the `backend` preset
grants that.

Retention bounds apply regardless of unacknowledged messages: records evicted by
count/byte limits cannot be redelivered. File storage persists records and consumer
state outside Tokio traffic threads; with the default `sync_on_write = true`,
acknowledgements wait for file synchronization. Disabling synchronization permits
loss of acknowledged writes on power failure. Unix also synchronizes directory
metadata; Windows has weaker power-loss guarantees for compaction renames. Disk
latency is consequently part of durable operations. Memory streams survive
client reconnects but disappear when Rift exits. File streams support one writer
process per storage directory, without replication or host-failure redundancy.

## Measure latency and throughput

```sh
cargo test --locked --all-targets -- --test-threads=1
cargo run --locked --release --example messaging_bench -- 20000
```

The benchmark reports local publish/receive and warm QUIC loopback round-trip
p50/p95/p99 latency for 64-byte payloads, plus pipelined throughput. These figures
exclude connection/TLS setup and persistence and check delivered payloads.
Separate measurements cover memory stream publish/fetch/ack and synchronized file
publication in the OS temporary directory (whose storage may be memory-backed).
Run repeatedly on the target hardware under representative fanout and payloads.
The benchmark is informational, with no timing thresholds in CI. Network distance,
scheduling, TLS, fanout and disk synchronization cannot have zero latency.

To measure file persistence on a specific filesystem instead of a potentially
memory-backed OS temporary directory, point `TMPDIR` at a directory on that
filesystem. For example, from a source checkout on Unix:

```sh
mkdir -p target
TMPDIR="$PWD/target" cargo run --locked --release --example messaging_bench -- 20000
```

File publication includes disk synchronization and blocking-worker dispatch;
the core/QUIC numbers exclude persistence.
