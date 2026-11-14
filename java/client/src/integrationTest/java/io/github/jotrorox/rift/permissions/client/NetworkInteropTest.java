package io.github.jotrorox.rift.permissions.client;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

import io.github.jotrorox.rift.messaging.MessagingClient;
import io.github.jotrorox.rift.network.BusMessage;
import io.github.jotrorox.rift.network.Messenger;
import io.github.jotrorox.rift.network.NetworkException;
import io.github.jotrorox.rift.network.NetworkException.Reason;
import io.github.jotrorox.rift.network.NetworkTimeoutException;
import io.github.jotrorox.rift.network.RiftNetwork;
import io.github.jotrorox.rift.network.ServerInfo;
import io.github.jotrorox.rift.network.ServerState;
import io.github.jotrorox.rift.network.Subscription;
import io.github.jotrorox.rift.network.client.NetworkClient;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.time.Duration;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.OptionalInt;
import java.util.UUID;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;

/**
 * The network API against the real proxy: {@code rift.network.*} with the grants of the {@code
 * backend} preset, and the bus as a {@link Messenger}.
 */
class NetworkInteropTest {
    private static final Duration WAIT = Duration.ofSeconds(10);

    private static RiftProcess rift;
    private static MessagingClient survivalBus;
    private static MessagingClient rawBus;
    private static RiftNetwork survival;
    private static RiftNetwork raw;

    @BeforeAll
    static void start() throws Exception {
        assumeTrue(Files.isExecutable(RiftProcess.binary()), "build the proxy first: cargo build --bin rift");
        rift = RiftProcess.start();
        survivalBus = rift.connect(RiftProcess.SURVIVAL_TOKEN);
        rawBus = rift.connect(RiftProcess.RAW_TOKEN);
        survivalBus.connected().get(20, TimeUnit.SECONDS);
        rawBus.connected().get(20, TimeUnit.SECONDS);
        survival = NetworkClient.builder().messaging(survivalBus).server("survival-1").group("survival").build();
        raw = NetworkClient.builder().messaging(rawBus).server("raw").build();
    }

    @AfterAll
    static void stop() throws Exception {
        if (rift != null) {
            survivalBus.close();
            rawBus.close();
            rift.close();
        }
    }

    private static <T> T get(CompletableFuture<T> future) throws Exception {
        return future.get(WAIT.toMillis(), TimeUnit.MILLISECONDS);
    }

    private static NetworkException failure(CompletableFuture<?> future) {
        ExecutionException error = assertThrows(ExecutionException.class, () -> get(future));
        return assertInstanceOf(NetworkException.class, error.getCause());
    }

    @Test
    void aBackendLoginListsTheNetwork() throws Exception {
        assertEquals("survival-1", survival.localServer());
        assertEquals(Optional.of("survival"), survival.localGroup());
        // Both configured backends have no health check, so both count as up.
        ServerInfo lobby = server("lobby", ServerState.RUNNING, Optional.empty(), Map.of());
        assertEquals(
                List.of(lobby, server("survival-1", ServerState.RUNNING, Optional.empty(), Map.of())),
                get(survival.servers()));
        assertEquals(List.of(lobby), get(survival.servers("LOBBY")));
        assertEquals(List.of(), get(survival.players()));
        assertEquals(List.of(), get(survival.players("lobby")));
        assertEquals(Optional.empty(), get(survival.player(UUID.randomUUID())));
        assertEquals(Optional.empty(), get(survival.player("Nobody")));

        NetworkException unknown = failure(survival.servers("missing"));
        assertEquals(Reason.REFUSED, unknown.reason());
        assertTrue(unknown.getMessage().contains("unknown server"), unknown.getMessage());
    }

    private static ServerInfo server(
            String name, ServerState state, Optional<String> serviceState, Map<String, String> properties) {
        return new ServerInfo(name, Optional.empty(), state, serviceState, properties, 0, OptionalInt.empty());
    }

    /** What a backend reports on {@code rift.server.state} shows in the listing, its drain included. */
    @Test
    void aServersOwnReportShowsInTheListing() throws Exception {
        String report = "{\"state\":\"INGAME\",\"properties\":{\"map\":\"Desert\"},\"draining\":true}";
        get(survival.messages().request("rift.server.state", report.getBytes(StandardCharsets.UTF_8), WAIT));
        try {
            assertEquals(
                    List.of(server("survival-1", ServerState.DRAINING, Optional.of("INGAME"), Map.of("map", "Desert"))),
                    get(survival.servers("survival-1")));
        } finally {
            String clear = "{\"state\":null,\"replace\":true,\"draining\":false}";
            get(survival.messages().request("rift.server.state", clear.getBytes(StandardCharsets.UTF_8), WAIT));
        }
        assertEquals(
                List.of(server("survival-1", ServerState.RUNNING, Optional.empty(), Map.of())),
                get(survival.servers("survival-1")));
    }

    @Test
    void connectAnswersForOfflinePlayersAndChecksTheDestination() throws Exception {
        UUID nobody = UUID.randomUUID();
        assertFalse(get(survival.connect(nobody, "lobby")));
        assertEquals(Reason.REFUSED, failure(survival.connect(nobody, "missing")).reason());
        // `lobby` is a server, not a service group.
        NetworkException notAGroup = failure(survival.connectToGroup(nobody, "lobby"));
        assertEquals(Reason.REFUSED, notAGroup.reason());
        assertTrue(notAGroup.getMessage().contains("service group"), notAGroup.getMessage());
    }

    @Test
    void aLoginWithoutTheGrantIsDenied() {
        NetworkException denied = failure(raw.servers());
        assertEquals(Reason.DENIED, denied.reason());
        assertTrue(denied.getMessage().contains("rift.network.servers"), denied.getMessage());
        // Nobody but the proxy may listen to network requests.
        assertEquals(
                Reason.DENIED,
                failure(survival.messages().subscribe("rift.network.*", message -> {})).reason());
    }

    @Test
    void messagesReachOtherLoginsAndAnswerRequests() throws Exception {
        Messenger here = survival.messages();
        Messenger there = raw.messages();
        BlockingQueue<BusMessage> received = new LinkedBlockingQueue<>();
        Subscription events = get(there.subscribe("test.party.*", received::add));
        Subscription echo = get(there.subscribe("test.echo", request -> there.publish(
                request.replyTo().orElseThrow(), ("echo:" + request.text()).getBytes(StandardCharsets.UTF_8))));
        get(here.publish("test.party.invite", "Alice".getBytes(StandardCharsets.UTF_8)));
        BusMessage invite = received.poll(WAIT.toMillis(), TimeUnit.MILLISECONDS);
        assertEquals("test.party.invite", invite.subject());
        assertEquals("Alice", invite.text());
        assertEquals(Optional.empty(), invite.replyTo());

        BusMessage reply = get(here.request("test.echo", "ping".getBytes(StandardCharsets.UTF_8), WAIT));
        assertEquals("echo:ping", reply.text());
        events.close();
        echo.close();

        assertEquals(
                Reason.UNAVAILABLE,
                failure(here.request("test.nobody", new byte[0], Duration.ofSeconds(2))).reason());
        Subscription silent = get(there.subscribe("test.silent", request -> {}));
        assertInstanceOf(
                NetworkTimeoutException.class,
                failure(here.request("test.silent", new byte[0], Duration.ofMillis(300))));
        silent.close();
        assertEquals(Reason.DENIED, failure(here.publish("other.subject", new byte[0])).reason());
    }
}
