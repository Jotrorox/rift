package io.github.jotrorox.rift.permissions;

import java.util.Objects;

/** Holds the installed {@link RiftPermissions} behind {@link RiftPermissions#get()}. */
public final class RiftPermissionsProvider {
    private static volatile RiftPermissions instance;

    private RiftPermissionsProvider() {}

    /**
     * @return the installed service
     * @throws IllegalStateException when none is installed, for example before RiftPerms enabled
     */
    public static RiftPermissions get() {
        RiftPermissions current = instance;
        if (current == null) {
            throw new IllegalStateException(
                    "Rift permissions are not installed; depend on RiftPerms or install an implementation");
        }
        return current;
    }

    /**
     * Install the service. Called by the platform adapter or a test setup.
     *
     * @param permissions the implementation
     */
    public static synchronized void register(RiftPermissions permissions) {
        instance = Objects.requireNonNull(permissions, "permissions");
    }

    /**
     * Remove the service if it is still the installed one.
     *
     * @param permissions the implementation that was registered
     */
    public static synchronized void unregister(RiftPermissions permissions) {
        if (instance == permissions) {
            instance = null;
        }
    }
}
