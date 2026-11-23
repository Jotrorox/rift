# Messaging

Rift has one message bus: a subject broker shared by routing hooks, HTTP
hooks, Lua message handlers, Rust components and external server plugins.
Payloads are arbitrary bytes, so plugins can use JSON, protobuf or their own
binary format.

## Who can use the bus

| Client | Connects with | Read |
| --- | --- | --- |
| Rust code inside Rift | `rift::messaging::Broker` | [Use the bus in process](#use-the-bus-in-process) |
| Server plugins in any language | QUIC | [Accept server plugins over QUIC](#accept-server-plugins-over-quic) |
| Java servers and services | QUIC, the `rift-messaging` client | [Java clients](#java-clients) |
| Lua callbacks | `rift.publish`, `on_message` | [Messaging from Lua](messaging-lua.md) |

The subjects Rift itself serves are the
[built-in bus subjects](messaging-subjects.md).

A Paper plugin that only needs servers, players and messages should use the
[network API](network-java.md) instead of shading a bus client. Existing
plugins that use the BungeeCord plugin channel can enable
[BungeeCord compatibility](bungeecord.md), which works over Minecraft
connections, independently of this broker.

## Subjects and delivery

The bus follows NATS-style subjects, queue groups and request/reply.

- `*` matches one token; a terminal `>` matches one or more tokens.
  Publishers use literal subjects.
- Without a queue group, every matching subscription receives a copy. With
  one, only one matching member receives a given publication.
- Core publish/subscribe has no history. Subscribe before publishing or
  declaring a responder ready.
- Publishers do not wait for slow subscribers. A full subscriber queue
  disconnects that subscriber and reports `SlowConsumer`; a publish report
  counts slow consumers separately from successful deliveries.
- The broker bounds payload size, subject size, subscription count and
  subscriber queue capacity.
- Consumers that cannot lose messages should use
  [retained streams](#retained-streams) with explicit acknowledgement.

> [!NOTE]
> Rift uses its own [wire protocol](messaging-protocol.md). Existing NATS
> clients do not speak it.

The broker runs in one process, without replication, transactions or
exactly-once processing. For those systems' full semantics, see the official
[NATS concepts](https://docs.nats.io/nats-concepts/overview) and
[JetStream documentation](https://docs.nats.io/nats-concepts/jetstream).

### In a cluster

The peers of a [cluster](clustering.md) each run a broker, and the brokers
federate: a message reaches every subscriber in the cluster that should
see it, forwarded at most once to each peer. Which subjects cross peers
depends on their [scope](clustering.md#subject-scopes): Rift's request
subjects are answered by the peer a client is connected to, events about
replicated state come from each subscriber's own peer, session events come
from the peer where they happen (Lua and plugins see only their own
peer's, so plugin code sees each event once), and user subjects cross
peers. A subscription to a user subject can ask for `"local"` scope to hear
only its own peer; the default is `"cluster"`. A queue group delivers each
message to one member in the whole cluster, preferring a member on the
publisher's peer. See [the protocol](messaging-protocol.md#clusters) for
what clients notice.

## Use the bus in process

Pass clones of the same `Broker` to your components. A new broker is an
independent bus.

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

`publish` does not await network, disk or subscriber processing; `recv` and
`request` are asynchronous. Cloned messages share payload allocations. Drop a
subscription to unregister it.

## Accept server plugins over QUIC

The listener is off until you configure it. It uses UDP, independent of
Minecraft's TCP listener.

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

Set this as `rift.config.messaging = { ... }` in a script, or inside a legacy
returned configuration table.

- **TLS.** `certificate` and `private_key` are PEM files. The certificate
  must carry the DNS or IP name clients verify, and clients need the matching
  trusted CA.
- **Paths.** Certificate and storage paths are relative to Rift's working
  directory.
- **Limits.** Connection limits and timeouts are under
  [authenticated QUIC configuration](messaging-lua.md#authenticated-quic-configuration).
- **Managed servers** can get their own logins without being listed here. See
  [bus logins for instances](bus-logins.md),
  which also describes `advertise` and `advertise_server_name`, the address
  and certificate name handed to them.

> [!WARNING]
> Messaging configuration changes, including credentials and stream limits,
> require a restart. Lua callback source can reload while connections,
> subscriptions and queued messages remain intact.

## Principals and permissions

Each entry of `principals` is one login for external clients:

| Field | Meaning |
| --- | --- |
| `name` | The login name |
| `token_env` | Environment variable holding the token, read at startup. A token is 32–1024 bytes without control characters |
| `publish`, `subscribe` | Subject patterns the login may publish or subscribe to. Empty by default |
| `control` | `true` is needed, besides a grant, for the [control subjects](messaging-subjects.md#control-subjects) |
| `players` | `true` is needed, besides a grant, to publish on the [player API](messaging-subjects.md#player-api) |
| `preset` | `"backend"` adds the grants of a [backend permission principal](#backend-permission-principals) |

Keep grants as narrow as the plugin needs. Whatever the grants say:

- External clients cannot publish on `rift.events`.
- Reply subjects of remote clients must be authorized `_INBOX.*` subjects.
- Rift records which principal sent each remote message, so handlers know the
  caller without trusting the payload.

Local Lua configuration is trusted and uses the local broker directly. The
exact matching rules are in the
[protocol](messaging-protocol.md#authentication-and-permissions).

### Backend permission principals

A backend that replicates [permissions](permissions.md) needs the
`rift.perms.*` subjects of the [permission protocol](permissions-protocol.md).
Instead of listing them, give its principal the `backend` preset:

```lua
principals = {
    { name = "server.survival-1", token_env = "RIFT_SURVIVAL_1_TOKEN", preset = "backend" },
},
```

Name the principal `server.<backend>`: Rift trusts it to speak for that
backend's players and nobody else's. The preset adds these grants to whatever
`publish` and `subscribe` the entry also lists:

| Grant | Subjects |
| --- | --- |
| Publish | `rift.perms.hello`, `rift.perms.subject.*`, `rift.perms.groups.*`, `rift.perms.apply`, `rift.perms.register`, `rift.perms.command`, `rift.perms.context`, [`rift.server.state`](messaging-subjects.md#server-state) and `rift.network.*` (the [network API](messaging-subjects.md#network-api)) |
| Subscribe | `rift.perms.update`, `rift.perms.update.>`, `rift.events.permissions` and `rift.perms.changes` (the retained change log) |

A backend can neither forge updates nor read another backend's requests:

- Only Rift itself publishes `rift.perms.update`, `rift.perms.update.>` and
  `rift.perms.changes`.
- Only principals with `control = true` may subscribe to the request subjects
  (`rift.perms.hello`, `rift.perms.subject.*`, …).

## Rust clients

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

- Keep one connection and publisher open instead of reconnecting for each
  message.
- `Client::publish` waits for broker acceptance. `Publisher::publish`
  pipelines unconfirmed writes for throughput; `flush` checks acceptance of
  earlier writes. Neither means a live subscriber has finished processing.
- Each subscription and request has its own QUIC stream.

Other languages can implement the small
[binary protocol](messaging-protocol.md) using a QUIC library.

## Java clients

`rift-messaging` is the same client for the JVM (Java 21 or newer), from the
Maven repository described under [artifacts](permissions-java.md#artifacts):

```kotlin
dependencies {
    implementation("io.github.jotrorox.rift:rift-messaging:0.2.0")
    implementation("io.github.jotrorox.rift:rift-control:0.2.0") // RiftControl, RiftEvents
}
```

```java
MessagingClient bus = MessagingClient.builder()
        .address("127.0.0.1", 4222).serverName("localhost")
        .trust(Path.of("messaging-cert.pem"))
        .token(System.getenv("RIFT_PAPER_TOKEN"))
        .build();   // or builder().fromEnvironment() on a server Rift manages
bus.connected().join();
bus.subscribe("game.player.*", null, message -> System.out.println(message.text()));
bus.publish("game.player.join", "hello".getBytes(StandardCharsets.UTF_8));
```

- The client connects in the background, reconnects with backoff and
  re-establishes its subscriptions.
- For a cluster, add every peer with `addAddress(host, port)` (or
  `addAddress(host, port, serverName)`); `fromEnvironment()` reads them
  from `RIFT_BUS_ADDRESSES`. After losing a connection the client moves on
  to the next address at once.
- `subscribe(pattern, queue, SubscriptionScope.LOCAL, handler)` hears only
  the peer the client is connected to.
- Every operation returns a `CompletableFuture` that completes on a client
  thread, never on a server thread.
- Publications and requests fail fast while disconnected and are never
  retried.

`rift-control` adds typed access to the
[control subjects and events](messaging-subjects.md#java-control-and-events),
and the client itself reads and writes [retained streams](#java-streams).

## Lua

Use `rift.publish` in `on_route`, `on_http` and `on_message`. See
[messaging from Lua](messaging-lua.md), or start from
[`examples/messaging.lua`](../examples/messaging.lua). Rust and QUIC clients
are the lower-overhead path when Lua VM creation is too expensive.

## Retained streams

A stream keeps records for replay and acknowledgement. Streams are separate
from live publication, to keep storage out of the hot path:

- Only explicit `stream_publish` / `Stream::publish` calls append a retained
  record. Ordinary publications do not persist.
- Stream publications do not fan out to live subscriptions. Publish once to
  each API when you want both; the two operations are not an atomic
  transaction.

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
`delete_consumer`:

- `ConsumerConfig` selects a filter, starting sequence, acknowledgement
  deadline and maximum in-flight records.
- Fetch returns sequence numbers and delivery counts. Acknowledge only after
  processing.
- Unacknowledged records become available again after the deadline, and after
  reopening a file store.
- Durable names are scoped to the authenticated principal. Reopen the same
  name with the same configuration to resume.
- Use a distinct consumer per independent subscription, or share a name among
  workers in the same principal.
- One principal may hold a quarter of a stream's `max_consumers`. Delete
  consumers that are no longer used.

### Storage and durability

- Memory streams survive client reconnects but disappear when Rift exits.
- File streams persist records and consumer state. They support one writer
  process per storage directory, without replication or host-failure
  redundancy.
- Retention bounds apply regardless of unacknowledged messages: records
  evicted by the count or byte limits cannot be redelivered.
- With the default `sync_on_write = true`, acknowledgements wait for file
  synchronization, so disk latency is part of durable operations. Disabling
  it permits loss of acknowledged writes on power failure.
- Unix also synchronizes directory metadata; Windows has weaker power-loss
  guarantees for compaction renames.

### The permission change stream

Rift declares one stream itself: `rift.perms.changes`, every committed
permission change in revision order (see the
[protocol](permissions-protocol.md#revisions-and-ordering)).

- It is file-backed in the directory beside the instance database
  (`rift.perms-changes/` next to `rift.sqlite3`), or in memory without a
  configuration file.
- It holds the latest 100,000 changes.
- Read it with a durable consumer on stream `rift.perms.changes` and filter
  `rift.perms.changes`; the `backend` preset grants that.

### Java: streams

The [Java client](#java-clients) has the same operations:

```java
bus.streamPublish("jobs", "game.jobs.build", payload)
        .thenAccept(stored -> log("stored as " + stored.sequence()));

ConsumerConfig config = ConsumerConfig.builder("game.jobs.>")
        .ackWait(Duration.ofSeconds(60))
        .maxAckPending(32)
        .build();
StreamConsumer worker = bus.consumer("jobs", "builder", config).join();
for (Delivery job : worker.fetch(16).join()) {
    build(job.message().payload());   // job.isRedelivery() when it was fetched before
    worker.ack(job).join();           // only after processing
}
worker.delete();                      // when the consumer is no longer needed
```

- `fetch` returns what is available now, up to 256 records, and does not wait
  for new ones, so a worker polls.
- A `StreamConsumer` stays usable across reconnects because Rift holds its
  position. Calls fail fast while disconnected.
- Rift refuses with a `MessagingException`: an unknown stream, a subject the
  stream does not store, or the same consumer name reopened with different
  settings.

## Measure latency and throughput

```sh
cargo test --locked --all-targets -- --test-threads=1
cargo run --locked --release --example messaging_bench -- 20000
```

The benchmark checks delivered payloads and reports:

- local publish/receive and warm QUIC loopback round-trip latency (p50, p95,
  p99) for 64-byte payloads, plus pipelined throughput, excluding connection
  and TLS setup and persistence;
- memory stream publish/fetch/ack;
- synchronized file publication in the OS temporary directory, including disk
  synchronization and blocking-worker dispatch.

It is informational, with no timing thresholds in CI. Run it repeatedly on the
target hardware under representative fanout and payloads.

The OS temporary directory may be memory-backed. To measure a specific
filesystem, point `TMPDIR` at a directory on it, for example from a source
checkout on Unix:

```sh
mkdir -p target
TMPDIR="$PWD/target" cargo run --locked --release --example messaging_bench -- 20000
```
