package io.github.jotrorox.rift.permissions;

import java.util.List;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;

/**
 * Writes for one track, an ordered list of groups such as {@code default → vip → mod}. A user on
 * none of its groups counts as being on {@code default} when the track lists it.
 */
public final class TrackHandle {
    private final RiftPermissions permissions;
    private final String name;

    TrackHandle(RiftPermissions permissions, String name) {
        this.permissions = permissions;
        this.name = Names.track(name);
    }

    /** @return the normalised track name */
    public String name() {
        return name;
    }

    /**
     * @param groups distinct groups, in order
     * @return the committed result of creating or replacing the track
     */
    public CompletableFuture<Applied> set(List<String> groups) {
        return permissions.apply(new Change.SetTrack(name, groups));
    }

    /** @return the committed result of deleting the track */
    public CompletableFuture<Applied> delete() {
        return permissions.apply(new Change.DeleteTrack(name));
    }

    /**
     * @param user the player to move to the next group, globally
     * @return the committed result
     */
    public CompletableFuture<Applied> promote(UUID user) {
        return promote(user, ContextSet.empty());
    }

    /**
     * @param user the player
     * @param contexts the contexts of the moved inherit node
     * @return the committed result
     */
    public CompletableFuture<Applied> promote(UUID user, ContextSet contexts) {
        return permissions.apply(new Change.Promote(user, name, contexts));
    }

    /**
     * @param user the player to move to the previous group, globally
     * @return the committed result
     */
    public CompletableFuture<Applied> demote(UUID user) {
        return demote(user, ContextSet.empty());
    }

    /**
     * @param user the player
     * @param contexts the contexts of the moved inherit node
     * @return the committed result
     */
    public CompletableFuture<Applied> demote(UUID user, ContextSet contexts) {
        return permissions.apply(new Change.Demote(user, name, contexts));
    }
}
