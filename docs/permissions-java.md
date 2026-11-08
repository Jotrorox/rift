# Rift permissions for Java plugins

Rift keeps one permission store on the proxy and gives every backend a
replica. On Paper, Folia and Purpur the replica lives in the **RiftPerms**
plugin, which takes LuckPerms' place: `Player#hasPermission`, Vault and
PlaceholderAPI keep working, and plugins written for Rift get a native API
with the same model as the Rust crate. This page is for plugin authors and for
operators installing RiftPerms. The wire format is in
[the permissions protocol](permissions-protocol.md).

## Artifacts

Everything is published to a Maven repository on GitHub Pages, group
`io.github.jotrorox.rift`, mirrored to GitHub Packages:

| Artifact | What it is | Who depends on it |
| --- | --- | --- |
| `rift-permissions-api` | Interfaces and immutable values, no dependencies, no Bukkit imports | Every plugin, as `compileOnly` |
| `rift-permissions-test` | An in-memory implementation with builders, plus a MockBukkit helper | Plugin tests |
| `rift-permissions-client` | Replica, resolver, JSON codec and bus integration | Standalone Java services |
| `rift-messaging` | The QUIC client for Rift's [messaging bus](messaging-protocol.md) | Standalone Java services |
| `rift-permissions-paper` | The RiftPerms plugin jar (also attached to each GitHub release) | Server operators |

Versions follow the proxy's releases: use the API version of the RiftPerms
you run.

## Depending on the API

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

## Reading

Reads never touch the network. They answer from the replica, in the contexts
you pass or in the player's current ones (`server`, `world`, `dimension`,
`gamemode` and whatever context calculators add).

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
does; use `Tristate` directly when you want your own default.

The resolution rules are the proxy's (and are held to the same fixture corpus
in CI): the most specific pattern wins (exact, then the longest `prefix.*`,
then `*`); among equally specific entries, more context pairs win, then the
player's own nodes before groups (groups ordered by weight, then distance,
then name), then deny over allow. Expiry is checked at every call.

## Writing

Writes go to the proxy, which applies them in one transaction and one
revision and pushes the result to every server. Every write returns a
`CompletableFuture<Applied>` that completes when the proxy has committed, or
fails with `PermissionException` (a refusal, with an `ErrorCode`) or
`PermissionTimeoutException` after the configured timeout. Nothing hangs, so
`join()` is safe on a server thread in a small plugin; Folia plugins should
chain instead.

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

A write made on behalf of a player records them in the audit log and requires
them to be online here and to hold the matching `rift.perms.*` node:

```java
perms.apply(new Actor.Player(sender.getUniqueId()), change, "reward from /claim");
```

A timed-out write may still have been applied; compare `perms.revision()`
before retrying.

## Events

```java
perms.events().subscribe(PermissionChangeEvent.class, event -> {
    // revision, actor, ops (before/after pairs), changed players, changed groups
});
perms.events().subscribe(SubjectUpdatedEvent.class, event -> {
    // a player's snapshot on this server was replaced: drop caches about them
});
```

Listeners run in order on one RiftPerms thread, never on a server thread;
schedule back (with the entity scheduler on Folia) before touching game state.
Change events are delivered at most once, so use them for reactions, not for
bookkeeping that must see every revision.

## Custom contexts

```java
perms.contexts().register((Player player, ContextSet.Builder contexts) -> {
    String region = regions.at(player.getLocation());
    if (region != null) {
        contexts.add("region", region);
    }
});
```

Calculators run whenever a player's contexts are computed (cached for 50 ms),
possibly off the main thread, so keep them fast and thread-safe. Custom keys
are reported to the proxy on `rift.perms.context`, which accepts only the keys
in its `permissions.contexts.external` allowlist and then uses them for
proxy-side checks too.

## Testing

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

## Installing RiftPerms

1. Give the backend a principal on the proxy's messaging bus, named after the
   server, with exactly the permission grants from the protocol:

   ```lua
   messaging = {
       listen = "0.0.0.0:4222",
       certificate = "messaging-cert.pem",
       private_key = "messaging-key.pem",
       principals = {
           {
               name = "server.survival-1",
               token_env = "RIFT_SURVIVAL_1_TOKEN",
               publish = {
                   "rift.perms.hello", "rift.perms.subject.*", "rift.perms.groups.*",
                   "rift.perms.apply", "rift.perms.register", "rift.perms.command",
                   "rift.perms.context",
               },
               subscribe = { "rift.perms.update", "rift.perms.update.>", "rift.events.permissions" },
           },
       },
   }
   ```

2. Drop `RiftPerms-<version>.jar` into `plugins/`, remove LuckPerms, copy the
   bus certificate (or its CA) to `plugins/RiftPerms/messaging-cert.pem`, and
   start the server with the token in the environment
   (`RIFT_PERMS_TOKEN` by default).

3. Edit `plugins/RiftPerms/config.yml`:

   ```yaml
   server: survival-1            # this backend's name: the "server" context
   bus:
     address: 127.0.0.1:4222     # messaging.listen on the proxy
     server_name: localhost      # the DNS name in the bus certificate
     certificate: messaging-cert.pem
     token_env: RIFT_PERMS_TOKEN
   join_timeout_ms: 2000         # how long a joining player waits for their snapshot
   request_timeout_ms: 5000      # writes, /perms and Vault writes
   offline_policy: deny          # deny | last_snapshot | fallback_file
   snapshot_file: replica.json
   fallback_file: fallback.json
   vault: true
   placeholders: true
   ```

RiftPerms waits for a joining player's snapshot during
`AsyncPlayerPreLoginEvent` (the proxy usually pushes it before the player
arrives), injects its permissible at `PlayerLoginEvent`, and recalculates a
player's permissions on their own entity scheduler when their snapshot
changes, so it is safe on Folia. Permission attachments still work and are
consulted first, as in vanilla Bukkit.

### Offline policy

Players whose snapshot the server does not hold (the proxy is unreachable or
did not answer within `join_timeout_ms`) are answered by the policy, and
retried every 5 seconds:

- `deny`: nothing is set, so plugin defaults and operator status apply.
- `last_snapshot`: the snapshot saved at the last clean shutdown, in
  `snapshot_file` as `{"revision": n, "subjects": [compiled subject, ...]}`.
- `fallback_file`: the same static node list for every such player, in
  `fallback_file` as `{"primary_group": "default", "nodes": [typed node, ...]}`.
  Permission, prefix, suffix and meta nodes apply as the player's own; positive
  `inherit` nodes count as group memberships (`group.<name>`, Vault, the
  `in_group` placeholder) without granting the group's nodes.

### Vault

With Vault installed, RiftPerms registers `Permission` and `Chat` providers.
Reads come from the replica (an offline player's snapshot is fetched once);
writes become changes on the proxy and block for its reply, because Vault is
synchronous. A `world` argument scopes writes to `server=<this server>` and
`world=<world>`. `getPlayerGroups` lists every group the player is in,
inherited ones included; `groupHas` looks at the group's own nodes only.
Setting a prefix or suffix through Vault gives it a priority ten above the
highest one the subject has, so it wins.

### PlaceholderAPI

| Placeholder | Value |
| --- | --- |
| `%rift_prefix%`, `%rift_suffix%` | The winning prefix or suffix, or empty |
| `%rift_primary_group%` | The primary group |
| `%rift_meta_<key>%` | A meta value, or empty |
| `%rift_has_permission_<node>%` | `true` or `false` |
| `%rift_in_group_<name>%` | `true` or `false`, inherited groups included |

### `/perms`

`/perms` (alias `/rp`) accepts the LuckPerms grammar for common operations
(`/perms user Alice permission set essentials.fly true server=survival`,
`/perms group vip setweight 50`, `/perms trace Alice essentials.fly`). The
backend only forwards the arguments; the proxy parses, authorises
(`rift.perms.<verb>` nodes), applies, audits and answers with a message.
Tab completion offers online players, the group table and registered
permissions. On enable, RiftPerms sends every plugin's `plugin.yml`
permissions to the proxy for the dashboard and tab completion.

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
Tristate t = perms.check(uuid, "chat.color", ContextSet.of("server", "lobby-bot"));
```

Without a platform adapter, players are `UUID`s. `rift-messaging` alone is a
general client for the bus: `publish`, `request`, `subscribe` (re-established
after every reconnect) and a pipelined `Publisher`.

## What a LuckPerms user loses

- `/lp editor`: the Rift dashboard edits groups, users and tracks instead.
- `/lp verbose`: `/perms trace <player> <node>` explains the winning entry and
  why every other match lost.
- LuckPerms' SQL tables, which some analytics plugins read directly.
- Plugins with a hard `depend: [LuckPerms]` or imports of `net.luckperms.api`:
  there is no LuckPerms API shim, so they need a port to `rift-permissions-api`,
  which is mostly renames.
- Regex nodes (`r=`): the importer refuses them; `{a,b}` alternations are
  expanded into separate nodes on import.

## Building from source

```sh
cd java
./gradlew build                       # libraries, tests against tests/fixtures/permissions, the plugin jar
cargo build --bin rift && ./gradlew integrationTest   # the Java clients against a real rift over QUIC
```

The plugin jar is `java/paper/build/libs/RiftPerms-<version>.jar`. Java 21 is
the target (Paper's minimum); Gradle downloads a JDK 21 toolchain when none is
installed.
