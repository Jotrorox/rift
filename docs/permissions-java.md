# Permissions API for Java

Paper, Folia and Purpur plugins read and write Rift's permissions through
`rift-permissions-api`, which the [RiftPerms](riftperms.md) plugin provides at
runtime. The model is the Rust crate's, and the wire format is in
[the permissions protocol](permissions-protocol.md).

## Artifacts

Everything is published to a Maven repository on GitHub Pages, group
`io.github.jotrorox.rift`, mirrored to GitHub Packages:

| Artifact | What it is | Who depends on it |
| --- | --- | --- |
| `rift-permissions-api` | Interfaces and immutable values, no dependencies, no Bukkit imports | Every plugin, as `compileOnly` |
| `rift-permissions-test` | An in-memory implementation with builders, plus a MockBukkit helper | Plugin tests |
| `rift-permissions-client` | Replica, resolver, JSON codec and bus integration | Standalone Java services |
| `rift-messaging` | The QUIC client for Rift's [messaging bus](messaging-protocol.md), [retained streams](messaging.md#java-streams) included | Standalone Java services |
| `rift-control` | [`RiftControl` and `RiftEvents`](messaging-subjects.md#java-control-and-events): status, maintenance, drain, transfer, reload, nodes and typed events | Operator tools and services with a `control` login |
| `rift-permissions-paper` | The RiftPerms plugin jar (also attached to each GitHub release) | Server operators |

Versions follow the proxy's releases: use the API version of the RiftPerms
you run.

RiftPerms also provides the [network API](network-java.md)
(`rift-network-api`): listing servers, finding and connecting players, and
messages to proxy plugins, over the same bus connection.

## Depend on the API

Gradle (Kotlin DSL):

```kotlin
repositories {
    maven("https://jotrorox.github.io/rift/maven")
}

dependencies {
    compileOnly("io.github.jotrorox.rift:rift-permissions-api:0.1.0")
    testImplementation("io.github.jotrorox.rift:rift-permissions-test:0.1.0")
}
```

Gradle (Groovy DSL):

```groovy
repositories {
    maven { url "https://jotrorox.github.io/rift/maven" }
}

dependencies {
    compileOnly "io.github.jotrorox.rift:rift-permissions-api:0.1.0"
}
```

Maven:

```xml
<repository>
  <id>rift</id>
  <url>https://jotrorox.github.io/rift/maven</url>
</repository>

<dependency>
  <groupId>io.github.jotrorox.rift</groupId>
  <artifactId>rift-permissions-api</artifactId>
  <version>0.1.0</version>
  <scope>provided</scope>
</dependency>
```

Do not shade the API: RiftPerms provides it at runtime. Declare the
dependency in `plugin.yml` so RiftPerms enables first:

```yaml
depend: [RiftPerms]   # or softdepend, if your plugin also runs without it
```

## Read permissions

Reads never touch the network. They answer from the replica, in the contexts
you pass or in the player's current ones: `server`, `group`, `world`,
`dimension`, `gamemode` and whatever
[context calculators](#add-custom-contexts) add. A set that names no `server`
gets this server's, and its `group` too.

```java
RiftPermissions perms = RiftPermissions.get();
// or: Bukkit.getServicesManager().load(RiftPermissions.class)

Tristate fly = perms.check(player, "essentials.fly");          // TRUE, FALSE or UNDEFINED
boolean canFly = fly.asBoolean(false);
Tristate there = perms.check(player.getUniqueId(), "essentials.fly",
        ContextSet.builder().add("server", "survival").add("world", "world_nether").build());

MetaView meta = perms.meta(player);
String prefix = meta.prefix();                                 // null when none applies
int homes = meta.value("homes").map(Integer::parseInt).orElse(1);
String group = meta.primaryGroup();

boolean vip = perms.inGroup(player.getUniqueId(), "vip", perms.contexts().of(player));
Trace why = perms.trace(player.getUniqueId(), "essentials.fly", perms.contexts().of(player));
```

`Tristate.UNDEFINED` means nothing matched. `Player#hasPermission` then falls
back to the registered permission's default and operator status, as Bukkit
does. Use `Tristate` directly when you want your own default.

Checks follow the proxy's [resolution rules](permissions.md#how-a-check-is-decided) and
are held to the same fixture corpus in CI. Expiry is checked at every call.

## Write permissions

Writes go to the proxy, which applies them in one transaction and one
revision and pushes the result to every server. Every write returns a
`CompletableFuture<Applied>` that:

- completes when the proxy has committed;
- fails with `PermissionException` (a refusal, with an `ErrorCode`);
- fails with `PermissionTimeoutException` after the configured timeout.

Nothing hangs, so `join()` is safe on a server thread in a small plugin.
Folia plugins should chain instead.

```java
perms.user(uuid)
     .set(Node.permission("essentials.fly", true).expiresIn(Duration.ofHours(1)).context("server", "survival"))
     .thenAccept(applied -> getLogger().info("revision " + applied.revision()));
perms.user(uuid).addParent("vip", Duration.ofDays(30));
perms.group("vip").setWeight(50);
perms.group("vip").setPrefix(100, "&6[VIP] ");
perms.track("ranks").promote(uuid);
perms.apply(Change.bulk(Filter.kind(NodeKind.PERMISSION).keyLike("essentials.%"), Patch.value(false)));
```

A write made on behalf of a player records them in the audit log. It requires
them to be online here and to hold the matching `rift.perms.*` node:

```java
perms.apply(new Actor.Player(sender.getUniqueId()), change, "reward from /claim");
```

> [!WARNING]
> A timed-out write may still have been applied. Compare `perms.revision()`
> before retrying.

## Listen for changes

```java
perms.events().subscribe(PermissionChangeEvent.class, event -> {
    // revision, actor, ops (before/after pairs), changed players, changed groups
});
perms.events().subscribe(SubjectUpdatedEvent.class, event -> {
    // a player's snapshot on this server was replaced: drop caches about them
});
```

- Listeners run in order on one RiftPerms thread, never on a server thread.
  Schedule back (with the entity scheduler on Folia) before touching game
  state.
- Change events are delivered at most once. Use them for reactions, not for
  bookkeeping that must see every revision.

## Visibility on Paper

Rift leaves a hidden player out of everything it builds (see
[visibility](permissions.md#visibility)), but a backend lists its own players
itself. A Paper vanish plugin hides them there by reading the same meta key
from the replica, so the proxy, every backend and the plugin agree without
talking to each other:

```java
RiftPermissions perms = RiftPermissions.get();

/** Whether viewer may see target, by Rift's rule. */
boolean visible(Player viewer, Player target) {
    Optional<String> node = perms.meta(target).value("rift.visible-to");
    return node.isEmpty()
            || viewer.equals(target)
            || perms.check(viewer.getUniqueId(), node.get(), perms.contexts().of(viewer)).asBoolean(false);
}

void apply(Player target) {
    for (Player viewer : Bukkit.getOnlinePlayers()) {
        if (visible(viewer, target)) {
            viewer.showPlayer(plugin, target);
        } else {
            viewer.hidePlayer(plugin, target);
        }
    }
}
```

Call `apply` for a player:

- on join, and for every other player too, since the newcomer is also a
  viewer;
- on world change;
- from a `SubjectUpdatedEvent` listener, which fires when the target's rule
  or a viewer's `see` node changes. [Schedule back](#listen-for-changes)
  before calling `hidePlayer`.

Toggling is a write like any other, for example
`perms.user(uuid).set(Node.meta("rift.visible-to", "vanish.see")).join()`.
The proxy then hides the player network-wide.

## Add custom contexts

```java
perms.contexts().register((Player player, ContextSet.Builder contexts) -> {
    String region = regions.at(player.getLocation());
    if (region != null) {
        contexts.add("region", region);
    }
});
```

Calculators run whenever a player's contexts are computed (cached for 50 ms),
possibly off the main thread, so keep them fast and thread-safe.

RiftPerms reports custom keys to the proxy on `rift.perms.context`. The proxy
accepts only the keys in its `permissions.contexts.external` allowlist, and
then uses them for proxy-side checks too.

## Test your plugin

`rift-permissions-test` implements the same API in memory with the proxy's
compilation rules, so tests need no proxy and no network:

```java
InMemoryRiftPermissions perms = InMemoryRiftPermissions.builder()
        .group("vip", group -> group.weight(10).permission("essentials.fly", true).prefix(100, "[VIP] "))
        .user(alice, user -> user.parent("vip"))
        .track("ranks", "default", "vip", "mod")
        .build();

assertEquals(Tristate.TRUE, perms.check(alice, "essentials.fly", ContextSet.empty()));
perms.track("ranks").promote(alice).join();
```

Writes apply immediately (one revision per effective change) and events fire
synchronously. With MockBukkit on the test classpath, players are real
`Player` objects and `hasPermission` answers from the in-memory store:

```java
ServerMock server = MockBukkit.mock();
InMemoryRiftPermissions perms = MockBukkitPermissions.builder("survival")
        .group("default", group -> group.permission("kits.starter", true))
        .build();
MockBukkitPermissions.install(MockBukkit.createMockPlugin(), perms);   // services manager + RiftPermissions.get()
PlayerMock player = server.addPlayer();
MockBukkitPermissions.inject(player, perms);
assertTrue(player.hasPermission("kits.starter"));
```

To test against a real proxy, `rift dev --server <name>` starts one with a
backend principal and prints its token (see
[`rift dev`](permissions.md#rift-dev)).

## Standalone Java services

A lobby bot or a web service can follow permissions without Bukkit:

```java
MessagingClient bus = MessagingClient.builder()
        .address("proxy.internal", 4222).serverName("proxy.internal")
        .trust(Path.of("messaging-cert.pem"))
        .token(System.getenv("RIFT_LOBBY_TOKEN"))
        .build();
PermissionsClient perms = PermissionsClient.builder()
        .messaging(bus)
        .server("lobby-bot")
        .build();
perms.load(uuid).join();
Tristate t = perms.check(uuid, "chat.color", ContextSet.empty());   // server=lobby-bot is added
```

Without a platform adapter, players are `UUID`s. The principal must be
`server.lobby-bot` with the `backend` preset.

- `playerJoined`, `awaitSubject` and `playerLeft` mirror the
  [Rust client's](permissions-clients.md#client-lifecycle) `join` and
  `leave`.
- `register`, `reportContexts`, `command` and `fetchGroup` cover the rest of
  the protocol.
- `isAvailable` tells whether the proxy answered `hello`.

`rift-messaging` alone is a general client for the bus: `publish`, `request`,
`subscribe` (re-established after every reconnect) and a pipelined
`Publisher`.

On a managed server with a bus login,
`MessagingClient.builder().fromEnvironment().build()` takes the address,
certificate name, certificate and token from Rift's `RIFT_BUS_*` variables,
and names the first missing one otherwise.

## Build from source

```sh
cd java
./gradlew build                       # libraries, tests against tests/fixtures/permissions, the plugin jar
cargo build --bin rift && ./gradlew integrationTest   # the Java clients against a real rift over QUIC
```

The plugin jar is `java/paper/build/libs/RiftPerms-<version>.jar`. Java 21 is
the target (Paper's minimum); Gradle downloads a JDK 21 toolchain when none is
installed.

The integration tests run the Java clients and the shaded plugin jar's
client against the real proxy: logins, writes with `rift admin perms`, a
restart. They are skipped when `target/debug/rift` (or `$RIFT_BINARY`) is
missing.
