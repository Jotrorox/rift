package io.github.jotrorox.rift.network;

import java.util.Objects;

/** Holds the installed {@link RiftNetwork} behind {@link RiftNetwork#get()}. */
public final class RiftNetworkProvider {
    private static volatile RiftNetwork instance;

    private RiftNetworkProvider() {}

    /**
     * @return the installed service
     * @throws IllegalStateException when none is installed, for example before RiftPerms enabled
     */
    public static RiftNetwork get() {
        RiftNetwork current = instance;
        if (current == null) {
            throw new IllegalStateException(
                    "The Rift network API is not installed; depend on RiftPerms or install an implementation");
        }
        return current;
    }

    /**
     * Install the service. Called by the platform adapter or a test setup.
     *
     * @param network the implementation
     */
    public static synchronized void register(RiftNetwork network) {
        instance = Objects.requireNonNull(network, "network");
    }

    /**
     * Remove the service if it is still the installed one.
     *
     * @param network the implementation that was registered
     */
    public static synchronized void unregister(RiftNetwork network) {
        if (instance == network) {
            instance = null;
        }
    }
}
