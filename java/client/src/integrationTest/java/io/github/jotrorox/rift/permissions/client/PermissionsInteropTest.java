package io.github.jotrorox.rift.permissions.client;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

import io.github.jotrorox.rift.messaging.MessagingClient;
import io.github.jotrorox.rift.permissions.Actor;
import io.github.jotrorox.rift.permissions.Applied;
import io.github.jotrorox.rift.permissions.Change;
import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.ErrorCode;
import io.github.jotrorox.rift.permissions.Node;
import io.github.jotrorox.rift.permissions.PermissionChangeEvent;
import io.github.jotrorox.rift.permissions.PermissionException;
import io.github.jotrorox.rift.permissions.PermissionTimeoutException;
import io.github.jotrorox.rift.permissions.SubjectRef;
import io.github.jotrorox.rift.permissions.SubjectUpdatedEvent;
import io.github.jotrorox.rift.permissions.Tristate;
import java.nio.file.Files;
import java.time.Duration;
import java.util.List;
import java.util.UUID;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.CompletionException;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.function.BooleanSupplier;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

/**
 * {@link PermissionsClient} end to end over the real QUIC bus, with a second Java client standing
 * in for the proxy's permission handlers.
 */
class PermissionsInteropTest {
    private static final UUID ALICE = UUID.fromString("01234567-89ab-cdef-0123-456789abcdef");
    private static final Duration WAIT = Duration.ofSeconds(15);

    private RiftProcess rift;
    private ProxyStandIn proxy;
    private MessagingClient bus;
    private PermissionsClient client;

    @BeforeEach
    void start() throws Exception {
        assumeTrue(Files.isExecutable(RiftProcess.binary()), "build the proxy first: cargo build --bin rift");
        rift = RiftProcess.start();
        proxy = new ProxyStandIn(rift.connect(RiftProcess.STANDIN_TOKEN));
        proxy.bus.connected().get(20, TimeUnit.SECONDS);
        proxy.start();
        bus = rift.connect(RiftProcess.BACKEND_TOKEN);
        client = PermissionsClient.builder()
                .messaging(bus)
                .server("survival-1")
                .requestTimeout(Duration.ofSeconds(2))
                .build();
    }

    @AfterEach
    void stop() throws Exception {
        if (rift != null) {
            client.close();
            bus.close();
            proxy.close();
            rift.close();
        }
    }

    private static void await(String what, BooleanSupplier condition) throws InterruptedException {
        long deadline = System.nanoTime() + WAIT.toNanos();
        while (!condition.getAsBoolean()) {
            if (System.nanoTime() > deadline) {
                throw new AssertionError("timed out waiting for " + what);
            }
            Thread.sleep(20);
        }
    }

    @Test
    void helloJoinWriteAndPush() throws Exception {
        Protocol.HelloRequest hello = proxy.hellos.poll(WAIT.toSeconds(), TimeUnit.SECONDS);
        assertNotNull(hello, "no hello");
        assertEquals("survival-1", hello.server());
        assertEquals(0, hello.revision());
        assertTrue(hello.clientVersion().startsWith("rift-permissions-client/"));
        await("hello reply", client::isAvailable);
        assertEquals(1, client.revision());
        await("group table", () -> client.groups().size() == 2);

        // A joining player without a pushed snapshot: subject.get during pre-login.
        assertEquals(ALICE, client.awaitSubject(ALICE).get(5, TimeUnit.SECONDS).uuid());
        assertEquals("survival-1", proxy.gets.poll(1, TimeUnit.SECONDS).server());
        ContextSet here = client.contexts().of(ALICE);
        assertEquals("survival-1", here.get(ContextSet.SERVER).orElseThrow());
        assertEquals(Tristate.TRUE, client.check(ALICE, "essentials.fly", here));
        assertTrue(client.inGroup(ALICE, "default", here));

        BlockingQueue<PermissionChangeEvent> changes = new LinkedBlockingQueue<>();
        BlockingQueue<SubjectUpdatedEvent> updates = new LinkedBlockingQueue<>();
        client.events().subscribe(PermissionChangeEvent.class, changes::add);
        client.events().subscribe(SubjectUpdatedEvent.class, updates::add);

        // A write: the future completes with the proxy's Applied, then the event and push follow.
        Applied applied = client.apply(new Actor.Player(ALICE),
                Change.setNode(SubjectRef.user(ALICE), Node.permission("essentials.fly", false)), "integration")
                .get(5, TimeUnit.SECONDS);
        assertEquals(2, applied.revision());
        Protocol.ApplyRequest request = proxy.applies.poll(1, TimeUnit.SECONDS);
        assertEquals(ALICE, request.actorUuid());
        assertEquals("integration", request.reason());
        PermissionChangeEvent change = changes.poll(WAIT.toSeconds(), TimeUnit.SECONDS);
        assertNotNull(change, "no change event");
        assertEquals(2, change.revision());
        assertEquals(new SubjectUpdatedEvent(ALICE, 2), updates.poll(WAIT.toSeconds(), TimeUnit.SECONDS));
        assertEquals(Tristate.FALSE, client.check(ALICE, "essentials.fly", here));
        assertEquals(2, client.revision());

        // Refusals and timeouts are typed.
        CompletionException refused = assertThrows(CompletionException.class,
                () -> client.group("vip").setWeight(5).join());
        assertEquals(ErrorCode.FORBIDDEN, assertInstanceOf(PermissionException.class, refused.getCause()).code());
        CompletionException timedOut = assertThrows(CompletionException.class,
                () -> client.command(null, List.of("ignore")).join());
        assertInstanceOf(PermissionTimeoutException.class, timedOut.getCause());

        // Command forwarding returns the proxy's component.
        Protocol.CommandReply reply = client.command(ALICE, List.of("user", "Alice", "info")).get(5, TimeUnit.SECONDS);
        assertEquals("{\"text\":\"ran user Alice info\",\"color\":\"green\"}", reply.component());

        // Registrations go out with PUBLISH_ACK; context reports keep allowlisted keys only.
        client.register(List.of(new Protocol.RegisteredPermission("essentials.fly", "Fly",
                Protocol.PermissionDefault.OP, "Essentials")));
        assertEquals("essentials.fly", proxy.registrations.poll(WAIT.toSeconds(), TimeUnit.SECONDS).getFirst().node());
        client.reportContexts(ALICE, ContextSet.builder().add("region", "spawn").add("secret", "x").build());
        Protocol.ContextReport report = proxy.contexts.poll(WAIT.toSeconds(), TimeUnit.SECONDS);
        assertEquals("region=spawn", report.contexts().toString());
        client.reportContexts(ALICE, ContextSet.builder().add("region", "spawn").build());
        assertNull(proxy.contexts.poll(300, TimeUnit.MILLISECONDS), "unchanged reports are not resent");
    }

    @Test
    void reconnectsResubscribesAndResynchronises() throws Exception {
        await("hello reply", client::isAvailable);
        client.awaitSubject(ALICE).get(5, TimeUnit.SECONDS);
        proxy.hellos.clear();

        rift.stop();
        assertEquals(Tristate.TRUE, client.check(ALICE, "essentials.fly", ContextSet.empty()),
                "the stale replica keeps answering");
        // Writes fail within the client timeout instead of hanging.
        CompletionException down = assertThrows(CompletionException.class,
                () -> client.group("vip").setWeight(1).join());
        assertInstanceOf(PermissionException.class, down.getCause());

        // The proxy comes back on the same port at a newer revision with a changed snapshot.
        proxy.revision = 5;
        proxy.fly.put(ALICE, false);
        rift.launch();
        // Rift has no stable stateless-reset key, so a restart is noticed by the idle timeout.
        Protocol.HelloRequest hello = proxy.hellos.poll(45, TimeUnit.SECONDS);
        assertNotNull(hello, "no hello after reconnecting");
        assertEquals(1, hello.revision(), "hello carries the replica's revision");
        Protocol.SubjectBatchRequest batch = proxy.batches.poll(WAIT.toSeconds(), TimeUnit.SECONDS);
        assertNotNull(batch, "a revision mismatch refetches stored subjects");
        assertEquals(List.of(ALICE), batch.uuids());
        await("resynchronised snapshot", () -> client.check(ALICE, "essentials.fly", ContextSet.empty()) == Tristate.FALSE);
        assertEquals(5, client.revision());

        // Subscriptions were re-established: a pushed snapshot arrives again.
        proxy.revision = 6;
        proxy.fly.put(ALICE, true);
        proxy.push(ALICE);
        await("pushed snapshot", () -> client.check(ALICE, "essentials.fly", ContextSet.empty()) == Tristate.TRUE);
    }
}
