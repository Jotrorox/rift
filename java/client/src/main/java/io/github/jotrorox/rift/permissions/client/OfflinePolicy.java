package io.github.jotrorox.rift.permissions.client;

import java.nio.file.Path;
import java.util.Objects;

/**
 * What a player gets when no snapshot came from the proxy, because it is unreachable or did not
 * answer within the join timeout. Players whose snapshot is already in the replica keep being
 * served from it either way. The files are the Rust client's ({@code docs/permissions-clients.md}).
 */
public sealed interface OfflinePolicy {
    /** @return answer undefined, so plugin defaults and operator status still apply */
    static OfflinePolicy deny() {
        return new Deny();
    }

    /**
     * @param file where the replica is written on {@link PermissionsClient#close()}, atomically, as
     *     {@code {"revision": n, "subjects": [compiled subject, ...]}} (the {@code subject.batch}
     *     reply shape); loaded on start, and a missing or unreadable file starts empty
     * @return keep serving the players known before a restart while the proxy is away
     */
    static OfflinePolicy lastSnapshot(Path file) {
        return new LastSnapshot(file);
    }

    /**
     * @param file groups and users for emergencies, as {@code {"groups": [{"name", "weight"?,
     *     "display_name"?, "nodes": [typed node, ...]}, ...], "users": [{"uuid", "primary_group"?,
     *     "nodes": [...]}, ...]}}; read and validated when the client is built
     * @return compile such players locally exactly as the proxy would, at revision 0 so that any
     *     snapshot from the proxy replaces it; a player the file does not list gets {@code default}
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
     * @param file the fallback file
     */
    record FallbackFile(Path file) implements OfflinePolicy {
        public FallbackFile {
            Objects.requireNonNull(file, "file");
        }
    }
}
