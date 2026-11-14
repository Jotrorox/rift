package io.github.jotrorox.rift.network;

/** A registration that ends when closed. Closing twice does nothing. */
@FunctionalInterface
public interface Subscription extends AutoCloseable {
    @Override
    void close();
}
