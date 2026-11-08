package io.github.jotrorox.rift.permissions.client;

import io.github.jotrorox.rift.permissions.ContextSet;
import java.util.UUID;

/**
 * The host's view of its players, supplied by an adapter such as RiftPerms on Paper. Without one,
 * the client accepts {@link UUID}s as players and gives them only the {@code server} context plus
 * the registered calculators' pairs.
 */
public interface Platform {
    /**
     * @param player a host player object
     * @return its UUID
     * @throws IllegalArgumentException for objects that are not players of this host
     */
    UUID uuid(Object player);

    /**
     * @param player a host player object
     * @return its active contexts, including those of {@link PermissionsClient#calculators()}
     * @throws IllegalArgumentException for objects that are not players of this host
     */
    ContextSet contexts(Object player);
}
