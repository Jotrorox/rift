package io.github.jotrorox.rift.messaging;

/**
 * Connection state changes, called on the client's connection thread. Keep them short; start
 * longer work asynchronously.
 */
public interface ConnectionListener {
    /** Authenticated, and every subscription is active again. */
    default void connected() {}

    /**
     * An established connection ended; the client reconnects with backoff.
     *
     * @param cause why
     */
    default void disconnected(Throwable cause) {}

    /**
     * A connection attempt failed; the client retries with backoff.
     *
     * @param cause why
     */
    default void connectFailed(Throwable cause) {}
}
