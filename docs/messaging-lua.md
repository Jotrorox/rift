# Messaging from Lua

Lua callbacks publish on the [message bus](messaging.md) with `rift.publish`
and receive messages in `on_message`. The local broker exists without a
network listener, so no QUIC setup is needed.

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

Set `messaging = {}` to customize local limits. Omit `messaging` or use
`false` for default local limits and no UDP listener.

## Lua API

| Function | Available in | What it does |
| --- | --- | --- |
| `rift.publish(subject, payload, optional_reply_subject)` | `on_route`, `on_http`, `on_message` | Publishes; returns a table containing `delivered` and `slow_consumers` |
| `on_message = function(message)` | Configuration | Receives each delivery of `messaging.subscriptions` |
| `rift.on("message", function(message) ... end)` | Local Lua modules and folder-based plugins | Registers the same callback; multiple handlers run in registration order. See the [Lua API](lua.md) and [modular example](../examples/modular/rift.lua) |
| `rift.players` | `on_message`, `on_http` | [Messages players](#message-players) |
| `rift.servers.create(group)`, `remove(name)`, `start(name)`, `stop(name)`, `download(template)` | Wherever `rift.publish` is | Submits a [control request](messaging-subjects.md#control-subjects) and returns its reply subject |

## Publish and receive

Subscriptions are persistent. Rift dispatches each delivery asynchronously
into `on_message`, with a fresh sandboxed VM.

- Subject tokens are separated by dots. Wildcards and queue groups work as
  described under [subjects and delivery](messaging.md#subjects-and-delivery).
- Payloads are binary Lua strings. Replies use the message's optional reply
  subject, `message.reply`.
- Publishing never waits for receivers or disk. Delivery counts describe
  enqueueing, not completed handler execution.
- In a [cluster](messaging.md#in-a-cluster), `messaging.subscriptions`
  have `"cluster"` scope: they hear user subjects published on any peer.
  `delivered` counts this peer's subscribers only. Persistent Lua plugins
  can choose `"local"` with `rift.subscribe(subject, { scope = "local" })`
  (see [Lua plugins](plugins-lua.md)).

## Message players

With [`rift.config.plugins`](players-api.md) set, `on_message`
and `on_http` can also use `rift.players`. For example, relay chat published
by another server to everyone on this one:

```lua
on_message = function(message)
    if message.subject == 'network.chat' then
        rift.players.broadcast(message.payload)
    end
end,
```

## Start and stop servers

`rift.servers` lets a plugin spin servers up and down:

```lua
messaging = { subscriptions = { { subject = 'rift.events.instances' } } },
on_http = function(request)
    if request.path == '/ext/games/new' then
        -- Returns the _INBOX subject that receives the result.
        return { status = 202, body = rift.servers.create('games') }
    end
end,
on_message = function(message)
    -- {"event":"created","name":"games-3","group":"games","port":25602,...}
    rift.publish('network.capacity', message.payload)
end,
```

- Each function returns immediately, without waiting for the server or the
  download.
- Each accepts an optional reply subject starting with `_INBOX.`; otherwise
  Rift generates one.
- Subscribe to the reply subject or to `rift.events.instances` to learn the
  outcome.
- Requests count toward the publication limit below.

## Limits and state

| Limit | Value |
| --- | --- |
| Callback execution | 50 ms, besides the existing instruction and memory limits |
| Publications per callback | 256 messages totaling 1 MiB |

- Message handlers wait for a message worker slot, while later deliveries
  stay in bounded subscription queues. HTTP and connection hooks keep their
  usual bounded worker admission.
- An overflowing subscription discards pending messages and re-subscribes.
  Messaging is at most once and does not replay them.
- Handler errors are logged and later messages continue.
- Use [retained streams](messaging.md#retained-streams) and explicit
  acknowledgements for recoverable work.
- Lua globals and upvalues do not persist between deliveries. Store shared
  state in an application consumer or retained stream.
- Refer to `rift` inside callbacks. Rift installs it after evaluating the
  configuration, so configuration validation cannot publish messages.
- A configuration reload updates `on_message` while retaining subscriptions
  and queued messages. Changing messaging settings requires a restart.

> [!TIP]
> Lua VM creation and sandbox checks add work per callback. For the lowest
> latency, use the Rust `Broker` directly from an async plugin.

## Authenticated QUIC configuration

Add these fields to `messaging` to accept external server plugins.
[Messaging](messaging.md#accept-server-plugins-over-quic) explains TLS,
principals and their grants.

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
    max_connections_per_principal = 256,
    max_connections_per_address = 256,
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

| Field | Counts |
| --- | --- |
| `max_connections` | Authenticated connections of the configured principals |
| `max_connections_per_principal` | One principal, so a leaked token cannot take every slot |
| `max_connections_per_address` | One IPv4 address or IPv6 /64, including connections that have not authenticated yet |

- Plugins on managed servers usually share the controller's address, so raise
  the per-principal and per-address limits above the number of servers whose
  plugins connect.
- Node agents never count against `max_connections` or the per-principal
  limit. See
  [limits and lifecycle](messaging-protocol.md#limits-and-lifecycle).
- A stream with `storage_path` stores records and durable consumer state on
  disk; omitting the path selects bounded memory retention.
- Stream publication is an explicit API, separate from `rift.publish`, which
  keeps local publish/subscribe free of disk waits. The
  [wire protocol](messaging-protocol.md) documents the QUIC operations.

## Embed in Rust

Rust embedders inject a broker with `Router::with_messaging` and
`HttpScript::execute_with_messaging`. `MessageHandler::new` subscribes
synchronously, `script_updates()` returns a watch sender for hot reload, and
`run()` drives bounded asynchronous workers. Dropping the run task closes its
subscriptions; already-running Lua jobs finish within their sandbox budget.
