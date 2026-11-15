package io.github.jotrorox.rift.messaging;

import java.util.List;
import java.util.concurrent.CompletableFuture;

/**
 * A durable consumer of a retained stream, owned by the authenticated identity. Rift keeps its
 * position, so this handle stays usable across reconnects; calls fail fast while disconnected.
 *
 * <p>Delivery is at least once: acknowledge each record only after it was processed. Several
 * workers of the same identity can share a consumer name to divide its records.
 */
public interface StreamConsumer {
    /** @return the stream this consumer reads */
    String stream();

    /** @return the consumer name, scoped to the authenticated identity */
    String name();

    /**
     * Fetch the records that are available now, without waiting for new ones.
     *
     * @param limit 1 to 256
     * @return up to {@code limit} records in sequence order, empty when none is available
     */
    CompletableFuture<List<Delivery>> fetch(int limit);

    /**
     * @param sequence the sequence of a processed record
     * @return completes when Rift recorded the acknowledgement
     */
    CompletableFuture<Void> ack(long sequence);

    /**
     * @param delivery a processed delivery
     * @return completes when Rift recorded the acknowledgement
     */
    default CompletableFuture<Void> ack(Delivery delivery) {
        return ack(delivery.sequence());
    }

    /**
     * Delete the consumer on Rift, discarding its position and pending deliveries. One identity may
     * hold a quarter of a stream's {@code max_consumers}, so delete consumers that are no longer
     * used.
     *
     * @return completes when the consumer is gone
     */
    CompletableFuture<Void> delete();
}
