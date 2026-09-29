# Lua plugins on the shared message bus

Rift exposes the same broker to Rust plugins, Lua callbacks, and authenticated
QUIC clients. The local broker exists without a network listener. Set
`messaging = {}` to customize local limits; omit `messaging` or use `false` for
default local limits and no UDP listener.

A Lua plugin receives messages through persistent subscriptions. Rift dispatches
each delivery asynchronously into `on_message`, with a fresh sandboxed VM.
Payloads are binary Lua strings. Replies use the message's optional reply subject.

```lua
return {
    listeners = { public = '0.0.0.0:25565' },
    backends = { lobby = '127.0.0.1:25566' },
    routes = { public = 'lobby' },
    messaging = {
        subscription_capacity = 1024,
        max_payload_bytes = 1024 * 1024,
        subscriptions = {
            { subject = 'plugins.echo', queue = 'echo-workers' },
            { subject = 'rift.events.>' },
        },
    },
    on_message = function(message)
        if message.subject == 'plugins.echo' and message.reply then
            rift.publish(message.reply, message.payload)
        end
    end,
    on_route = function(connection)
        rift.publish('plugins.connections', connection.peer_ip)
        return nil
    end,
    on_http = function(request)
        if request.path == '/ext/notify' then
            local sent = rift.publish('plugins.notifications', request.body)
            return { status = 202, body = tostring(sent.delivered) }
        end
    end,
}
```

`rift.publish(subject, payload, optional_reply_subject)` is available inside
`on_route`, `on_http`, and `on_message`. It returns a table containing `delivered`
and `slow_consumers`. Delivery counts describe enqueueing, not completed handler
execution. Publishing never waits for receivers or disk. Subject tokens are
separated by dots; subscriptions support `*` for one token and a terminal `>` for
one or more remaining tokens. Queue subscribers divide matching deliveries;
ordinary subscribers each receive a copy.

Callbacks share their existing instruction, memory, and 50 ms execution limits.
A callback may publish at most 256 messages totaling 1 MiB. HTTP and connection
hooks retain their usual bounded worker admission, while message handlers wait
for a message worker slot with subsequent deliveries remaining in bounded
subscription queues. An overflowing subscription discards pending messages and
re-subscribes; at-most-once messaging does not replay those lost messages. Handler
errors are logged and later messages continue. Use retained streams and explicit
acknowledgements for recoverable work.

Lua globals and upvalues do not persist between deliveries. Store shared state in
an application consumer or retained stream. `rift` is installed after evaluating
the configuration and before invoking callbacks; refer to it inside callbacks.
This ensures configuration validation cannot publish messages. Configuration
reload updates `on_message` while retaining subscriptions and queued messages.
Changing messaging settings requires a restart.

For the lowest latency, use the public Rust `Broker` directly from an async
plugin. Lua VM creation and sandbox checks add work per callback. Neither API
promises zero latency; benchmark the complete workload and payload sizes.

## Authenticated QUIC configuration

Add these fields to `messaging` to accept external server plugins. Paths are
relative to Rift's working directory. The private key and certificate must be
PEM files. Each principal reads its token from an environment variable at startup.
Certificates are verified by clients; configure the matching trusted CA.

```lua
messaging = {
    listen = '127.0.0.1:4222',
    certificate = 'certs/server.pem',
    private_key = 'certs/server-key.pem',
    principals = {
        {
            name = 'paper',
            token_env = 'RIFT_PAPER_TOKEN',
            publish = { 'plugins.>', 'events.>', 'rift.control.status' },
            subscribe = { 'plugins.>', 'rift.events.>', '_INBOX.>' },
            control = true,
        },
    },
    max_connections = 1024,
    max_streams_per_connection = 128,
    handshake_timeout_ms = 5000,
    io_timeout_ms = 30000,
    streams = {
        events = {
            subjects = { 'events.>' },
            storage_path = 'data/message-events',
            max_messages = 100000,
            max_bytes = 64 * 1024 * 1024,
            max_payload_bytes = 1024 * 1024,
            max_consumers = 128,
            sync_on_write = true,
        },
    },
}
```

Publication and subscription ACLs are explicit and default to empty. Control
publication also requires `control = true`. Local Lua configuration is trusted
and uses the local broker directly. Remote clients cannot impersonate Rift's
`rift.events` publisher. Remote reply subjects must be authorized `_INBOX.*`
subjects. Keep publish and subscribe grants as narrow as the plugin needs.

A stream with `storage_path` stores records and durable consumer state on disk;
omitting the path selects bounded memory retention. Stream publication is an
explicit API separate from ordinary `rift.publish`, keeping local pub/sub free of
disk waits. The [wire protocol](messaging-protocol.md) documents the portable
QUIC operations for pub/sub, requests, stream publication, replay, and acknowledgements.

Rust embedders inject a broker with `Router::with_messaging` and
`HttpScript::execute_with_messaging`. `MessageHandler::new` subscribes
synchronously, `script_updates()` returns a watch sender for hot reload, and
`run()` drives bounded asynchronous workers. Dropping the run task closes its
subscriptions; already-running Lua jobs finish within their sandbox budget.

Local Lua modules and folder-based plugins can register this callback with
`rift.on("message", function(message) ... end)`. Multiple message handlers run
in registration order. See the [Lua API](lua.md) and
[modular example](../examples/modular/rift.lua).
