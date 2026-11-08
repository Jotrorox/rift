package io.github.jotrorox.rift.permissions.client;

import io.github.jotrorox.rift.permissions.CompiledSubject;
import io.github.jotrorox.rift.permissions.GroupInfo;
import io.github.jotrorox.rift.permissions.PermissionChangeEvent;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.UUID;
import java.util.concurrent.ConcurrentHashMap;

/**
 * A backend's copy of compiled subjects and the group table, kept current from
 * {@code rift.perms.*} messages; a port of the Rust crate's {@code Replica}. Reads never lock and
 * never wait on the network.
 *
 * <ul>
 *   <li>A snapshot replaces the stored one only when its revision is newer, so duplicated or
 *       reordered pushes are harmless.
 *   <li>The replica's revision follows {@code hello} and change events, never snapshots. An event
 *       at exactly the next revision lists the stored subjects to refetch; an older one is ignored;
 *       a newer one reveals a lost message and asks for a resync.
 * </ul>
 */
public final class Replica {
    /** How long a player's snapshot is kept after they leave, in seconds. */
    public static final long EVICT_AFTER = 600;

    /**
     * The group table and the revision it was fetched at.
     *
     * @param revision the proxy's revision in the groups reply
     * @param groups the groups
     */
    public record Groups(long revision, List<GroupInfo> groups) {
        public Groups {
            groups = List.copyOf(groups);
        }
    }

    /** A stored snapshot; {@code leftAt} is when the player left this backend, or null. */
    private record Slot(CompiledSubject subject, Long leftAt) {}

    private final ConcurrentHashMap<UUID, Slot> subjects = new ConcurrentHashMap<>();
    private final Object revisionLock = new Object();
    private volatile long revision;
    private volatile Groups groups = new Groups(0, List.of());

    /** @return the last revision seen from {@code hello} or a change event */
    public long revision() {
        return revision;
    }

    /**
     * Adopt the proxy's revision from a {@code hello} reply.
     *
     * @param proxy the proxy's revision
     * @return true when it differs from a non-empty replica's, so every stored subject is stale
     */
    public boolean syncRevision(long proxy) {
        synchronized (revisionLock) {
            boolean resync = revision != proxy && !subjects.isEmpty();
            revision = proxy;
            return resync;
        }
    }

    /**
     * Store a snapshot if it is newer than the stored one for that player, keeping the player's
     * eviction state.
     *
     * @param subject the snapshot
     * @return whether it was stored
     */
    public boolean insert(CompiledSubject subject) {
        boolean[] stored = {false};
        subjects.compute(subject.uuid(), (uuid, slot) -> {
            if (slot != null && slot.subject().revision() >= subject.revision()) {
                return slot;
            }
            stored[0] = true;
            return new Slot(subject, slot == null ? null : slot.leftAt());
        });
        return stored[0];
    }

    /**
     * @param uuid a player
     * @return the stored snapshot
     */
    public Optional<CompiledSubject> get(UUID uuid) {
        Slot slot = subjects.get(uuid);
        return slot == null ? Optional.empty() : Optional.of(slot.subject());
    }

    /**
     * @param uuid a player
     * @return whether a snapshot is stored
     */
    public boolean contains(UUID uuid) {
        return subjects.containsKey(uuid);
    }

    /**
     * @param uuid a player
     * @return the removed snapshot
     */
    public Optional<CompiledSubject> remove(UUID uuid) {
        Slot slot = subjects.remove(uuid);
        return slot == null ? Optional.empty() : Optional.of(slot.subject());
    }

    /** @return the number of stored snapshots */
    public int size() {
        return subjects.size();
    }

    /** @return whether nothing is stored */
    public boolean isEmpty() {
        return subjects.isEmpty();
    }

    /** @return every stored player, for a resync */
    public List<UUID> uuids() {
        return List.copyOf(subjects.keySet());
    }

    /** @return every stored snapshot, for persisting the replica */
    public List<CompiledSubject> subjects() {
        return subjects.values().stream().map(Slot::subject).toList();
    }

    /**
     * Apply a change event from {@code rift.perms.update}.
     *
     * @param event the event
     * @return what the caller should refetch
     */
    public UpdateOutcome applyUpdate(PermissionChangeEvent event) {
        synchronized (revisionLock) {
            if (event.revision() <= revision) {
                return new UpdateOutcome.Stale();
            }
            boolean gap = event.revision() != revision + 1;
            revision = event.revision();
            if (gap) {
                return new UpdateOutcome.Gap(uuids());
            }
            List<UUID> refetch = new ArrayList<>();
            for (UUID uuid : event.changed()) {
                Slot slot = subjects.get(uuid);
                if (slot != null && slot.subject().revision() < event.revision()) {
                    refetch.add(uuid);
                }
            }
            return new UpdateOutcome.Applied(refetch, !event.groups().isEmpty());
        }
    }

    /**
     * Mark a player as gone from this backend; the snapshot stays until {@link #evict} removes it.
     *
     * @param uuid the player
     * @param now Unix seconds
     */
    public void markLeft(UUID uuid, long now) {
        subjects.computeIfPresent(uuid, (key, slot) -> new Slot(slot.subject(), now));
    }

    /**
     * Cancel a pending eviction when the player is back.
     *
     * @param uuid the player
     */
    public void markJoined(UUID uuid) {
        subjects.computeIfPresent(uuid, (key, slot) -> new Slot(slot.subject(), null));
    }

    /**
     * Remove players who left at least {@code after} seconds before {@code now}.
     *
     * @param now Unix seconds
     * @param after normally {@link #EVICT_AFTER}
     * @return the removed players
     */
    public List<UUID> evict(long now, long after) {
        List<UUID> gone = new ArrayList<>();
        subjects.forEach((uuid, slot) -> {
            if (slot.leftAt() != null && slot.leftAt() + after <= now
                    && subjects.remove(uuid, slot)) {
                gone.add(uuid);
            }
        });
        return gone;
    }

    /**
     * Replace the group table unless {@code revision} is older than the stored one.
     *
     * @param revision the proxy's revision in the groups reply
     * @param table the groups
     * @return whether it was stored
     */
    public boolean setGroups(long revision, List<GroupInfo> table) {
        synchronized (revisionLock) {
            if (revision < groups.revision()) {
                return false;
            }
            groups = new Groups(revision, table);
            return true;
        }
    }

    /** @return the group table and the revision it was fetched at */
    public Groups groups() {
        return groups;
    }
}
