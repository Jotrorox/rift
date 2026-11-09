package io.github.jotrorox.rift.messaging;

import java.nio.charset.StandardCharsets;
import java.util.Objects;

/**
 * A delivered message.
 *
 * @param subject the literal subject it was published on
 * @param reply the reply subject a responder publishes to, or null
 * @param payload the bytes, at most 1 MiB
 */
public record Message(String subject, String reply, byte[] payload) {
    public Message {
        Objects.requireNonNull(subject, "subject");
        Objects.requireNonNull(payload, "payload");
    }

    /** @return the payload decoded as UTF-8 */
    public String text() {
        return new String(payload, StandardCharsets.UTF_8);
    }
}
