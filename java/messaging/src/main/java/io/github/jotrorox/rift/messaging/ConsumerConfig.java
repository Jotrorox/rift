package io.github.jotrorox.rift.messaging;

import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.Objects;

/**
 * Settings of a durable stream consumer. Reopen a consumer with the same settings to resume where
 * it left off.
 *
 * @param filterSubject the subject pattern selecting records; the identity's subscribe grants must
 *     cover it
 * @param startSequence the first sequence to deliver when the consumer is created
 * @param ackWait how long a fetched record stays reserved before it is delivered again
 * @param maxAckPending the most records that may be fetched but not yet acknowledged
 */
public record ConsumerConfig(String filterSubject, long startSequence, Duration ackWait, int maxAckPending) {
    /** The first record of a stream. */
    public static final long DEFAULT_START_SEQUENCE = 1;
    /** Rift's default acknowledgement deadline. */
    public static final Duration DEFAULT_ACK_WAIT = Duration.ofSeconds(30);
    /** Rift's default limit of unacknowledged records. */
    public static final int DEFAULT_MAX_ACK_PENDING = 128;

    /**
     * @throws IllegalArgumentException when the pattern is malformed or a limit is out of range
     */
    public ConsumerConfig {
        Objects.requireNonNull(filterSubject, "filterSubject");
        Objects.requireNonNull(ackWait, "ackWait");
        if (!isPattern(filterSubject)) {
            throw new IllegalArgumentException("invalid subject pattern: " + filterSubject);
        }
        if (startSequence < 1) {
            throw new IllegalArgumentException("startSequence must be at least 1");
        }
        if (ackWait.isNegative()
                || ackWait.isZero()
                || ackWait.compareTo(Duration.ofMillis(Frames.MAX_ACK_WAIT_MILLIS)) > 0
                || ackWait.getNano() % 1_000_000 != 0) {
            throw new IllegalArgumentException("ackWait must be 1 ms to 1 hour in whole milliseconds");
        }
        if (maxAckPending < 1 || maxAckPending > Frames.MAX_ACK_PENDING) {
            throw new IllegalArgumentException("maxAckPending must be 1 to " + Frames.MAX_ACK_PENDING);
        }
    }

    /**
     * @param filterSubject the subject pattern selecting records
     * @return a consumer of that pattern with Rift's default limits, starting at the first record
     */
    public static ConsumerConfig of(String filterSubject) {
        return builder(filterSubject).build();
    }

    /**
     * @param filterSubject the subject pattern selecting records
     * @return a builder starting from Rift's defaults
     */
    public static Builder builder(String filterSubject) {
        return new Builder(filterSubject);
    }

    /** Dot-separated tokens without whitespace; {@code *} is one token, {@code >} the last one. */
    private static boolean isPattern(String pattern) {
        if (pattern.isEmpty() || pattern.getBytes(StandardCharsets.UTF_8).length > Frames.MAX_SUBJECT) {
            return false;
        }
        String[] tokens = pattern.split("\\.", -1);
        for (int i = 0; i < tokens.length; i++) {
            String token = tokens[i];
            if (token.isEmpty()
                    || token.codePoints().anyMatch(c -> Character.isWhitespace(c)
                            || Character.isSpaceChar(c)
                            || Character.isISOControl(c))) {
                return false;
            }
            boolean wildcard = token.indexOf('*') >= 0 || token.indexOf('>') >= 0;
            if (wildcard && !token.equals("*") && !token.equals(">")) {
                return false;
            }
            if (token.equals(">") && i != tokens.length - 1) {
                return false;
            }
        }
        return true;
    }

    /** Builds a {@link ConsumerConfig}. */
    public static final class Builder {
        private final String filterSubject;
        private long startSequence = DEFAULT_START_SEQUENCE;
        private Duration ackWait = DEFAULT_ACK_WAIT;
        private int maxAckPending = DEFAULT_MAX_ACK_PENDING;

        private Builder(String filterSubject) {
            this.filterSubject = filterSubject;
        }

        /**
         * @param startSequence the first sequence to deliver when the consumer is created
         * @return this builder
         */
        public Builder startSequence(long startSequence) {
            this.startSequence = startSequence;
            return this;
        }

        /**
         * @param ackWait 1 ms to 1 hour, in whole milliseconds
         * @return this builder
         */
        public Builder ackWait(Duration ackWait) {
            this.ackWait = ackWait;
            return this;
        }

        /**
         * @param maxAckPending 1 to 65536
         * @return this builder
         */
        public Builder maxAckPending(int maxAckPending) {
            this.maxAckPending = maxAckPending;
            return this;
        }

        /**
         * @return the settings
         * @throws IllegalArgumentException when the pattern is malformed or a limit is out of range
         */
        public ConsumerConfig build() {
            return new ConsumerConfig(filterSubject, startSequence, ackWait, maxAckPending);
        }
    }
}
