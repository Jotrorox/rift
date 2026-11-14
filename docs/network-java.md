# Rift network API for Java plugins

A lobby selector, a party plugin or a cloud API needs to know which servers
are up, where a player is, and how to send them somewhere, including "any
instance of group `bedwars`". `rift-network-api` is the typed API for that on
Paper, Folia and Purpur. The **RiftPerms** plugin provides it at runtime over
the bus connection it already has, so a plugin needs neither the BungeeCord
plugin channel (and a carrier player) nor its own shaded bus client.

The requests behind it are the [network API
subjects](messaging.md#network-api-subjects); the permission API of the same
plugin is in [permissions for Java plugins](permissions-java.md).

## Artifacts

Same repository and versions as the [permission
artifacts](permissions-java.md#artifacts), group `io.github.jotrorox.rift`:

| Artifact | What it is | Who depends on it |
| --- | --- | --- |
| `rift-network-api` | Interfaces and immutable values, no dependencies, no Bukkit imports | Every plugin, as `compileOnly` |
| `rift-network-test` | An in-memory network with a builder | Plugin tests |
| `rift-network-client` | The API over a `rift-messaging` connection | Standalone Java services |

```kotlin
repositories {
    maven("https://jotrorox.github.io/rift/maven")
}

dependencies {
    compileOnly("io.github.jotrorox.rift:rift-network-api:0.1.0")
    testImplementation("io.github.jotrorox.rift:rift-network-test:0.1.0")
}
```

Do not shade the API: RiftPerms provides it. Declare the dependency in
`plugin.yml` so RiftPerms enables first:

```yaml
depend: [RiftPerms]
```

## Setup on the proxy

Nothing beyond [installing RiftPerms](permissions-java.md#installing-riftperms):
the `backend` preset of its bus login includes publish access to
`rift.network.*`. A login without the preset needs that grant itself:

```lua
{ name = "server.lobby-1", token_env = "RIFT_LOBBY_1_TOKEN", publish = { "rift.network.*" } }
```

Instances of a service group get it from the group's
[`messaging` block](managed-servers.md#bus-logins-for-instances)
(`messaging = { preset = "backend" }`).

## Servers and players

```java
RiftNetwork network = RiftNetwork.get();
// or: Bukkit.getServicesManager().load(RiftNetwork.class)

network.servers("lobby").thenAccept(lobbies -> {
    for (ServerInfo lobby : lobbies) {
        // name, group, state, players, maxPlayers
        boolean joinable = lobby.acceptsPlayers() && !lobby.isFull();
    }
});
network.servers();                       // every server that is up
network.players("bedwars");              // players on a server or on a group's instances
network.player(uuid).thenAccept(found ->
        found.ifPresent(friend -> sender.sendMessage(friend.name() + " is on " + friend.server())));
String here = network.localServer();     // this server's name; localGroup() is its service group
```

`servers` lists servers that are up, sorted by name: a managed server while
its process is running, an unmanaged one unless health checks mark it down.
`ServerState.DRAINING` servers keep their players but take no new ones.
`players()` is the count a player without special permissions sees, and
`maxPlayers()` is the limit Rift knows (the server's queue capacity, else its
group's `scaling.capacity_per_instance`), empty when it knows none. A server
name or service group that does not exist fails the future with
`NetworkException.Reason.REFUSED`; a group without running instances is an
empty list.

Player listings include players a [visibility
rule](permissions.md#visibility) hides, with the node a viewer needs in
`NetworkPlayer.visibleTo()`. Before showing a hidden player to someone, check
that node for the viewer, as in [visibility on
Paper](permissions-java.md#visibility-on-paper):

```java
boolean shown = friend.visibleTo()
        .map(node -> RiftPermissions.get().check(viewer, node).asBoolean(false))
        .orElse(true);
```

## Connecting players

```java
network.connect(player.getUniqueId(), "survival");          // one server
network.connectToGroup(player.getUniqueId(), "bedwars");    // any instance of the group
```

`connectToGroup` lets the proxy pick the instance, as it does for a route to
the group, with the group's queue and scaling rules; it refuses a name that is
not a service group. Both go through the proxy's access rules, capacity and
`before_transfer` hooks, like `/server`.

The future completes with `true` once the transfer is queued and `false` when
the player is offline. A queued transfer can still be refused, for example by
a full server; the player then stays where they are, and `player(uuid)` shows
where that is.

A server moves **its own players**: the login `server.lobby-1` may connect
players who are on `lobby-1`. Moving a player from another server fails with
`REFUSED` unless the login has `players = true` on the proxy, which a login
Rift issues to an instance never has. Every `connect` is written to the
[operator audit log](http.md#authentication-and-access) as `network.connect`.

## Messages to proxy plugins and other servers

`network.messages()` is the proxy's [message bus](messaging.md): the same
subjects [persistent plugins](plugins.md), the configuration script's
[`on_message` handlers](messaging-lua.md) and other servers use. Payloads are
bytes.

```java
Messenger bus = RiftNetwork.get().messages();

bus.subscribe("party.invite.*", message -> invites.handle(message.text()));
bus.publish("party.invite." + target, json.getBytes(StandardCharsets.UTF_8));

bus.request("friends.list", uuid.toString().getBytes(StandardCharsets.UTF_8), Duration.ofSeconds(2))
   .thenAccept(reply -> show(reply.text()));

// Answer requests: publish to the subject the sender asked for.
bus.subscribe("lobby.stats", request -> request.replyTo().ifPresent(reply ->
        bus.publish(reply, stats().getBytes(StandardCharsets.UTF_8))));
```

The login needs a grant for every subject it uses, in the principal or the
group's `messaging` block; a missing one fails the future with
`Reason.DENIED`:

```lua
messaging = { preset = "backend", publish = { "party.>" }, subscribe = { "party.>" } }
```

`subscribe(pattern, queue, handler)` joins a queue group, where each message
goes to one member. Subscriptions survive reconnects until closed. Delivery
is at most once, to subscribers connected at that moment.

## Threads and failures

Every call returns a `CompletableFuture` that completes on a Rift thread, and
subscription handlers run on one, never on a server thread. Schedule back
before touching game state (with the entity scheduler on Folia):

```java
network.servers("lobby").thenAccept(lobbies ->
        player.getScheduler().run(plugin, task -> menu.open(player, lobbies), null));
```

Futures fail with `NetworkException`, whose `reason()` is `REFUSED` (the
proxy said no, with its message), `DENIED` (no grant), `UNAVAILABLE` (no
connection, no responder, or `NetworkTimeoutException` after RiftPerms'
`request_timeout_ms`) or `INVALID`. Nothing hangs. A timed-out `connect` may
still have been queued.

## Testing

`rift-network-test` implements the API in memory:

```java
InMemoryRiftNetwork network = InMemoryRiftNetwork.builder("lobby-1", "lobby")
        .server("lobby-1", "lobby", 50)
        .server("bedwars-1", "bedwars", 16)
        .player(alice, "Alice", "lobby-1")
        .build();
network.install();                                   // RiftNetwork.get() now returns it

network.connectToGroup(alice, "bedwars").join();
assertEquals("bedwars-1", network.player(alice).join().orElseThrow().server());
```

Futures are complete when returned and handlers run on the calling thread.
A transfer moves the player at once, to the running instance with the fewest
players that has room. `setServer`, `removeServer`, `setPlayer` and
`removePlayer` change the network during a test, `connectAnyone()` on the
builder lifts the own-players rule, and `messages()` is a working bus with
the same subject matching, queue groups and request/reply.

## Standalone Java services

Without Bukkit, build the client on a bus connection:

```java
MessagingClient bus = MessagingClient.builder().fromEnvironment().build();
RiftNetwork network = NetworkClient.builder().messaging(bus).fromEnvironment().build();
```

`fromEnvironment` reads the `RIFT_BUS_*`, `RIFT_SERVER_NAME` and
`RIFT_SERVER_GROUP` variables Rift sets for managed servers; otherwise use
`address`, `trust`, `token` and `server(...)`. A service that moves players
on any server needs `players = true` on its principal.
