package io.github.jotrorox.rift.permissions;

import java.util.Optional;
import java.util.concurrent.CompletableFuture;

/**
 * Writes for one group. Each method sends one {@link Change} through
 * {@link RiftPermissions#apply(Change)}.
 */
public final class GroupHandle {
    private final RiftPermissions permissions;
    private final String name;

    GroupHandle(RiftPermissions permissions, String name) {
        this.permissions = permissions;
        this.name = Names.group(name);
    }

    /** @return the normalised group name */
    public String name() {
        return name;
    }

    /** @return the group as last fetched, if it exists */
    public Optional<GroupInfo> info() {
        return permissions.groups().stream().filter(g -> g.name().equals(name)).findFirst();
    }

    /** @return the committed result of creating the group with weight 0 */
    public CompletableFuture<Applied> create() {
        return create(0, null);
    }

    /**
     * @param weight the weight
     * @param displayName the display name, or null
     * @return the committed result
     */
    public CompletableFuture<Applied> create(int weight, String displayName) {
        return permissions.apply(new Change.CreateGroup(name, weight, displayName));
    }

    /** @return the committed result of deleting the group and its nodes */
    public CompletableFuture<Applied> delete() {
        return permissions.apply(new Change.DeleteGroup(name));
    }

    /**
     * @param node a node, replacing the one in its slot
     * @return the committed result
     */
    public CompletableFuture<Applied> set(Node node) {
        return permissions.apply(Change.setNode(SubjectRef.group(name), node));
    }

    /**
     * @param node a node whose slot (kind, storage key, contexts) to clear
     * @return the committed result
     */
    public CompletableFuture<Applied> unset(Node node) {
        return permissions.apply(Change.unsetNode(SubjectRef.group(name), node));
    }

    /**
     * @param weight higher weights rank first
     * @return the committed result
     */
    public CompletableFuture<Applied> setWeight(int weight) {
        return permissions.apply(new Change.SetWeight(name, weight));
    }

    /**
     * @param displayName the display name, or null to clear it
     * @return the committed result
     */
    public CompletableFuture<Applied> setDisplayName(String displayName) {
        return permissions.apply(new Change.SetDisplayName(name, displayName));
    }

    /**
     * @param priority the highest priority wins
     * @param text the prefix
     * @return the committed result
     */
    public CompletableFuture<Applied> setPrefix(int priority, String text) {
        return set(Node.prefix(priority, text));
    }

    /**
     * @param priority the highest priority wins
     * @param text the suffix
     * @return the committed result
     */
    public CompletableFuture<Applied> setSuffix(int priority, String text) {
        return set(Node.suffix(priority, text));
    }

    /**
     * @param key the meta key
     * @param value the value
     * @return the committed result
     */
    public CompletableFuture<Applied> setMeta(String key, String value) {
        return set(Node.meta(key, value));
    }

    /**
     * @param parent a group to inherit globally
     * @return the committed result
     */
    public CompletableFuture<Applied> addParent(String parent) {
        return set(Node.inherit(parent, true));
    }

    /**
     * @param parent a global parent to remove
     * @return the committed result
     */
    public CompletableFuture<Applied> removeParent(String parent) {
        return unset(Node.inherit(parent, true));
    }
}
