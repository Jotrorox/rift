package io.github.jotrorox.rift.permissions;

import java.util.List;
import java.util.Objects;
import java.util.UUID;

/**
 * A committed write, as the proxy publishes it on {@code rift.perms.update}. Delivery is at most
 * once: subscribe for reactions, not for bookkeeping that must see every revision.
 *
 * @param revision the new revision
 * @param at commit time in Unix seconds
 * @param actor who made the change
 * @param reason free text, or null
 * @param ops the effects as before/after pairs
 * @param opsTruncated whether {@code ops} was cut short; the audit log has the full list
 * @param changed players whose compiled subjects changed
 * @param groups groups whose own data changed
 */
public record PermissionChangeEvent(
        long revision,
        long at,
        Actor actor,
        String reason,
        List<Op> ops,
        boolean opsTruncated,
        List<UUID> changed,
        List<String> groups) {
    public PermissionChangeEvent {
        Objects.requireNonNull(actor, "actor");
        ops = List.copyOf(ops);
        changed = List.copyOf(changed);
        groups = List.copyOf(groups);
    }

    /** One effect of a change; null on either side means absent. */
    public sealed interface Op {}

    /**
     * A node added, removed or replaced.
     *
     * @param subject the owner
     * @param before the old node, or null
     * @param after the new node, or null
     */
    public record NodeChange(SubjectRef subject, Node before, Node after) implements Op {}

    /**
     * A group's weight and display name.
     *
     * @param name the group
     * @param before the old columns, or null when the group was created
     * @param after the new columns, or null when the group was deleted
     */
    public record GroupChange(String name, GroupAttributes before, GroupAttributes after) implements Op {}

    /**
     * A track's group list.
     *
     * @param name the track
     * @param before the old list, or null
     * @param after the new list, or null
     */
    public record TrackChange(String name, List<String> before, List<String> after) implements Op {
        public TrackChange {
            before = before == null ? null : List.copyOf(before);
            after = after == null ? null : List.copyOf(after);
        }
    }

    /**
     * A user's explicit primary group.
     *
     * @param user the player
     * @param before the old group, or null
     * @param after the new group, or null
     */
    public record PrimaryGroupChange(UUID user, String before, String after) implements Op {}

    /**
     * The columns of a group.
     *
     * @param weight the weight
     * @param displayName the display name, or null
     */
    public record GroupAttributes(int weight, String displayName) {}
}
