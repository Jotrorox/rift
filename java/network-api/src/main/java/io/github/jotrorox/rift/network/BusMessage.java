package io.github.jotrorox.rift.network;

import java.nio.charset.StandardCharsets;
import java.util.Objects;
import java.util.Optional;

/**
 * A message from the bus.
 *
 * @param subject the literal subject it was published on
 * @param replyTo the subject a responder publishes its answer to, when the sender asked for one
 * @param payload the bytes
 */
public record BusMessage(String subject, Optional<String> replyTo, byte[] payload) {
    public BusMessage {
        Objects.requireNonNull(subject, "subject");
        Objects.requireNonNull(replyTo, "replyTo");
        Objects.requireNonNull(payload, "payload");
    }

    /** @return the payload decoded as UTF-8 */
    public String text() {
        return new String(payload, StandardCharsets.UTF_8);
    }
}
