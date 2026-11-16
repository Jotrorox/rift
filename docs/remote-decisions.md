# Remote decisions

A service with its own database, such as a ban system, can decide `login`,
`initial_server`, `before_transfer` and `kick` without running in Lua: this is
**remote mode**. Rift sends a request on the [message bus](messaging.md) and
waits asynchronously, so a slow responder holds no Lua worker and no callback
slot.

```lua
rift.config.plugins = {
    decisions = {
        login = {
            subject = "relaxoban.login", timeout_ms = 2000,
            on_timeout = "deny", on_no_responders = "deny",
            deny_message = "Ban status unavailable, please retry",
            max_in_flight = 256,
        },
        before_transfer = { subject = "network.transfer", timeout_ms = 300, on_timeout = "allow" },
    },
}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `subject` | required | Literal subject the request is sent to, outside `rift.*` and `_INBOX.*` |
| `timeout_ms` | `1000` | How long to wait for a reply, 1–10000 |
| `on_timeout` | `"deny"` | `"allow"` or `"deny"` when no reply arrives in time |
| `on_no_responders` | `"deny"` | Applied at once, without waiting for the timeout, when nobody subscribes to `subject` |
| `deny_message` | Rift's `decision.unavailable` [message](text.md#rifts-own-messages) | Shown to the player when a policy denies; a string or a chat component table |
| `max_in_flight` | `256` | Open requests for this hook, 1–65536. When it is full, `on_timeout` applies at once. This protects the proxy and the responder against login floods |

Decisions fail closed by default. They can change on reload; open requests keep
counting against the limit while it stays the same.

**Order.** Rift asks the bus last, only when the Lua hook and every persistent
plugin continue ([dispatch order](plugin-events.md#dispatch-order)). A hook
without Lua code or plugins goes straight to the bus.

**Request.** A JSON object with `hook`, `uuid` (lowercase 32-digit hex),
`name`, `connection_id` (decimal string), `peer_ip`, `hostname`, `protocol`,
`default_server`, `server`, `target` and `reason`. Fields a hook does not have
are `null`, as in the Lua context.

**Reply.** One of these JSON objects, with no other fields:

| Reply | Allowed for |
| --- | --- |
| `{"action":"continue"}` | every decided hook |
| `{"action":"deny","reason":"…"}` | every decided hook |
| `{"action":"server","server":"lobby-2"}` | `initial_server` and `kick` |

- A `reason` is a string of plain text or a JSON text component, an object or a
  list, such as `{"action":"deny","reason":{"text":"Banned","color":"red"}}`
  ([rich text](players-api.md#rich-text)).
- Replies are validated exactly like Lua results: strings must contain 1–1024
  bytes and a server must be configured. An invalid reply is an error, so login
  is rejected and a transfer target is skipped.
- A reply that arrives after the timeout is dropped. A timed-out request may
  still have been processed, so **decisions must not have side effects**.

**Responders.** Subscribe with a queue group so several instances share the
load and none is a single point of failure. A remote principal needs
`subscribe` on the decision subject and `publish` on `_INBOX.>` to reply:

```lua
principals = {
    {
        name = "relaxoban",
        token_env = "RIFT_RELAXOBAN_TOKEN",
        subscribe = { "relaxoban.login" },
        publish = { "_INBOX.>" },
    },
}
```

> [!WARNING]
> Do not run a login decider only on backends that Rift starts on demand. With
> no responder the first login is denied, so the lobby never starts and no
> responder ever appears. Run the decider independently of the backends, for
> example as a small standalone service using a messaging client.

[`rift.events.players`](messaging-subjects.md#control-subjects) reports joins,
server switches and quits, so services that only follow players need no Lua at
all.
