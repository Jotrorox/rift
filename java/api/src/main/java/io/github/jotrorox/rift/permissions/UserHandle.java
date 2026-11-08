package io.github.jotrorox.rift.permissions;

import java.time.Duration;
import java.util.Objects;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;

/**
 * Writes for one player. Each method sends one {@link Change} through
 * {@link RiftPermissions#apply(Change)}.
 */
public final class UserHandle {
    private final RiftPermissions permissions;
    private final UUID uuid;

    UserHandle(RiftPermissions permissions, UUID uuid) {
        this.permissions = permissions;
        this.uuid = Objects.requireNonNull(uuid, "uuid");
    }

    /** @return the player */
    public UUID uuid() {
        return uuid;
    }

    private SubjectRef subject() {
        return SubjectRef.user(uuid);
    }

    /**
     * @param node a node, replacing the one in its slot
     * @return the committed result
     */
    public CompletableFuture<Applied> set(Node node) {
        return permissions.apply(Change.setNode(subject(), node));
    }

    /**
     * @param node a node whose slot (kind, storage key, contexts) to clear
     * @return the committed result
     */
    public CompletableFuture<Applied> unset(Node node) {
        return permissions.apply(Change.unsetNode(subject(), node));
    }

    /**
     * @param group the group to inherit globally and permanently
     * @return the committed result
     */
    public CompletableFuture<Applied> addParent(String group) {
        return set(Node.inherit(group, true));
    }

    /**
     * @param group the group to inherit globally
     * @param duration how long from now
     * @return the committed result
     */
    public CompletableFuture<Applied> addParent(String group, Duration duration) {
        return set(Node.inherit(group, true).expiresIn(duration));
    }

    /**
     * @param group a global parent to remove
     * @return the committed result
     */
    public CompletableFuture<Applied> removeParent(String group) {
        return unset(Node.inherit(group, true));
    }

    /**
     * @param group the explicit primary group, or null to compute it again
     * @return the committed result
     */
    public CompletableFuture<Applied> setPrimaryGroup(String group) {
        return permissions.apply(new Change.SetPrimaryGroup(uuid, group));
    }

    /** @return the committed result of removing every node of the player */
    public CompletableFuture<Applied> clear() {
        return permissions.apply(new Change.ClearSubject(subject(), null));
    }
}
