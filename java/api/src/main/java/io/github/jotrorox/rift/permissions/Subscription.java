package io.github.jotrorox.rift.permissions;

/** A registration that ends when closed. Closing twice does nothing. */
@FunctionalInterface
public interface Subscription extends AutoCloseable {
    @Override
    void close();
}
