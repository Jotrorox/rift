# Shared plugin messaging

Rift has one subject broker shared by routing hooks, HTTP hooks, Lua message
handlers, and external server plugins over QUIC. Native Rust components can also
use `rift::messaging::Broker` directly. Payloads are arbitrary bytes, so plugins
can choose JSON, protobuf, or their existing binary format.

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
legacy returned configuration table. Tokens must contain
32–1024 bytes without control characters. An external principal needs explicit
publish/subscribe grants. Control subjects additionally require `control = true`;
external clients cannot forge `rift.events` publications. Certificate and storage
paths are relative to Rift's working directory. Messaging configuration changes,
including credentials and stream limits, require a restart. Lua callback source
can reload while connections, subscriptions and queued messages remain intact.

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

## Rift control subjects

Send a JSON request using `Client::request(subject, payload, timeout)`. Responses
are `{"ok":true,"data":...}` or `{"ok":false,"error":"..."}`. They use the same
handlers as Rift's existing administration service, even if that service's TCP
listener is disabled. The control bridge requires an `_INBOX.` reply subject;
request helpers create one automatically. Control requests are serialized.

| Subject | JSON payload | Result |
| --- | --- | --- |
| `rift.control.status` | `{}` | Connections, players, backends and maintenance state |
| `rift.control.maintenance` | `{"enabled":true}` | Enable/disable new login maintenance |
| `rift.control.drain` | `{"backend":"lobby","enabled":true}` | Enable/disable draining for a backend |
| `rift.control.transfer` | `{"connection_id":123,"backend":"lobby"}` | Transfer an online player with existing protocol/access checks |
| `rift.control.reload` | `{}` | Reload the selected configuration file |

`rift.events.control` reports command name and success, excluding status/player
details. `rift.events.reload` reports the active revision after successful reload.
`rift.events.lifecycle` reports `ready` and `draining`. These are ephemeral events;
query status to get current state. A timed-out command may already have executed;
check status before retrying a mutation. No automatic mutation retry is performed.

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

The QUIC client exposes `stream_publish`, `consumer`, `fetch`, and `ack`.
`ConsumerConfig` selects a filter, starting sequence, acknowledgement deadline
and maximum in-flight records. Fetch returns sequence numbers and delivery counts;
acknowledge only after processing. Unacknowledged records become available again
after the deadline, and after reopening a file store. Remote durable names are
scoped to authenticated principal names. Reopen the same name with the same
configuration to resume. Use a distinct consumer per independent subscription or
share a name among workers in the same principal.

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
