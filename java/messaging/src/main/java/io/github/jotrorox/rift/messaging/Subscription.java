package io.github.jotrorox.rift.messaging;

/** A live subscription; it is re-established after every reconnect until closed. */
public interface Subscription extends AutoCloseable {
    /** @return the subscribed pattern */
    String pattern();

    /** Stop the subscription and release its stream. Closing twice does nothing. */
    @Override
    void close();
}
