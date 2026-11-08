package io.github.jotrorox.rift.permissions.client;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

import io.github.jotrorox.rift.messaging.Message;
import io.github.jotrorox.rift.messaging.MessagingClient;
import io.github.jotrorox.rift.messaging.MessagingException;
import io.github.jotrorox.rift.messaging.Publisher;
import io.github.jotrorox.rift.messaging.Subscription;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.time.Duration;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.CompletionException;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;

/** The Java QUIC client against the real Rift broker: framing, auth, ACLs, request/reply, pub/sub. */
class MessagingInteropTest {
    private static RiftProcess rift;
    private static MessagingClient backend;
    private static MessagingClient peer;

    @BeforeAll
    static void start() throws Exception {
        assumeTrue(Files.isExecutable(RiftProcess.binary()), "build the proxy first: cargo build --bin rift");
        rift = RiftProcess.start();
        backend = rift.connect(RiftProcess.SURVIVAL_TOKEN);
        peer = rift.connect(RiftProcess.RAW_TOKEN);
        backend.connected().get(20, TimeUnit.SECONDS);
        peer.connected().get(20, TimeUnit.SECONDS);
    }

    @AfterAll
    static void stop() throws Exception {
        if (rift != null) {
            backend.close();
            peer.close();
            rift.close();
        }
    }

    private static byte[] bytes(String text) {
        return text.getBytes(StandardCharsets.UTF_8);
    }

    @Test
    void requestReplyAcrossPrincipals() throws Exception {
        Subscription responder = peer.subscribe("test.echo", null, message ->
                peer.publish(message.reply(), bytes("echo:" + message.text())).join()).get(10, TimeUnit.SECONDS);
        try {
            byte[] binary = {0, (byte) 0xff, 'h', 'i'};
            Message reply = backend.request("test.echo", binary, Duration.ofSeconds(5)).get(10, TimeUnit.SECONDS);
            assertTrue(reply.subject().startsWith("_INBOX."), reply.subject());
            assertArrayEquals(bytes("echo:" + new String(binary, StandardCharsets.UTF_8)), reply.payload());
        } finally {
            responder.close();
        }
    }

    @Test
    void publishSubscribeWithWildcards() throws Exception {
        BlockingQueue<Message> received = new LinkedBlockingQueue<>();
        Subscription subscription = backend.subscribe("test.events.>", null, received::add).get(10, TimeUnit.SECONDS);
        try {
            peer.publish("test.events.player.join", bytes("alice")).get(10, TimeUnit.SECONDS);
            Message message = received.poll(10, TimeUnit.SECONDS);
            assertNotNull(message, "no delivery");
            assertEquals("test.events.player.join", message.subject());
            assertEquals("alice", message.text());

            try (Publisher publisher = peer.publisher()) {
                for (int i = 0; i < 50; i++) {
                    publisher.publish("test.events.count", null, bytes(Integer.toString(i)));
                }
                publisher.flush().get(10, TimeUnit.SECONDS);
            }
            for (int i = 0; i < 50; i++) {
                Message next = received.poll(10, TimeUnit.SECONDS);
                assertNotNull(next, "missing pipelined message " + i);
                assertEquals(Integer.toString(i), next.text(), "messages of one publisher stream stay ordered");
            }
        } finally {
            subscription.close();
        }
    }

    @Test
    void aclDenialsAreTypedErrors() throws Exception {
        ExecutionException publish = assertThrows(ExecutionException.class,
                () -> backend.publish("forbidden.subject", bytes("x")).get(10, TimeUnit.SECONDS));
        MessagingException denied = assertInstanceOf(MessagingException.class, publish.getCause());
        assertEquals(MessagingException.PERMISSION, denied.code());
        ExecutionException events = assertThrows(ExecutionException.class,
                () -> peer.publish("rift.events.permissions", bytes("spoof")).get(10, TimeUnit.SECONDS));
        assertEquals(MessagingException.PERMISSION, ((MessagingException) events.getCause()).code());
        ExecutionException subscribe = assertThrows(ExecutionException.class,
                () -> backend.subscribe("rift.perms.hello", null, message -> {}).get(10, TimeUnit.SECONDS));
        assertEquals(MessagingException.PERMISSION, ((MessagingException) subscribe.getCause()).code());
        // Whatever the grants say, only the proxy publishes permission updates.
        for (String subject : new String[] {"rift.perms.update", "rift.perms.update.resync",
                "rift.perms.update.0123456789abcdef0123456789abcdef"}) {
            ExecutionException forged = assertThrows(ExecutionException.class,
                    () -> backend.publish(subject, bytes("{}")).get(10, TimeUnit.SECONDS), subject);
            assertEquals(MessagingException.PERMISSION, ((MessagingException) forged.getCause()).code(), subject);
        }
        // The shared publisher recovers after the server closed its stream with ERROR.
        peer.publish("test.after.denial", bytes("ok")).get(10, TimeUnit.SECONDS);
    }

    @Test
    void requestsReportNoRespondersAndTimeouts() throws Exception {
        ExecutionException none = assertThrows(ExecutionException.class,
                () -> backend.request("test.nobody", bytes("x"), Duration.ofSeconds(2)).get(10, TimeUnit.SECONDS));
        assertEquals("no responders available", none.getCause().getMessage());
        Subscription silent = peer.subscribe("test.silent", null, message -> {}).get(10, TimeUnit.SECONDS);
        try {
            CompletionException timeout = assertThrows(CompletionException.class,
                    () -> backend.request("test.silent", bytes("x"), Duration.ofMillis(300)).join());
            assertInstanceOf(TimeoutException.class, timeout.getCause());
        } finally {
            silent.close();
        }
    }
}
