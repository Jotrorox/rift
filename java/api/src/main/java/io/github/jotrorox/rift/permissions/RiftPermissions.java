package io.github.jotrorox.rift.permissions;

import java.util.List;
import java.util.Optional;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;

/**
 * Rift's permission service. Reads are synchronous and answered from the local replica of the
 * proxy's data, without network; writes go to the proxy and complete when it has committed them,
 * failing with {@link PermissionTimeoutException} after the client timeout.
 *
 * <pre>{@code
 * RiftPermissions perms = RiftPermissions.get();
 * boolean fly = perms.check(player, "essentials.fly").asBoolean(false);
 * perms.user(uuid).set(Node.permission("essentials.fly", true).expiresIn(Duration.ofHours(1)))
 *      .thenAccept(applied -> log.info("revision " + applied.revision()));
 * }</pre>
 *
 * <p>Methods taking an {@code Object player} accept the platform's player type
 * ({@code org.bukkit.entity.Player} on Paper) and use that player's current contexts.
 */
public interface RiftPermissions {
    /** The group every user inherits implicitly; it always exists. */
    String DEFAULT_GROUP = "default";

    /**
     * @return the installed service
     * @throws IllegalStateException when no implementation is installed
     */
    static RiftPermissions get() {
        return RiftPermissionsProvider.get();
    }

    /** @return the last store revision this server has seen */
    long revision();

    /**
     * @param uuid the player
     * @param node the permission, lowercased before matching
     * @param contexts the active contexts
     * @return the answer; {@link Tristate#UNDEFINED} when nothing matches, the node is invalid or
     *     the player is unknown here
     */
    Tristate check(UUID uuid, String node, ContextSet contexts);

    /**
     * @param player a platform player
     * @param node the permission
     * @return the answer in the player's current contexts
     */
    default Tristate check(Object player, String node) {
        return check(contexts().uuid(player), node, contexts().of(player));
    }

    /**
     * @param uuid the player
     * @param node the permission
     * @param contexts the active contexts
     * @return the answer with the winning entry and the reason every other match lost
     */
    Trace trace(UUID uuid, String node, ContextSet contexts);

    /**
     * @param uuid the player
     * @param contexts the active contexts
     * @return prefix, suffix, meta and primary group; empty values when the player is unknown here
     */
    MetaView meta(UUID uuid, ContextSet contexts);

    /**
     * @param player a platform player
     * @return the meta view in the player's current contexts
     */
    default MetaView meta(Object player) {
        return meta(contexts().uuid(player), contexts().of(player));
    }

    /**
     * @param uuid the player
     * @param contexts the active contexts
     * @return the groups the player is in, directly or inherited, in rank order
     */
    List<String> groupsOf(UUID uuid, ContextSet contexts);

    /**
     * @param uuid the player
     * @param group a group name
     * @param contexts the active contexts
     * @return whether the player is in {@code group}, directly or inherited
     */
    boolean inGroup(UUID uuid, String group, ContextSet contexts);

    /**
     * @param uuid the player
     * @return the snapshot held for the player, if any
     */
    Optional<CompiledSubject> compiled(UUID uuid);

    /**
     * Fetch a player's snapshot from the proxy, for players who are not online here.
     *
     * @param uuid the player
     * @return the snapshot, which every UUID has
     */
    CompletableFuture<CompiledSubject> load(UUID uuid);

    /** @return every group with weight, display name and chat nodes, as last fetched */
    List<GroupInfo> groups();

    /**
     * Apply a change as this server.
     *
     * @param change the change
     * @return the committed result
     */
    CompletableFuture<Applied> apply(Change change);

    /**
     * Apply a change on behalf of a player, who must be online here and hold the matching
     * {@code rift.perms.*} node on the proxy.
     *
     * @param actor the player, or null to act as this server
     * @param change the change
     * @param reason free text for the audit log, or null
     * @return the committed result
     */
    CompletableFuture<Applied> apply(Actor.Player actor, Change change, String reason);

    /**
     * @param uuid the player
     * @return a handle for writing the player's nodes
     */
    default UserHandle user(UUID uuid) {
        return new UserHandle(this, uuid);
    }

    /**
     * @param name the group
     * @return a handle for writing the group
     */
    default GroupHandle group(String name) {
        return new GroupHandle(this, name);
    }

    /**
     * @param name the track
     * @return a handle for writing and moving along the track
     */
    default TrackHandle track(String name) {
        return new TrackHandle(this, name);
    }

    /** @return change and update events */
    EventBus events();

    /** @return player contexts and custom calculators */
    Contexts contexts();
}
