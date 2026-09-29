# Rift messaging protocol, version 1

Rift exposes the same subject broker to native components, Lua plugins, and remote
server plugins. Remote clients use QUIC over UDP with TLS 1.3 and the exact ALPN
`rift-messaging/1`. This is a Rift protocol inspired by NATS and JetStream; NATS
clients cannot connect directly. The listener is opt-in. There is no plaintext
fallback, certificate-verification bypass, or 0-RTT execution of commands.

A connection authenticates once, then carries independent bidirectional streams.
Keep a publisher stream open for high throughput; use a separate stream per
subscription or request. QUIC separates stream flow control so a stalled
subscription does not block a request on another stream. Congestion control,
scheduling, TLS and network distance still impose latency: there is no promise of
zero network latency. Local plugins call the in-process broker without QUIC.

## Authentication and permissions

The first client-initiated bidirectional stream must contain exactly one AUTH
frame. The server replies OK and finishes that stream. Wait for this response
before opening operation streams. A failed authentication closes the connection.
Tokens are sent only inside the verified encrypted connection. Rust clients trust
explicit CA/server certificates and verify the configured server name.

Each configured identity has publish and subscribe subject patterns. `*` matches
exactly one nonempty token; terminal `>` matches one or more remaining tokens.
Subscriptions must be contained within an allowed pattern: permission for
`game.*` does not authorize `game.>` or `>`. Subscription rights for `game.>` do
not grant rights for bare `game`.

Publishing or subscribing to any overlapping `rift.control` subject requires
both a matching ACL and `control = true`. This includes bare `rift.control` and
broad subscriptions such as `>`. Remote publishing to `rift.events` and its
children is always rejected because Rift owns those events. Explicit reply
subjects must be literal `_INBOX.*` subjects covered by the requester's subscribe
ACL. Responders need publish access to the inbox namespaces they answer. The
REQUEST operation creates a private broker inbox itself, so a requester using
REQUEST needs only publish access to the requested subject and no inbox
subscribe grant. Do not grant broad
`_INBOX.>` subscribe permission to ordinary plugins: that grant exposes other
identities' reply traffic, including control/status responses. For manual
request/reply, grant a distinct namespace such as `_INBOX.paper.>` to its owner
and use reply subjects within that namespace.

Remote durable consumer names are scoped to the authenticated identity. Other
identities cannot fetch or acknowledge those consumers, even when they use the
same supplied consumer name. Changing a principal's identity name therefore
changes its durable consumer namespace.

## Framing

Integers are unsigned and big endian. Every frame is `u32 body_length`, followed
by exactly that many body bytes. `body_length` includes the opcode and excludes
the length prefix. Length zero and lengths above 1,114,112 bytes are rejected
before allocating the body. Authentication frames have a separate 4,099-byte
body limit (opcode, token length and at most 4,096 token bytes). Payloads are
limited to 1,048,576 bytes, additionally subject to the broker/stream configured
limit. Subject strings are at most 1,024
UTF-8 bytes; all encoded strings are at most 4,096 UTF-8 bytes.

`str` means `u16 byte_length` followed by UTF-8 bytes. No terminating NUL is used.
An absent reply subject or queue group is encoded as an empty string. `payload`
means all remaining bytes in that frame, including zero bytes. Unknown opcodes,
invalid UTF-8, invalid subjects, truncated fields, and unexpected trailing fields
in fixed-layout frames terminate that operation stream with ERROR when possible.

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

PUBLISH, PUBLISH_ACK and PING can be repeated on one ordered stream. A client
must not wait for a response to PUBLISH; use PING as a barrier after a batch. A
PUBLISH_ACK confirms live broker routing, not subscriber processing or disk
persistence. When a bounded subscriber queue fills, that subscriber is closed
with an explicit error and other subscribers continue. Live delivery is at most
once. Messages with no live subscriber are discarded.

SUBSCRIBE converts its stream into a delivery stream. The client retains both
halves for its lifetime, then sends STOP_SENDING/reset on cancellation. Other
operations use one stream per call; clients can finish their sending half after
writing the command. Messages are ordered within a publisher stream; there is no
ordering guarantee across different publisher streams.

REQUEST timeout is 1–120,000 ms. A reply uses the MESSAGE layout with the generated
inbox subject. CONSUMER_FETCH limit is 1–256. Consumer ack wait is 1–3,600,000 ms,
with 1–65,536 pending acknowledgements. Names use 1–96 ASCII letters, digits,
underscores, dashes or dots. Unknown streams are rejected. Durable stream writes
are explicit: STREAM_PUBLISH does not also publish to the live broker.

STORED acknowledges the stream's configured durability boundary. Durable
consumers deliver at least once: acknowledge only after processing. Reopen the
same consumer with the same configuration to resume. Fetch returns currently
available messages without waiting; unacknowledged deliveries become eligible
for redelivery after ack wait. Retention may remove old records. This standalone
store does not implement distributed replication, consensus or clustered
failover.

## Limits and lifecycle

The default server allows 1,024 connections and 128 concurrent operation streams
per connection. TLS/authentication and a stream's first frame have a five-second
deadline. Partial subsequent frames and blocked outgoing writes have a
30-second deadline. Idle publisher streams stay usable; QUIC uses ten-second
keepalives and a 60-second idle timeout. Limits are configurable through the
server AuthConfig. QUIC receive/send windows and broker subscriber queues are
bounded. Slow network subscribers are disconnected rather than holding up local
publishers. Dropping Server closes its endpoint and aborts its owned tasks.

The Rust client applies 30-second operation deadlines; REQUEST uses its supplied
timeout plus five seconds for transport. `RemoteSubscription::recv()` is
cancellation safe. Cancelling a write can leave a partially written frame, so a
Publisher marks itself unusable after any failed or cancelled operation; create
a new publisher to continue. The shared `Client::publish` publisher opens a new stream on the next call
after a failure; it never automatically retries the failed publication. A failed publication may already have reached the broker: retries can
duplicate messages. Applications that need idempotency must supply their own
message identifiers.

## Rust example

```rust,no_run
use bytes::Bytes;
use rift::messaging::quic::{Client, tls_client_config};

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

The transport uses Quinn's [owned Bytes chunk writes](https://docs.rs/quinn/0.11.12/quinn/struct.SendStream.html#method.write_all_chunks)
to avoid copying payloads into an additional application frame buffer. On
receive, a frame contained in one owned Quinn chunk is passed through without
copying its body; fragmented frames are assembled into a bounded buffer. TLS
configuration follows the [Quinn certificate configuration](https://quinn-rs.github.io/quinn/quinn/certificate.html)
model with explicit trust roots.
