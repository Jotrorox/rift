package io.github.jotrorox.rift.permissions.client;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

import io.github.jotrorox.rift.control.ControlException;
import io.github.jotrorox.rift.control.ProxyStatus;
import io.github.jotrorox.rift.control.RiftControl;
import io.github.jotrorox.rift.control.RiftEvents;
import io.github.jotrorox.rift.messaging.MessagingClient;
import io.github.jotrorox.rift.messaging.MessagingException;
import io.github.jotrorox.rift.messaging.Subscription;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.List;
import java.util.Map;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;

/** {@code rift.control.*} and {@code rift.events.*} against the real proxy. */
class ControlInteropTest {
    private static RiftProcess rift;
    private static MessagingClient operatorBus;
    private static MessagingClient survivalBus;
    private static RiftControl control;

    @BeforeAll
    static void start() throws Exception {
        assumeTrue(Files.isExecutable(RiftProcess.binary()), "build the proxy first: cargo build --bin rift");
        rift = RiftProcess.start();
        operatorBus = rift.connect(RiftProcess.OPERATOR_TOKEN);
        survivalBus = rift.connect(RiftProcess.SURVIVAL_TOKEN);
        operatorBus.connected().get(20, TimeUnit.SECONDS);
        survivalBus.connected().get(20, TimeUnit.SECONDS);
        control = new RiftControl(operatorBus);
    }

    @AfterAll
    static void stop() throws Exception {
        if (rift != null) {
            operatorBus.close();
            survivalBus.close();
            rift.close();
        }
    }

    private static <T> T get(CompletableFuture<T> future) throws Exception {
        return future.get(15, TimeUnit.SECONDS);
    }

    private static <T extends Throwable> T failure(Class<T> type, CompletableFuture<?> future) {
        ExecutionException error = assertThrows(ExecutionException.class, () -> get(future));
        return assertInstanceOf(type, error.getCause(), () -> "failed with " + error.getCause());
    }

    /** The next event of {@code type}, skipping the others. */
    private static <T extends RiftEvents.Event> T next(BlockingQueue<RiftEvents.Event> events, Class<T> type)
            throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
        while (System.nanoTime() < deadline) {
            RiftEvents.Event event = events.poll(200, TimeUnit.MILLISECONDS);
            if (type.isInstance(event)) {
                return type.cast(event);
            }
        }
        throw new AssertionError("no " + type.getSimpleName() + " event");
    }

    @Test
    void statusMaintenanceAndDrain() throws Exception {
        BlockingQueue<RiftEvents.Event> events = new LinkedBlockingQueue<>();
        Subscription subscription = get(RiftEvents.subscribe(operatorBus, RiftEvents.CONTROL, events::add));
        try {
            ProxyStatus status = get(control.status());
            assertFalse(status.maintenance());
            assertEquals(0, status.playersOnline());
            assertTrue(status.players().isEmpty());
            assertEquals(
                    List.of("lobby", "survival-1"),
                    status.backends().stream().map(ProxyStatus.Backend::name).sorted().toList());
            assertNotNull(status.raw().get("managed_servers"));

            assertTrue(get(control.maintenance(true)));
            assertEquals(new RiftEvents.Control("maintenance", true), next(events, RiftEvents.Control.class));
            assertTrue(get(control.status()).maintenance());
            assertFalse(get(control.maintenance(false)));

            assertTrue(get(control.drain("lobby", true)));
            assertTrue(get(control.status()).backend("lobby").orElseThrow().draining());
            assertFalse(get(control.drain("lobby", false)));
            assertFalse(get(control.status()).backend("lobby").orElseThrow().draining());

            ControlException unknown = failure(ControlException.class, control.drain("missing", true));
            assertEquals("drain", unknown.command());
            assertTrue(unknown.getMessage().contains("unknown backend"), unknown.getMessage());
            ControlException offline = failure(ControlException.class, control.transfer("Nobody", "lobby"));
            assertEquals("transfer", offline.command());
            failure(ControlException.class, control.transfer(987_654, "lobby"));
        } finally {
            subscription.close();
        }
    }

    @Test
    void aServersOwnReportShowsInStatusAndAsAnEvent() throws Exception {
        BlockingQueue<RiftEvents.Event> events = new LinkedBlockingQueue<>();
        Subscription subscription = get(RiftEvents.subscribe(operatorBus, RiftEvents.SERVERS, events::add));
        try {
            get(survivalBus.publish(
                    "rift.server.state",
                    "{\"state\":\"INGAME\",\"properties\":{\"map\":\"Desert\"}}".getBytes(StandardCharsets.UTF_8)));
            RiftEvents.ServerState reported = next(events, RiftEvents.ServerState.class);
            assertEquals("survival-1", reported.server());
            assertEquals("INGAME", reported.state().orElseThrow());
            assertEquals(Map.of("map", "Desert"), reported.properties());
            assertFalse(reported.draining());

            ProxyStatus.Backend backend = get(control.status()).backend("survival-1").orElseThrow();
            assertEquals("INGAME", backend.serviceState().orElseThrow());
            assertEquals(Map.of("map", "Desert"), backend.properties());
        } finally {
            subscription.close();
            get(survivalBus.publish(
                    "rift.server.state", "{\"state\":null,\"replace\":true}".getBytes(StandardCharsets.UTF_8)));
        }
    }

    @Test
    void reloadNodesAndOtherCommands() throws Exception {
        BlockingQueue<RiftEvents.Event> events = new LinkedBlockingQueue<>();
        Subscription subscription = get(RiftEvents.subscribe(operatorBus, RiftEvents.ALL, events::add));
        try {
            get(control.reload());
            assertFalse(next(events, RiftEvents.Reload.class).revision().isEmpty());
        } finally {
            subscription.close();
        }
        // This proxy configures no nodes.
        assertEquals(List.of(), get(control.nodes()));
        // A command without a method of its own.
        assertInstanceOf(Json.Obj.class, get(control.execute("servers", Map.of())));
        assertEquals("bogus", failure(ControlException.class, control.execute("bogus", Map.of())).command());
    }

    @Test
    void aLoginWithoutControlIsDenied() {
        MessagingException denied =
                failure(MessagingException.class, new RiftControl(survivalBus).status());
        assertEquals(MessagingException.PERMISSION, denied.code());
        assertEquals(
                MessagingException.PERMISSION,
                failure(MessagingException.class, RiftEvents.subscribe(survivalBus, RiftEvents.ALL, event -> {}))
                        .code());
    }
}
