# Rift messaging protocol, version 1

Remote server plugins reach Rift's [message bus](messaging.md) with this
protocol. Read it to write a client in a language that has none.

## Transport

- Remote clients use QUIC over UDP with TLS 1.3 and the exact ALPN
  `rift-messaging/1`.
- There is no plaintext fallback, certificate-verification bypass, or 0-RTT
  execution of commands. The listener is opt-in.
- This is a Rift protocol inspired by NATS and JetStream; NATS clients cannot
  connect directly.
- Native components and Lua plugins use the same broker in process, without
  QUIC.

A connection authenticates once, then carries independent bidirectional
streams. Keep a publisher stream open for high throughput; use a separate
stream per subscription or request. QUIC separates stream flow control, so a
stalled subscription does not block a request on another stream.

## Authentication and permissions

1. The first client-initiated bidirectional stream must contain exactly one
   AUTH frame.
2. The server replies OK and finishes that stream. Wait for this response
   before opening operation streams.

A failed authentication closes the connection. Tokens are sent only inside
the verified encrypted connection. Rust clients trust explicit CA/server
certificates and verify the configured server name.

### Subject patterns

Each configured identity has publish and subscribe subject patterns.

- `*` matches exactly one nonempty token; a terminal `>` matches one or more
  remaining tokens.
- Subscriptions must be contained within an allowed pattern: permission for
  `game.*` does not authorize `game.>` or `>`.
- Subscription rights for `game.>` do not grant rights for bare `game`.

### Reserved subjects

Besides a matching ACL, these subjects require:

| Subjects | To publish | To subscribe to an overlapping pattern |
| --- | --- | --- |
| `rift.control` | `control = true` | `control = true` |
| `rift.node.<n>` | Being the [node agent](nodes.md) authenticated as `node.<n>`; other identities may not publish to `rift.node` subjects | `control = true` |
| `rift.players` | `players = true` | `control = true`, because a subscriber would receive other callers' requests |
| `rift.network` | Nothing more | `control = true`, for the same reason |
| `rift.events` and its children | Always rejected for remote clients, because Rift owns those events | Nothing more |

Overlapping patterns include the bare subject, such as `rift.control`, and
broad subscriptions such as `>`.

### Reply subjects

- Explicit reply subjects must be literal `_INBOX.*` subjects covered by the
  requester's subscribe ACL.
- Responders need publish access to the inbox namespaces they answer.
- The REQUEST operation creates a private broker inbox itself, so a requester
  using REQUEST needs only publish access to the requested subject and no
  inbox subscribe grant.

> [!WARNING]
> Do not grant broad `_INBOX.>` subscribe permission to ordinary plugins:
> that grant exposes other identities' reply traffic, including
> control/status responses. For manual request/reply, grant a distinct
> namespace such as `_INBOX.paper.>` to its owner and use reply subjects
> within that namespace.

### Durable consumer names

Remote durable consumer names are scoped to the authenticated identity. Other
identities cannot fetch or acknowledge those consumers, even when they use
the same supplied consumer name. Changing a principal's identity name
therefore changes its durable consumer namespace.

## Framing

Integers are unsigned and big endian. Every frame is `u32 body_length`,
followed by exactly that many body bytes. `body_length` includes the one-byte
opcode and excludes the length prefix.

| Notation | Encoding |
| --- | --- |
| `str` | `u16 byte_length` followed by UTF-8 bytes. No terminating NUL is used |
| Absent reply subject or queue group | An empty string |
| `payload` | All remaining bytes in that frame, including zero bytes |

| Limit | Value |
| --- | --- |
| `body_length` | 1,114,112 bytes. Length zero and larger lengths are rejected before allocating the body |
| Authentication frame body | 4,099 bytes (opcode, token length and at most 4,096 token bytes) |
| Payload | 1,048,576 bytes, additionally subject to the broker/stream configured limit |
| Subject string | 1,024 UTF-8 bytes |
| Any encoded string | 4,096 UTF-8 bytes |
| REQUEST `timeout_ms` | 1–120,000 ms |
| CONSUMER_FETCH `limit` | 1–256 |
| Consumer `ack_wait_ms` | 1–3,600,000 ms |
| Consumer `max_ack_pending` | 1–65,536 pending acknowledgements |
| Names | 1–96 ASCII letters, digits, underscores, dashes or dots |

Unknown opcodes, invalid UTF-8, invalid subjects, truncated fields, and
unexpected trailing fields in fixed-layout frames terminate that operation
stream with ERROR when possible.

| Opcode | Name | Body following opcode | Server response |
| --- | --- | --- | --- |
| 1 | AUTH | token:str | OK; first stream only |
| 2 | OK | empty | server response |
| 3 | ERROR | code:u16, message:str | server response; code 1 permission, 2 operation/protocol |
| 4 | PUBLISH | subject:str, reply:str, payload | none on success; ERROR closes stream on failure |
| 5 | PUBLISH_ACK | subject:str, reply:str, payload | OK after live broker acceptance |
| 6 | SUBSCRIBE | pattern:str, queue:str | OK after registration, followed by MESSAGE frames |
| 7 | MESSAGE | subject:str, reply:str, payload | server delivery/response |
| 8 | PING | empty | OK after all preceding commands on this stream |
| 9 | REQUEST | timeout_ms:u32, subject:str, payload | one MESSAGE, or ERROR |
| 10 | STREAM_PUBLISH | stream:str, subject:str, reply:str, payload | STORED |
| 11 | STORED | sequence:u64, timestamp_millis:u64 | server response |
| 12 | CONSUMER_OPEN | stream:str, name:str, filter:str, start_sequence:u64, ack_wait_ms:u32, max_ack_pending:u32 | OK |
| 13 | CONSUMER_FETCH | stream:str, name:str, limit:u32 | zero or more DELIVERY frames, then OK |
| 14 | DELIVERY | sequence:u64, timestamp_millis:u64, delivery_count:u32, subject:str, reply:str, payload | server response |
| 15 | ACK | stream:str, name:str, sequence:u64 | OK |
| 16 | CONSUMER_DELETE | stream:str, name:str | OK |

## Operations

### Publish

- PUBLISH, PUBLISH_ACK and PING can be repeated on one ordered stream.
- A client must not wait for a response to PUBLISH; use PING as a barrier
  after a batch.
- A PUBLISH_ACK confirms live broker routing, not subscriber processing or
  disk persistence.
- Live delivery is at most once. Messages with no live subscriber are
  discarded.
- When a bounded subscriber queue fills, that subscriber is closed with an
  explicit error and other subscribers continue.
- Messages are ordered within a publisher stream; there is no ordering
  guarantee across different publisher streams.

### Subscribe and request

- SUBSCRIBE converts its stream into a delivery stream. The client retains
  both halves for its lifetime, then sends STOP_SENDING/reset on
  cancellation.
- Other operations use one stream per call; clients can finish their sending
  half after writing the command.
- A reply to REQUEST uses the MESSAGE layout with the generated inbox
  subject.

### Streams and consumers

- Durable stream writes are explicit: STREAM_PUBLISH does not also publish to
  the live broker. Unknown streams are rejected.
- STORED acknowledges the stream's configured durability boundary.
- Durable consumers deliver at least once: acknowledge only after processing.
- Reopen the same consumer with the same configuration to resume.
- One identity may hold at most a quarter of a stream's `max_consumers` (at
  least one); CONSUMER_OPEN for a new consumer beyond that is rejected.
- CONSUMER_DELETE removes one of the caller's own consumers with its position
  and pending deliveries, which frees its place.
- Fetch returns currently available messages without waiting.
- Unacknowledged deliveries become eligible for redelivery after ack wait.
- Retention may remove old records.

This standalone store does not implement distributed replication, consensus
or clustered failover.

## Limits and lifecycle

The server answers each new connection with a QUIC Retry before it spends any
connection slot on it, so packets with spoofed source addresses cannot fill
the limits below. Every compliant QUIC client follows a Retry automatically;
it adds one round trip to connection setup.

| Limit | Value |
| --- | --- |
| Authenticated connections | 1,024 by default |
| Authenticated connections per principal | 256 by default |
| Connections from one IPv4 address or IPv6 /64, at any stage | 256 by default |
| Concurrent operation streams per connection | 128 by default |
| TLS/authentication and a stream's first frame | Five-second deadline by default |
| Partial subsequent frames and blocked outgoing writes | 30-second deadline by default |
| Connections still handshaking or authenticating | Their own pool of 256, at most 16 per address |
| QUIC keepalive | Ten seconds |
| QUIC idle timeout | 60 seconds |

- The defaults are configurable through the server `AuthConfig`.
- Node agents (`node.*` principals) have one session each and count against
  neither the total nor the per-principal limit, so plugin connections can
  never lock out a node.
- A connection over a limit is refused before authentication, or receives an
  ERROR such as `too many connections for this identity` in reply to AUTH.
- Idle publisher streams stay usable.
- QUIC receive/send windows and broker subscriber queues are bounded. Slow
  network subscribers are disconnected rather than holding up local
  publishers.
- Dropping `Server` closes its endpoint and aborts its owned tasks.

## Rust client

The client lives in the `rift-messaging` crate, which backend plugins use
without depending on Rift itself; `rift::messaging::quic` re-exports it.

```rust,no_run
use bytes::Bytes;
use rift_messaging::{Client, tls_client_config};

# async fn example() -> std::io::Result<()> {
let tls = tls_client_config("certs/ca.pem")?;
let client = Client::connect(
    "127.0.0.1:7443".parse().unwrap(), "localhost", tls,
    &std::env::var("RIFT_PLUGIN_TOKEN").expect("token"),
).await?;
let mut scores = client.subscribe("game.score.*", None).await?;
let mut publisher = client.publisher().await?;
for score in 0..100u32 {
    publisher.publish("game.score.player42", None,
        Bytes::copy_from_slice(&score.to_be_bytes())).await?;
}
publisher.flush().await?;
let message = scores.recv().await?;
assert_eq!(&*message.subject, "game.score.player42");
# Ok(()) }
```

- The client applies 30-second operation deadlines; REQUEST uses its supplied
  timeout plus five seconds for transport.
- `RemoteSubscription::recv()` is cancellation safe.
- Cancelling a write can leave a partially written frame, so a `Publisher`
  marks itself unusable after any failed or cancelled operation; create a new
  publisher to continue.
- The shared `Client::publish` publisher opens a new stream on the next call
  after a failure; it never automatically retries the failed publication.
- TLS configuration follows the
  [Quinn certificate configuration](https://quinn-rs.github.io/quinn/quinn/certificate.html)
  model with explicit trust roots.

> [!WARNING]
> A failed publication may already have reached the broker: retries can
> duplicate messages. Applications that need idempotency must supply their
> own message identifiers.
