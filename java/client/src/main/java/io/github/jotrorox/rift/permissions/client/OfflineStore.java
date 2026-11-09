package io.github.jotrorox.rift.permissions.client;

import io.github.jotrorox.rift.permissions.CompiledSubject;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.AtomicMoveNotSupportedException;
import java.nio.file.Files;
import java.nio.file.NoSuchFileException;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.util.List;
import java.util.UUID;
import java.util.logging.Logger;

/**
 * Loads and saves the files behind an {@link OfflinePolicy}, in the Rust client's formats: the
 * last snapshot is the {@code subject.batch} reply shape, the fallback file groups and users in
 * typed nodes ({@link Fallback}).
 */
final class OfflineStore {
    private final OfflinePolicy policy;
    private final Protocol.SubjectBatchReply snapshot;
    private final Fallback fallback;

    private OfflineStore(OfflinePolicy policy, Protocol.SubjectBatchReply snapshot, Fallback fallback) {
        this.policy = policy;
        this.snapshot = snapshot;
        this.fallback = fallback;
    }

    /**
     * A missing or unreadable snapshot file starts empty (and is logged); a fallback file must
     * exist and be valid.
     *
     * @throws IOException when the fallback file cannot be read or is invalid
     */
    static OfflineStore load(OfflinePolicy policy, Logger logger) throws IOException {
        return switch (policy) {
            case OfflinePolicy.Deny deny -> new OfflineStore(deny, null, null);
            case OfflinePolicy.LastSnapshot last -> new OfflineStore(last, readSnapshot(last.file(), logger), null);
            case OfflinePolicy.FallbackFile file -> {
                try {
                    yield new OfflineStore(file, null,
                            Fallback.parse(Json.parse(Files.readString(file.file(), StandardCharsets.UTF_8))));
                } catch (IllegalArgumentException e) {
                    throw new IOException(file.file() + ": " + e.getMessage(), e);
                }
            }
        };
    }

    private static Protocol.SubjectBatchReply readSnapshot(Path file, Logger logger) {
        try {
            Protocol.SubjectBatchReply snapshot =
                    Codec.readSubjectBatchReply(Json.parse(Files.readString(file, StandardCharsets.UTF_8)));
            logger.info("Loaded " + snapshot.subjects().size() + " permission snapshots at revision "
                    + snapshot.revision() + " from " + file);
            return snapshot;
        } catch (NoSuchFileException e) {
            return null;
        } catch (IOException | IllegalArgumentException e) {
            logger.warning("Ignoring unreadable permission snapshot " + file + ": " + e.getMessage());
            return null;
        }
    }

    /** @return the snapshot loaded for {@code last_snapshot}, or null */
    Protocol.SubjectBatchReply snapshot() {
        return snapshot;
    }

    /** @return whether a fallback file answers for players without a snapshot */
    boolean hasFallback() {
        return fallback != null;
    }

    /**
     * @param uuid a player without a snapshot from the proxy
     * @param now Unix seconds
     * @return the fallback file's subject for the player at revision 0
     */
    CompiledSubject fallback(UUID uuid, long now) {
        return fallback.subject(uuid, now);
    }

    /** Persist the replica for {@code last_snapshot}, atomically; other policies write nothing. */
    void save(long revision, List<CompiledSubject> subjects) throws IOException {
        if (!(policy instanceof OfflinePolicy.LastSnapshot last)) {
            return;
        }
        Path file = last.file().toAbsolutePath();
        Files.createDirectories(file.getParent());
        Path temporary = file.resolveSibling(file.getFileName() + ".tmp");
        Files.writeString(temporary, Codec.toJson(new Protocol.SubjectBatchReply(revision, subjects)).toString(),
                StandardCharsets.UTF_8);
        try {
            Files.move(temporary, file, StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE);
        } catch (AtomicMoveNotSupportedException e) {
            Files.move(temporary, file, StandardCopyOption.REPLACE_EXISTING);
        }
    }
}
