package io.github.jotrorox.rift.permissions.client;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

import io.github.jotrorox.rift.messaging.ConsumerConfig;
import io.github.jotrorox.rift.messaging.Delivery;
import io.github.jotrorox.rift.messaging.MessagingClient;
import io.github.jotrorox.rift.messaging.MessagingException;
import io.github.jotrorox.rift.messaging.StoredRecord;
import io.github.jotrorox.rift.messaging.StreamConsumer;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.time.Duration;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;

/** Retained streams against the real broker: append, durable consumers, redelivery and ACLs. */
class StreamInteropTest {
    private static RiftProcess rift;
    private static MessagingClient backend;
    private static MessagingClient operator;

    @BeforeAll
    static void start() throws Exception {
        assumeTrue(Files.isExecutable(RiftProcess.binary()), "build the proxy first: cargo build --bin rift");
        rift = RiftProcess.start();
        backend = rift.connect(RiftProcess.SURVIVAL_TOKEN);
        operator = rift.connect(RiftProcess.OPERATOR_TOKEN);
        backend.connected().get(20, TimeUnit.SECONDS);
        operator.connected().get(20, TimeUnit.SECONDS);
    }

    @AfterAll
    static void stop() throws Exception {
        if (rift != null) {
            backend.close();
            operator.close();
            rift.close();
        }
    }

    private static byte[] bytes(String text) {
        return text.getBytes(StandardCharsets.UTF_8);
    }

    private static <T> T get(CompletableFuture<T> future) throws Exception {
        return future.get(10, TimeUnit.SECONDS);
    }

    private static MessagingException refused(CompletableFuture<?> future) {
        ExecutionException error = assertThrows(ExecutionException.class, () -> get(future));
        return assertInstanceOf(MessagingException.class, error.getCause(), () -> "failed with " + error.getCause());
    }

    @Test
    void recordsReplayAndAreRedeliveredUntilAcknowledged() throws Exception {
        StoredRecord first = get(backend.streamPublish("jobs", "test.jobs.build", bytes("a")));
        StoredRecord second = get(backend.streamPublish("jobs", "test.jobs.build", "_INBOX.jobs.replies", bytes("b")));
        assertEquals(first.sequence() + 1, second.sequence());
        assertTrue(first.timestampMillis() > 0);

        ConsumerConfig config = ConsumerConfig.builder("test.jobs.>")
                .startSequence(first.sequence())
                .ackWait(Duration.ofMillis(300))
                .build();
        StreamConsumer consumer = get(backend.consumer("jobs", "builder", config));
        assertEquals("jobs", consumer.stream());
        assertEquals("builder", consumer.name());

        List<Delivery> deliveries = get(consumer.fetch(10));
        assertEquals(2, deliveries.size());
        assertEquals(first.sequence(), deliveries.get(0).sequence());
        assertEquals("test.jobs.build", deliveries.get(0).message().subject());
        assertEquals("a", deliveries.get(0).message().text());
        assertEquals(1, deliveries.get(0).deliveryCount());
        assertFalse(deliveries.get(0).isRedelivery());
        assertEquals("_INBOX.jobs.replies", deliveries.get(1).message().reply());
        get(consumer.ack(deliveries.get(0)));

        Thread.sleep(600);
        // Reopening with the same settings resumes the consumer rather than starting over.
        StreamConsumer reopened = get(backend.consumer("jobs", "builder", config));
        List<Delivery> again = get(reopened.fetch(10));
        assertEquals(1, again.size());
        assertEquals(second.sequence(), again.get(0).sequence());
        assertTrue(again.get(0).isRedelivery());
        get(reopened.ack(again.get(0)));
        assertTrue(get(consumer.fetch(10)).isEmpty());

        get(consumer.delete());
        assertEquals(MessagingException.OPERATION, refused(consumer.fetch(1)).code());
    }

    @Test
    void theBrokerRefusesWhatItDoesNotStore() {
        assertEquals(
                MessagingException.OPERATION,
                refused(backend.streamPublish("missing", "test.jobs.x", new byte[0])).code());
        // The stream stores test.jobs.> only.
        refused(backend.streamPublish("jobs", "test.other", new byte[0]));
        refused(backend.consumer("missing", "builder", ConsumerConfig.of("test.jobs.>")));
        // The operator has no grant under test.>.
        assertEquals(
                MessagingException.PERMISSION,
                refused(operator.streamPublish("jobs", "test.jobs.x", new byte[0])).code());
        assertEquals(
                MessagingException.PERMISSION,
                refused(operator.consumer("jobs", "spy", ConsumerConfig.of("test.jobs.>"))).code());
        assertThrows(
                IllegalArgumentException.class,
                () -> backend.consumer("jobs", "no spaces", ConsumerConfig.of("test.jobs.>")));
    }
}
