package io.github.jotrorox.rift.messaging;

import java.time.Instant;
import java.util.Objects;

/**
 * A record fetched from a durable consumer. Acknowledge it after processing; otherwise it is
 * delivered again once the consumer's acknowledgement deadline has passed.
 *
 * @param sequence the record's sequence number in its stream
 * @param timestampMillis the time Rift stored the record, in Unix milliseconds
 * @param deliveryCount how often this record has been delivered, starting at 1
 * @param message the stored message
 */
public record Delivery(long sequence, long timestampMillis, long deliveryCount, Message message) {
    public Delivery {
        Objects.requireNonNull(message, "message");
    }

    /** @return the time Rift stored the record */
    public Instant timestamp() {
        return Instant.ofEpochMilli(timestampMillis);
    }

    /** @return whether this record was delivered before without being acknowledged */
    public boolean isRedelivery() {
        return deliveryCount > 1;
    }
}
