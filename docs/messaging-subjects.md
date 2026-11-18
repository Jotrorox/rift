# Built-in bus subjects

Rift itself serves these subjects of the [message bus](messaging.md): the
[player API](#player-api), the [network API](#network-api), the
[control subjects](#control-subjects), [events](#events) and
[server state](#server-state). Look up a subject here for its JSON payload
and the grants a login needs.

## Requests and replies

- A request is a JSON object with an `_INBOX.` reply subject. Request helpers
  such as `Client::request(subject, payload, timeout)` create one
  automatically.
- A reply is `{"ok":true,"data":...}` or `{"ok":false,"error":"..."}`.
- Every subject needs a publish grant; the "Needs" columns list what else.
- Only principals with `control = true` may subscribe to the request
  subjects: a subscriber would see every caller's actions and could answer in
  Rift's place.

> [!WARNING]
> A timed-out command may already have executed. Check status before retrying
> a mutation. Rift performs no automatic mutation retry.

## Player API

The `players` interface of the [plugin contract](plugins.md#host-api) is also
served on the bus, so a service such as a ban list can message, kick and
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

Send requests to `rift.players.<operation>`. The fields are the WIT
parameters in snake_case; `target` is a player name, a UUID or a connection
id.

| Subject | Request | Reply `data` | Needs |
| --- | --- | --- | --- |
| `rift.players.list` | `{}` or `{"server":"lobby"}` (a server or service group) | `[{"connection_id":7,"uuid":"…","name":"Alice","server":"lobby-1","protocol":774,"visible_to":null,"locale":"en_us"}]` | `players` |
| `rift.players.get` | `{"target":"Alice"}` | one player, or `null` | `players` |
| `rift.players.names` | `{}` or `{"prefix":"Al"}` | sorted names | `players` |
| `rift.players.message` | `{"target":"Alice","text":"Hello"}` | whether the player was online | `players` |
| `rift.players.action-bar` | `{"target":"Alice","text":"…"}` | whether the player was online | `players` |
| `rift.players.title` | `{"target":"Alice","title":"Welcome","subtitle":"…","fade_in":10,"stay":70,"fade_out":20}`; only `target` and one of `title`/`subtitle` are required | whether the player was online | `players` |
| `rift.players.kick` | `{"target":"Alice","reason":"Bye"}` | whether the player was online | `players` |
| `rift.players.transfer` | `{"target":"Alice","server":"survival"}` (passes `before_transfer`) | whether the player was online | `players` |
| `rift.players.broadcast` | `{"text":"…"}`, optionally `"server":"lobby"` | how many players received it | `players` |
| `rift.players.message-json`, `action-bar-json`, `kick-json`, `broadcast-json`, `title-json` | as above, with `text`, `reason`, `title` or `subtitle` a JSON text component | as above | `players` |

- `visible_to` is the node a viewer needs to see the player
  ([visibility](permissions.md#visibility)), or `null`.
- `locale` is the language the client reported, `null` until its settings
  arrive (see [online players](players-api.md)).
- Requests run concurrently, up to 64 at once, and publish no
  `rift.events.control` event.

### Text

| Kind | Rules |
| --- | --- |
| Plain text | 1..=1024 bytes, never read as JSON |
| `-json` component | At most 32 KiB, embedded (`"reason":{"text":"Banned","color":"red"}`) or as a JSON string, exactly as the WIT functions take it |

A component may be written as markup (`"reason":{"mini":"<red>Banned"}`, see
[styled text](text.md)). Line breaks make a multi-line kick screen. Hover and
click events are spelled for each player's client, as described under
[rich text](players-api.md#rich-text).

Dialogs, tab lists and boss bars stay plugin-only, because their clicks and
lifetimes belong to an owner.

## Network API

A backend asks which servers are up, where players are, and for a player to
be moved, on `rift.network.<operation>`. This is what the
[Java network API](network-java.md) speaks. It needs only a publish grant,
which the `backend` preset includes: no `players`, no `control`.

| Subject | Request | Reply `data` | Needs |
| --- | --- | --- | --- |
| `rift.network.servers` | `{}` or `{"server":"lobby"}` (a server or service group, ignoring ASCII case) | `[{"name":"lobby-1","group":"lobby","state":"running","service_state":"LOBBY","properties":{"map":"Hub"},"players":12,"max_players":50}]`: servers that are up, sorted by name | grant only |
| `rift.network.players` | `{}` or `{"server":"lobby"}` | players as `rift.players.list` reports them | grant only |
| `rift.network.player` | `{"player":"Alice"}` (a name or a UUID) | one player, or `null` | grant only |
| `rift.network.connect` | `{"player":"Alice","server":"survival"}` or `{"player":"Alice","group":"bedwars"}` | whether the player was online and the transfer queued | [depends on the caller](#who-may-move-whom) |

### Server fields

`servers` lists the servers the BungeeCord bridge's
[`RiftServers`](bungeecord.md#rift-requests) lists, with the same rules for
"up", the player count and `max_players`.

| Field | Meaning |
| --- | --- |
| `group` | `null` outside a service group |
| `state` | `draining` or `running`, whatever the reason a server drains |
| `service_state` | The label the server reported on [`rift.server.state`](#server-state), or `null` |
| `properties` | The properties it reported there |
| `max_players` | `0` when Rift knows no limit |

An unknown server or group is an error, a group without running instances an
empty list.

### Who may move whom

`connect` with `server` takes a server or a service group, like
`rift.players.transfer`. With `group` it takes a service group only, and Rift
picks the instance.

| Caller | May move |
| --- | --- |
| A login named `server.<name>` | Players who are on `<name>` |
| A login with `players = true` | Anyone, as through `rift.players.transfer` |
| Persistent plugins and the configuration script | Anyone |
| Anything else | Nobody; refused with an error |

- Rift checks the destination first, so a wrong name is an error even when
  the player is offline.
- The transfer passes access rules and `before_transfer` like any other. The
  reply does not wait for its outcome.

## Control subjects

A login needs `control = true` and a publish grant. Requests use the same
handlers as Rift's administration service, even if that service's TCP
listener is disabled.

| Subject | Request | Result | Needs |
| --- | --- | --- | --- |
| `rift.control.status` | `{}` | Connections, players, backends and maintenance state | `control` |
| `rift.control.maintenance` | `{"enabled":true}` | Enable/disable new login maintenance | `control` |
| `rift.control.drain` | `{"backend":"lobby","enabled":true}` | Enable/disable draining for a backend | `control` |
| `rift.control.transfer` | `{"connection_id":123,"backend":"lobby"}` | Transfer an online player with existing protocol/access checks | `control` |
| `rift.control.reload` | `{}` | Reload the selected configuration file | `control` |
| `rift.control.servers` | `{}` | Managed servers with lifecycle state, usage, group, template, storage and node | `control` |
| `rift.control.groups` | `{}` | Service groups with port ranges, templates, storage, scaling, node selection and instances | `control` |
| `rift.control.nodes` | `{}` | [Nodes](nodes.md) with reachability, address, labels, instances, allocated memory and measured host resources | `control` |
| `rift.control.templates` | `{}` | Templates with their server software and download cache state | `control` |
| `rift.control.create` | `{"group":"lobby"}` | Create an instance from a service group, downloading template software first when needed | `control` |
| `rift.control.remove` | `{"name":"lobby-1"}` | Remove an unused instance; disposable storage is deleted | `control` |
| `rift.control.start` | `{"name":"lobby-1"}` | Request a managed server start (accepted asynchronously) | `control` |
| `rift.control.stop` | `{"name":"lobby-1"}` | Request an unused managed server stop and pause automatic wake | `control` |
| `rift.control.download` | `{"template":"paper","refresh":false}` | Download (or with `refresh`, update) a template's server software | `control` |
| `rift.control.plugins` | `{"action":"list"}`, or `{"action":"status"\|"logs"\|"restart","name":"hello-lua"}` | [Plugins](plugin-operations.md) with state, restarts and calls; one plugin; its last 200 log lines; or restart a persistent plugin (accepted asynchronously) | `control` |

Control requests are serialized, except `create`, `remove` and `download`:

- These three run alongside other control requests, at most 16 at once. Rift
  still applies instance changes one at a time.
- They reply when the operation finishes, which can include a first-time
  download of up to 15 minutes. Use a request timeout long enough for
  downloads, or subscribe to `rift.events.instances` instead of waiting.

## Audit log

Requests that change something are written to the
[operator audit log](http.md#authentication-and-access):

| Family | Audited | Logged as |
| --- | --- | --- |
| Player API | All but `list`, `get` and `names` | `players.<operation>` with the player as the target, never the text, before the reply is sent |
| Network API | `connect`; reads are not audited | `network.connect`, with `player -> destination` as the target |
| Control | All but `status`, `servers`, `groups`, `nodes`, `templates` and the reading `plugins` actions | `control.<command>` |

The actor is `bus:<login>` for a bus login and `plugin:<name>` for a
persistent plugin (its `bus` interface, or its `servers` calls). The
configuration script appears as `rift.players` or `rift.control`.

Plugins normally use the `players` interface directly, which is not audited,
like their other host calls.

## Events

| Subject | Reports |
| --- | --- |
| `rift.events.control` | Command name and success, excluding status/player details |
| `rift.events.reload` | The active revision after a successful reload |
| `rift.events.players` | Players as they join, switch servers and quit |
| `rift.events.instances` | The creation, removal or move result, with `"event":"created"`, `"removed"` or `"moved"`, after each committed instance change, including automatic scaling; a named server's move has `group` and `storage` set to `null` |
| `rift.events.permissions` | Every committed [permission](permissions.md) change, the same JSON as `rift.perms.update` |
| `rift.events.plugins` | Persistent plugins starting, failing, restarting and stopping, and operator restarts, as described in [plugins](plugin-operations.md) |
| `rift.events.servers` | Changes to what backends report about themselves (see [server state](#server-state)) |
| `rift.events.lifecycle` | `ready` and `draining` |

These are ephemeral events; query status to get current state.

A player event:

```json
{"event":"join","connection_id":"7","uuid":"…","name":"Alice","server":"lobby","visible_to":null}
```

- A `switch` also carries the `previous` server, and a `quit` names the last
  one.
- A `visibility` event reports a change of `visible_to`, with the old value
  in `previous_visible_to`.
- `uuid` is lowercase 32-digit hex without hyphens.
- Events for one player arrive in order.

## Java: control and events

`RiftControl` in the `rift-control` artifact wraps the control subjects for a
[Java client](messaging.md#java-clients) whose login has `control = true` and
a publish grant for `rift.control.*`:

```java
RiftControl control = new RiftControl(bus);

control.status().thenAccept(status -> {
    status.playersOnline();
    status.backend("lobby").ifPresent(lobby -> log(lobby.up() + " " + lobby.draining()));
    status.player("Alice").ifPresent(alice -> control.transfer(alice.connectionId(), "survival"));
});
control.maintenance(true);              // completes with the state Rift reports
control.drain("survival", true);
control.transfer("Alice", "lobby");     // looks the connection up in status first
control.reload();
control.nodes().thenAccept(nodes -> nodes.forEach(node ->
        log(node.name() + " " + node.state() + " " + node.instances() + "/" + node.maxInstances())));

// Every other subject of the table, and ones a newer Rift adds:
control.execute("create", Map.of("group", new Json.Str("lobby")));
```

Each call is one request and returns a `CompletableFuture`. Nothing is
retried. It fails with:

| Exception | When |
| --- | --- |
| `ControlException` | Rift answered `{"ok":false,…}`; `getMessage()` is `<command>: <error>` |
| `TimeoutException` | No reply came within the timeout (45 s by default, `withTimeout` changes it, at most 120 s) |
| Another `MessagingException` | The bus refused the request, such as a missing grant |

`execute` returns the reply's `data` as the `Json` value model of
`rift-permissions-client`. `ProxyStatus` and `NodeInfo` keep the whole object
in `raw()` for fields these types do not name.

`RiftEvents` types the `rift.events.*` subjects. Its login needs a subscribe
grant such as `rift.events.>`:

```java
RiftEvents.subscribe(bus, RiftEvents.ALL, event -> {
    switch (event) {
        case RiftEvents.Player player -> log(player.kind() + " " + player.name() + " " + player.server());
        case RiftEvents.ServerState state -> log(state.server() + " is " + state.state().orElse("idle"));
        case RiftEvents.Instance instance -> log(instance.name() + (instance.created() ? " created" : " removed"));
        case RiftEvents.Lifecycle lifecycle -> log("proxy " + lifecycle.state());
        case RiftEvents.Reload reload -> log("revision " + reload.revision());
        case RiftEvents.Control command -> log(command.command() + (command.ok() ? " ok" : " refused"));
    }
});
```

- The subscription survives reconnects, but events published while
  disconnected are lost; read `status()` again after one.
- `rift.events.permissions` and `rift.events.plugins` have no type here and
  are skipped. Subscribe to them with `MessagingClient.subscribe`, or use the
  [permission API](permissions-java.md) for permission changes.

## Server state

A backend reports what it is doing on `rift.server.state`. A minigame server
can mark itself `INGAME` and stop new players reaching it when a round
starts, then reopen when the round ends, without an operator login.

| Subject | Request | Reply `data` | Needs |
| --- | --- | --- | --- |
| `rift.server.state` | The fields below, all optional | The server's state; only with a reply subject | A login that [may report](#who-may-report) |

```json
{"state":"INGAME","properties":{"map":"Desert","round":"3"},"draining":true}
```

Fields left out stay as they are:

| Field | Meaning |
| --- | --- |
| `state` | A label of 1–32 characters of `A-Z`, `a-z`, `0-9`, `_` or `-`, kept as sent; `null` clears it. Rift's own words (`running`, `draining`, `starting`, `stopping`, `stopped`, `failed`, `unknown`, in any case) are refused |
| `properties` | String values merged into the server's properties; a `null` value removes that key. Keys are 1–64 characters of `A-Z`, `a-z`, `0-9`, `_`, `-` or `.`; values at most 256 bytes without control characters; at most 32 properties per server |
| `replace` | `true` drops every existing property before `properties` is applied |
| `draining` | `true` accepts no new players, as the operator `drain` command does; players already there stay. `false` withdraws it |
| `server` | The server to report for; see [who may report](#who-may-report) |

The report needs no reply subject, so a plain publish is enough. With an
`_INBOX.` reply subject (any request helper) the answer is
`{"ok":false,"error":"…"}` or the following, where `draining` is the server's
effective drain from every source. Refused reports are logged either way.

```text
{"ok":true,"data":{"server":"game-1","state":"INGAME","properties":{…},"self_draining":true,"draining":true}}
```

A Java server plugin with the bundled client:

```java
MessagingClient bus = MessagingClient.builder().fromEnvironment().build();
bus.publish("rift.server.state",
    "{\"state\":\"INGAME\",\"properties\":{\"map\":\"Desert\"},\"draining\":true}"
        .getBytes(StandardCharsets.UTF_8));
```

### Who may report

| Caller | Reports for |
| --- | --- |
| A login named `server.<name>` | The configured backend `<name>` only. It may repeat its own name in `server` but not name another |
| A `control = true` principal | The server it names in `server` |
| In-process publishers (Lua message scripts and plugins) | The server they name in `server` |
| Any other login | Refused |

[Instance logins](bus-logins.md) get publish
access to `rift.server.state` automatically, and the `backend` preset adds it
for configured principals.

### How long a report lasts

A report belongs to one run of a server and is not stored on disk.

- A managed server forgets its label, properties and drain each time it
  starts, so a game server that crashed mid-round comes back accepting
  players.
- An instance forgets them when it is removed.
- An unmanaged backend keeps its report until it changes it or Rift restarts.

### Drain

A server's own drain adds to the other reasons a server drains: the
`draining` setting, the operator `drain` command, a stopped service group.

An operator's `drain <server> on|off`, from any surface, also withdraws the
server's own drain. So `drain off` always reopens a server unless its group
is stopped; the server can ask again with another report.

### Where the state shows

| Surface | Shows |
| --- | --- |
| `rift.control.servers` (also through a plugin's `servers.control`), `GET /api/servers`, `rift admin servers` | `service_state` (the label or `null`) and `properties` for every managed server, beside `draining` |
| `rift.control.status` | The same for every backend |
| BungeeCord [`RiftServers`](bungeecord.md#rift-requests) | Entry state `draining`, else the label, else `running` |
| `rift.events.servers` | Every change, including a report forgotten on restart |

The event carries the whole report, so subscribers can replace what they
hold. Its `draining` is the server's own drain only:

```json
{"event":"state","server":"game-1","state":"INGAME","properties":{"map":"Desert"},"draining":true}
```
