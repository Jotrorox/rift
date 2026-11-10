package io.github.jotrorox.rift.permissions.client;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

import io.github.jotrorox.rift.messaging.MessagingClient;
import io.github.jotrorox.rift.permissions.Actor;
import io.github.jotrorox.rift.permissions.Applied;
import io.github.jotrorox.rift.permissions.Change;
import io.github.jotrorox.rift.permissions.CompiledSubject;
import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.ErrorCode;
import io.github.jotrorox.rift.permissions.GroupInfo;
import io.github.jotrorox.rift.permissions.MetaView;
import io.github.jotrorox.rift.permissions.Node;
import io.github.jotrorox.rift.permissions.PermissionChangeEvent;
import io.github.jotrorox.rift.permissions.PermissionException;
import io.github.jotrorox.rift.permissions.SubjectRef;
import io.github.jotrorox.rift.permissions.SubjectUpdatedEvent;
import io.github.jotrorox.rift.permissions.Tristate;
import io.github.jotrorox.rift.permissions.Uuids;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.UUID;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.function.BooleanSupplier;
import java.util.logging.Handler;
import java.util.logging.LogRecord;
import java.util.logging.Logger;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/**
 * {@link PermissionsClient} against the real proxy's {@code rift.perms.*} handlers over QUIC, the
 * Java twin of {@code tests/permissions.rs}: two backends ({@code server.survival-1} with the
 * {@code backend} preset, {@code server.lobby} with {@code control = true}), writes through
 * {@code rift admin perms}, a player logging in through the proxy, and restarts.
 */
class PermissionsInteropTest {
    private static final String ALICE_NAME = "Alice";
    private static final UUID ALICE = MinecraftLogin.offlineUuid(ALICE_NAME);
    private static final Duration WAIT = Duration.ofSeconds(15);

    private final List<AutoCloseable> resources = new ArrayList<>();
    private RiftProcess rift;

    @BeforeEach
    void requireBinary() {
        assumeTrue(Files.isExecutable(RiftProcess.binary()), "build the proxy first: cargo build --bin rift");
    }

    @AfterEach
    void stop() throws Exception {
        for (AutoCloseable resource : resources.reversed()) {
            resource.close();
        }
        if (rift != null) {
            rift.close();
        }
    }

    /** Log lines of one client, for the handshake details the API does not expose. */
    private static final class Logs extends Handler {
        final List<String> lines = new CopyOnWriteArrayList<>();

        @Override
        public void publish(LogRecord record) {
            lines.add(record.getMessage());
        }

        @Override
        public void flush() {}

        @Override
        public void close() {}

        boolean any(String fragment) {
            return lines.stream().anyMatch(line -> line.contains(fragment));
        }
    }

    private static Logger logger(String name, Logs logs) {
        Logger logger = Logger.getLogger("rift-it-" + name + "-" + UUID.randomUUID());
        logger.addHandler(logs);
        return logger;
    }

    private PermissionsClient client(String server, String token, Logs logs) throws Exception {
        return client(server, token, logs, OfflinePolicy.deny());
    }

    private PermissionsClient client(String server, String token, Logs logs, OfflinePolicy policy) throws Exception {
        return client(server, null, token, logs, policy);
    }

    private PermissionsClient client(String server, String group, String token, Logs logs, OfflinePolicy policy)
            throws Exception {
        MessagingClient bus = rift.connect(token);
        resources.add(bus);
        PermissionsClient client = PermissionsClient.builder()
                .messaging(bus)
                .server(server)
                .group(group)
                .requestTimeout(Duration.ofSeconds(5))
                .joinTimeout(Duration.ofSeconds(2))
                .offlinePolicy(policy)
                .logger(logger(server, logs))
                .build();
        resources.add(client);
        return client;
    }

    private static void await(String what, BooleanSupplier condition) throws InterruptedException {
        await(what, WAIT, condition);
    }

    private static void await(String what, Duration wait, BooleanSupplier condition) throws InterruptedException {
        long deadline = System.nanoTime() + wait.toNanos();
        while (!condition.getAsBoolean()) {
            if (System.nanoTime() > deadline) {
                throw new AssertionError("timed out waiting for " + what);
            }
            Thread.sleep(20);
        }
    }

    private static PermissionException refusal(CompletableFuture<?> future, ErrorCode code) {
        CompletionException failure = assertThrows(CompletionException.class, future::join);
        PermissionException refused = assertInstanceOf(PermissionException.class, failure.getCause());
        assertEquals(code, refused.code(), refused.getMessage());
        return refused;
    }

    private static Change setNode(UUID uuid, Node node) {
        return Change.setNode(SubjectRef.user(uuid), node);
    }

    private static ContextSet contexts(String... pairs) {
        ContextSet.Builder builder = ContextSet.builder();
        for (int i = 0; i < pairs.length; i += 2) {
            builder.add(pairs[i], pairs[i + 1]);
        }
        return builder.build();
    }

    /** Log Alice in through the proxy; returns her client and backend sockets, open. */
    private Socket[] login(PermissionsClient survival, boolean[] pushed) throws Exception {
        CompletableFuture<Socket> player = CompletableFuture.supplyAsync(() -> {
            try {
                return MinecraftLogin.connect(rift.front(), ALICE_NAME);
            } catch (Exception e) {
                throw new CompletionException(e);
            }
        });
        Socket backend = MinecraftLogin.accept(rift.backend(), () -> {
            // Nobody asked for Alice yet: only the proxy's push can have brought her.
            long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2);
            while (System.nanoTime() < deadline && !pushed[0]) {
                pushed[0] = survival.compiled(ALICE).isPresent();
                Thread.onSpinWait();
            }
        });
        Socket client = player.get(10, TimeUnit.SECONDS);
        resources.add(client);
        resources.add(backend);
        return new Socket[] {client, backend};
    }

    @Test
    void backendsReplicateActAndResyncThroughTheRealProxy() throws Exception {
        rift = RiftProcess.start();
        Logs survivalLogs = new Logs();
        Logs lobbyLogs = new Logs();
        // survival-1 as an instance of a "survival" service group, for the group context.
        PermissionsClient survival =
                client("survival-1", "survival", RiftProcess.SURVIVAL_TOKEN, survivalLogs, OfflinePolicy.deny());
        PermissionsClient lobby = client("lobby", RiftProcess.LOBBY_TOKEN, lobbyLogs);

        // hello: both backends come online at the store's revision; the proxy saw each one.
        await("survival-1 hello", survival::isAvailable);
        await("lobby hello", lobby::isAvailable);
        assertEquals(0, survival.revision());
        assertTrue(survivalLogs.any("Connected to Rift permissions at revision 0"), survivalLogs.lines.toString());
        String backends = rift.perms("backends");
        assertTrue(backends.contains("server.survival-1 (survival-1): revision 0 at hello"), backends);
        assertTrue(backends.contains("rift-permissions-client/"), backends);
        assertTrue(backends.contains("server.lobby (lobby)"), backends);

        // A principal speaks only for its own server: hello naming another is refused with forbidden.
        Logs impostorLogs = new Logs();
        PermissionsClient impostor = client("survival-1", RiftProcess.LOBBY_TOKEN, impostorLogs);
        await("the refused hello", () -> impostorLogs.any("The proxy refused hello"));
        assertFalse(impostor.isAvailable());
        assertTrue(impostorLogs.any("speaks for server \"lobby\""), impostorLogs.lines.toString());
        MessagingClient raw = rift.connect(RiftProcess.LOBBY_TOKEN);
        resources.add(raw);
        raw.connected().get(20, TimeUnit.SECONDS);
        Protocol.Reply<Protocol.HelloReply> refused = Codec.readReply(Json.parse(raw.request(Protocol.HELLO,
                        Codec.toJson(new Protocol.HelloRequest("survival-1", 0, "raw")).toString()
                                .getBytes(StandardCharsets.UTF_8), Duration.ofSeconds(5))
                .get(10, TimeUnit.SECONDS).text()), Codec::readHelloReply);
        assertEquals(ErrorCode.FORBIDDEN, switch (refused) {
            case Protocol.Err<Protocol.HelloReply> err -> err.code();
            case Protocol.Ok<Protocol.HelloReply> ok -> null;
        });
        // The hello reply's fields as the proxy sends them.
        Protocol.Reply<Protocol.HelloReply> hello = Codec.readReply(Json.parse(raw.request(Protocol.HELLO,
                        Codec.toJson(new Protocol.HelloRequest("lobby", 0, "raw")).toString()
                                .getBytes(StandardCharsets.UTF_8), Duration.ofSeconds(5))
                .get(10, TimeUnit.SECONDS).text()), Codec::readHelloReply);
        Protocol.HelloReply welcome = switch (hello) {
            case Protocol.Ok<Protocol.HelloReply> ok -> ok.data();
            case Protocol.Err<Protocol.HelloReply> err -> throw new AssertionError(err.error());
        };
        assertEquals(Protocol.VERSION, welcome.protocol());
        assertEquals(List.of("region"), welcome.externalContexts());
        assertEquals(Protocol.Limits.DEFAULT, welcome.limits());
        assertTrue(Math.abs(welcome.now() - System.currentTimeMillis() / 1000) < 5);

        BlockingQueue<PermissionChangeEvent> changes = new LinkedBlockingQueue<>();
        BlockingQueue<SubjectUpdatedEvent> updates = new LinkedBlockingQueue<>();
        survival.events().subscribe(PermissionChangeEvent.class, changes::add);
        survival.events().subscribe(SubjectUpdatedEvent.class, updates::add);

        // Data for a player who never joined, addressed by UUID: the change event moves the revision.
        String text = rift.perms("user", Uuids.toHex(ALICE), "permission", "set", "essentials.fly");
        assertTrue(text.contains("Set essentials.fly to true"), text);
        PermissionChangeEvent first = changes.poll(WAIT.toSeconds(), TimeUnit.SECONDS);
        assertNotNull(first, "no change event");
        assertEquals(1, first.revision());
        assertEquals(List.of(ALICE), first.changed());
        await("revision 1", () -> survival.revision() == 1);
        assertTrue(survival.compiled(ALICE).isEmpty(), "nobody asked for Alice yet");

        // join: the proxy pushes the snapshot before forwarding the connection.
        boolean[] pushed = {false};
        login(survival, pushed);
        assertTrue(pushed[0], "Alice's snapshot was not pushed before her connection");
        survival.playerJoined(ALICE);
        CompiledSubject joined = survival.awaitSubject(ALICE).get(1, TimeUnit.SECONDS);
        assertEquals(1, joined.revision());
        ContextSet here = survival.contexts().of(ALICE);
        assertEquals("survival-1", here.get(ContextSet.SERVER).orElseThrow());
        assertEquals("survival", here.get(ContextSet.GROUP).orElseThrow());
        assertEquals("survival", survival.group().orElseThrow());
        assertEquals(Tristate.TRUE, survival.check(ALICE, "essentials.fly", here));
        assertTrue(survival.inGroup(ALICE, "default", here));
        await("Alice online on survival-1", () -> {
            try {
                return rift.perms("user", ALICE_NAME, "info").contains("Online on survival-1");
            } catch (Exception e) {
                throw new IllegalStateException(e);
            }
        });

        // A write through the CLI: change event plus pushed snapshot, and check() flips.
        updates.clear();
        rift.perms("user", ALICE_NAME, "permission", "set", "essentials.fly", "false");
        SubjectUpdatedEvent update = updates.poll(WAIT.toSeconds(), TimeUnit.SECONDS);
        assertEquals(new SubjectUpdatedEvent(ALICE, 2), update);
        assertEquals(Tristate.FALSE, survival.check(ALICE, "essentials.fly", here));
        await("revision 2", () -> survival.revision() == 2);

        // Contexts: world-scoped and server-scoped nodes; a set without server gets this server's.
        rift.perms("user", ALICE_NAME, "permission", "set", "warp.nether", "true", "world=nether");
        rift.perms("user", ALICE_NAME, "permission", "set", "kit.daily", "true", "server=lobby");
        rift.perms("user", ALICE_NAME, "permission", "set", "kit.weekly", "true", "server=survival-1");
        await("revision 5", () -> survival.compiled(ALICE).map(CompiledSubject::revision).orElse(0L) == 5);
        assertEquals(Tristate.TRUE, survival.check(ALICE, "warp.nether", here.with("world", "nether")));
        assertEquals(Tristate.UNDEFINED, survival.check(ALICE, "warp.nether", here.with("world", "overworld")));
        assertEquals(Tristate.UNDEFINED, survival.check(ALICE, "kit.daily", here));
        assertEquals(Tristate.TRUE, survival.check(ALICE, "kit.daily", contexts("server", "lobby")));
        assertEquals(Tristate.TRUE, survival.check(ALICE, "kit.weekly", ContextSet.empty()));
        assertEquals(Tristate.UNDEFINED, survival.check(ALICE, "kit.daily", ContextSet.empty()));

        // Meta: a weighted group with a prefix and a meta value; the group table follows.
        assertTrue(rift.perms("group", "vip", "create", "10").contains("Created group vip"));
        rift.perms("group", "vip", "meta", "setprefix", "100", "[VIP]");
        rift.perms("group", "vip", "meta", "set", "homes", "5");
        rift.perms("user", ALICE_NAME, "parent", "add", "vip");
        await("the vip prefix", () -> "[VIP]".equals(survival.meta(ALICE, here).prefix()));
        MetaView meta = survival.meta(ALICE, here);
        assertEquals("vip", meta.primaryGroup());
        assertEquals(Optional.of("5"), meta.value("homes"));
        assertTrue(survival.inGroup(ALICE, "vip", here));
        assertEquals(List.of("vip", "default"), survival.groupsOf(ALICE, here));
        await("the group table", () -> survival.groups().stream().anyMatch(g -> g.name().equals("vip")));
        GroupInfo vip = survival.groups().stream().filter(g -> g.name().equals("vip")).findFirst().orElseThrow();
        assertEquals(10, vip.weight());
        assertTrue(vip.nodes().contains(Node.prefix(100, "[VIP]")), vip.nodes().toString());
        GroupInfo fetched = survival.fetchGroup("vip").get(10, TimeUnit.SECONDS).orElseThrow();
        assertTrue(fetched.nodes().contains(Node.meta("homes", "5")), fetched.nodes().toString());

        // apply: a backend without control cannot write as itself; a player needs the verb node.
        Node chat = Node.permission("chat.color", true);
        PermissionException noControl = refusal(survival.apply(setNode(ALICE, chat)), ErrorCode.FORBIDDEN);
        assertTrue(noControl.getMessage().contains("control = true"), noControl.getMessage());
        PermissionException noNode = refusal(survival.apply(new Actor.Player(ALICE), setNode(ALICE, chat), "it"),
                ErrorCode.FORBIDDEN);
        assertTrue(noNode.getMessage().contains("rift.perms.user.permission.set"), noNode.getMessage());
        // The lobby cannot act for a player on survival-1.
        PermissionException elsewhere = refusal(lobby.apply(new Actor.Player(ALICE), setNode(ALICE, chat), "it"),
                ErrorCode.FORBIDDEN);
        assertTrue(elsewhere.getMessage().contains("not online"), elsewhere.getMessage());
        // A control principal writes as itself: the survival replica follows.
        Applied console = lobby.apply(setNode(ALICE, Node.permission("lobby.granted", true))).get(10, TimeUnit.SECONDS);
        assertEquals(1, console.ops());
        assertEquals(List.of(ALICE), console.changedUsers());
        await("the console write", () -> survival.check(ALICE, "lobby.granted", here) == Tristate.TRUE);
        await("its revision", () -> survival.revision() == console.revision());
        String log = rift.perms("log");
        assertTrue(log.contains("by backend:server.lobby"), log);

        // command: forwarded /perms answers with a component and never escalates.
        Protocol.CommandReply denied = survival.command(ALICE,
                List.of("user", ALICE_NAME, "permission", "set", "chat.color")).get(10, TimeUnit.SECONDS);
        assertTrue(denied.component().contains("rift.perms.user.permission.set"), denied.component());
        assertEquals(console.revision(), denied.revision());
        PermissionException consoleRefused = refusal(survival.command(null, List.of("listgroups")), ErrorCode.FORBIDDEN);
        assertTrue(consoleRefused.getMessage().contains("control = true"), consoleRefused.getMessage());
        Protocol.CommandReply groups = lobby.command(null, List.of("listgroups")).get(10, TimeUnit.SECONDS);
        assertTrue(groups.component().contains("vip (weight 10)"), groups.component());
        refusal(lobby.command(ALICE, List.of("listgroups")), ErrorCode.FORBIDDEN);
        rift.perms("user", ALICE_NAME, "permission", "set", "rift.perms.user.*");
        rift.perms("user", ALICE_NAME, "permission", "set", "chat.*");
        await("chat.* for Alice", () -> survival.check(ALICE, "chat.color", here) == Tristate.TRUE);
        long before = survival.revision();
        Protocol.CommandReply set = survival.command(ALICE,
                List.of("user", ALICE_NAME, "permission", "set", "chat.color", "false")).get(10, TimeUnit.SECONDS);
        assertTrue(set.component().contains("Set chat.color to false for Alice"), set.component());
        assertEquals(before + 1, set.revision());
        await("the forwarded write", () -> survival.check(ALICE, "chat.color", here) == Tristate.FALSE);
        Protocol.CommandReply escalate = survival.command(ALICE,
                List.of("user", ALICE_NAME, "permission", "set", "server.admin")).get(10, TimeUnit.SECONDS);
        assertTrue(escalate.component().contains("rift.perms.escalate"), escalate.component());
        // As the player, through apply, now that she holds the nodes.
        Applied own = survival.apply(new Actor.Player(ALICE), setNode(ALICE, Node.permission("chat.bold", true)),
                "integration").get(10, TimeUnit.SECONDS);
        await("Alice's own write", () -> survival.revision() == own.revision()
                && survival.check(ALICE, "chat.bold", here) == Tristate.TRUE);
        assertTrue(rift.perms("log", "--actor", ALICE_NAME).contains("by uuid:" + Uuids.toHex(ALICE)));

        // register: stored under the backend's principal.
        survival.register(List.of(new Protocol.RegisteredPermission("essentials.home", "Teleport home",
                Protocol.PermissionDefault.OP, "Essentials")));
        await("the registered permission", () -> {
            try {
                return rift.perms("registered")
                        .contains("essentials.home (default op) from server.survival-1/Essentials: Teleport home");
            } catch (Exception e) {
                throw new IllegalStateException(e);
            }
        });

        // context: only allowlisted keys, and only from the player's own backend.
        rift.perms("user", ALICE_NAME, "permission", "set", "warp.spawn", "true", "region=spawn");
        assertTrue(rift.perms("check", ALICE_NAME, "warp.spawn").contains("undefined"));
        survival.reportContexts(ALICE, contexts("region", "spawn", "evil", "x"));
        await("the reported context", () -> {
            try {
                return rift.perms("check", ALICE_NAME, "warp.spawn").contains("= true");
            } catch (Exception e) {
                throw new IllegalStateException(e);
            }
        });
        assertFalse(rift.logs().stream().anyMatch(line -> line.contains("dropped: evil")),
                "the client drops keys outside external_contexts itself");
        lobby.reportContexts(ALICE, contexts("region", "market"));
        Thread.sleep(300);
        assertTrue(rift.perms("check", ALICE_NAME, "warp.spawn").contains("= true"),
                "the lobby cannot report for Alice");
        survival.reportContexts(ALICE, contexts("region", "market"));
        await("the replaced context", () -> {
            try {
                return rift.perms("check", ALICE_NAME, "warp.spawn").contains("undefined");
            } catch (Exception e) {
                throw new IllegalStateException(e);
            }
        });
        // Locally the backend checks in the contexts it is given.
        assertEquals(Tristate.TRUE, survival.check(ALICE, "warp.spawn", here.with("region", "spawn")));

        // sync: every backend says hello again and refetches everything.
        impostor.close();
        Thread.sleep(1100);
        survivalLogs.lines.clear();
        String synced = rift.perms("sync");
        assertTrue(synced.contains("Asked every backend to resync to revision " + survival.revision()), synced);
        assertTrue(synced.contains("2 backends said hello again"), synced);
        await("the resync", () -> survivalLogs.any("Resynchronising with Rift permissions at revision "
                + survival.revision() + "; refetching 1 snapshots"));
        assertTrue(survivalLogs.any("The proxy requested a resync to revision "), survivalLogs.lines.toString());
        assertTrue(lobbyLogs.any("Resynchronising with Rift permissions"), lobbyLogs.lines.toString());
        assertTrue(survival.isAvailable());
    }

    @Test
    void reconnectsAfterARestartAndResynchronises(@TempDir Path directory) throws Exception {
        rift = RiftProcess.start();
        Logs logs = new Logs();
        Path snapshot = directory.resolve("replica.json");
        PermissionsClient survival = client("survival-1", RiftProcess.SURVIVAL_TOKEN, logs,
                OfflinePolicy.lastSnapshot(snapshot));
        await("hello", survival::isAvailable);
        rift.perms("user", Uuids.toHex(ALICE), "permission", "set", "essentials.fly", "false");
        await("revision 1", () -> survival.revision() == 1);
        survival.awaitSubject(ALICE).get(5, TimeUnit.SECONDS);
        survival.playerJoined(ALICE);
        assertEquals(Tristate.FALSE, survival.check(ALICE, "essentials.fly", ContextSet.empty()));

        rift.stop();
        assertEquals(Tristate.FALSE, survival.check(ALICE, "essentials.fly", ContextSet.empty()),
                "the stale replica keeps answering");
        // Writes fail within the client timeout instead of hanging.
        CompletionException down = assertThrows(CompletionException.class,
                () -> survival.group("vip").setWeight(1).join());
        assertInstanceOf(PermissionException.class, down.getCause());

        // The proxy comes back on the same port with the same store, and a write lands before the
        // client notices: only the handshake can bring it.
        logs.lines.clear();
        rift.launch();
        rift.perms("user", Uuids.toHex(ALICE), "permission", "set", "essentials.fly", "true");
        // Rift has no stable stateless-reset key, so a restart is noticed by the idle timeout.
        await("the resynchronised snapshot", Duration.ofSeconds(60),
                () -> survival.check(ALICE, "essentials.fly", ContextSet.empty()) == Tristate.TRUE);
        assertEquals(2, survival.revision());
        assertTrue(logs.any("Connected to Rift permissions at revision 2; refetching 1 snapshots"),
                logs.lines.toString());
        String backends = rift.perms("backends");
        assertTrue(backends.contains("server.survival-1 (survival-1): revision 1 at hello"), backends);

        // The subscriptions are back: a change event for a stored player refetches her snapshot.
        rift.perms("user", Uuids.toHex(ALICE), "permission", "set", "essentials.fly", "false");
        await("the refetch after the change event",
                () -> survival.check(ALICE, "essentials.fly", ContextSet.empty()) == Tristate.FALSE);
        assertEquals(3, survival.revision());

        // last_snapshot: written on close in the Rust client's shape, and served on the next start.
        survival.close();
        Json written = Json.parse(Files.readString(snapshot, StandardCharsets.UTF_8));
        Json.Obj file = assertInstanceOf(Json.Obj.class, written);
        assertEquals(java.util.Set.of("revision", "subjects"), file.members().keySet());
        Protocol.SubjectBatchReply saved = Codec.readSubjectBatchReply(written);
        assertEquals(3, saved.revision());
        assertEquals(List.of(ALICE), saved.subjects().stream().map(CompiledSubject::uuid).toList());
        rift.stop();
        Logs restarted = new Logs();
        PermissionsClient again = client("survival-1", RiftProcess.SURVIVAL_TOKEN, restarted,
                OfflinePolicy.lastSnapshot(snapshot));
        assertEquals(3, again.revision());
        assertEquals(Tristate.FALSE, again.check(ALICE, "essentials.fly", ContextSet.empty()));
        assertFalse(again.isFallback(ALICE));
        assertTrue(restarted.any("Loaded 1 permission snapshots at revision 3"), restarted.lines.toString());
    }

    @Test
    void fallbackFileAnswersUntilTheProxyDoes(@TempDir Path directory) throws Exception {
        rift = RiftProcess.create();
        UUID bob = UUID.fromString("00000000-0000-0000-0000-00000000000b");
        UUID carol = UUID.fromString("00000000-0000-0000-0000-00000000000c");
        Path file = directory.resolve("fallback.json");
        Files.writeString(file, """
                {"groups": [
                   {"name": "default", "nodes": [{"kind": "permission", "key": "essentials.spawn", "value": true}]},
                   {"name": "staff", "weight": 100, "nodes": [
                      {"kind": "permission", "key": "essentials.*", "value": true},
                      {"kind": "prefix", "priority": 100, "text": "&c[Staff] "}]}],
                 "users": [
                   {"uuid": "%s", "nodes": [{"kind": "inherit", "group": "staff", "value": true}]}]}
                """.formatted(Uuids.toHex(bob)));
        Logs logs = new Logs();
        PermissionsClient survival = client("survival-1", RiftProcess.SURVIVAL_TOKEN, logs,
                OfflinePolicy.fallbackFile(file));

        // The proxy is not running: joins time out and the fallback file is compiled locally.
        assertThrows(Exception.class, () -> survival.awaitSubject(bob).get(10, TimeUnit.SECONDS));
        survival.playerJoined(bob);
        assertTrue(survival.isFallback(bob));
        assertEquals(0, survival.compiled(bob).orElseThrow().revision());
        assertEquals(Tristate.TRUE, survival.check(bob, "essentials.fly", ContextSet.empty()));
        assertEquals("&c[Staff] ", survival.meta(bob, ContextSet.empty()).prefix());
        assertEquals("staff", survival.meta(bob, ContextSet.empty()).primaryGroup());
        survival.playerJoined(carol);
        assertTrue(survival.isFallback(carol), "a player the file does not list gets default");
        assertEquals(Tristate.TRUE, survival.check(carol, "essentials.spawn", ContextSet.empty()));
        assertEquals(Tristate.UNDEFINED, survival.check(carol, "essentials.fly", ContextSet.empty()));

        // The proxy starts with an empty store (revision 0, like the fallback subjects): its
        // snapshots replace the fallback ones anyway.
        rift.launch();
        await("hello", Duration.ofSeconds(30), survival::isAvailable);
        await("the proxy's snapshots", () -> !survival.isFallback(bob) && !survival.isFallback(carol));
        assertEquals(Tristate.UNDEFINED, survival.check(bob, "essentials.fly", ContextSet.empty()));
        assertEquals(Tristate.UNDEFINED, survival.check(carol, "essentials.spawn", ContextSet.empty()));
        assertEquals("default", survival.meta(bob, ContextSet.empty()).primaryGroup());
    }
}
