package io.github.jotrorox.rift.permissions.client;

import java.nio.file.Path;
import java.util.Objects;

/**
 * What a backend answers for a player whose snapshot it does not hold, because the proxy cannot be
 * reached or did not answer in time. Only such players are affected; held snapshots always win.
 */
public sealed interface OfflinePolicy {
    /** @return answer undefined, so plugin defaults and operator status still apply */
    static OfflinePolicy deny() {
        return new Deny();
    }

    /**
     * @param file where the replica is written on a clean shutdown, as
     *     {@code {"revision": n, "subjects": [compiled subject, ...]}}
     * @return serve the player's snapshot from the last clean shutdown, if it is there
     */
    static OfflinePolicy lastSnapshot(Path file) {
        return new LastSnapshot(file);
    }

    /**
     * @param file a static node list for emergencies, as
     *     {@code {"primary_group": "default", "nodes": [typed node, ...]}}
     * @return serve the same nodes to every such player
     */
    static OfflinePolicy fallbackFile(Path file) {
        return new FallbackFile(file);
    }

    /** See {@link #deny()}. */
    record Deny() implements OfflinePolicy {}

    /**
     * See {@link #lastSnapshot(Path)}.
     *
     * @param file the snapshot file
     */
    record LastSnapshot(Path file) implements OfflinePolicy {
        public LastSnapshot {
            Objects.requireNonNull(file, "file");
        }
    }

    /**
     * See {@link #fallbackFile(Path)}.
     *
     * @param file the fallback node list
     */
    record FallbackFile(Path file) implements OfflinePolicy {
        public FallbackFile {
            Objects.requireNonNull(file, "file");
        }
    }
}
