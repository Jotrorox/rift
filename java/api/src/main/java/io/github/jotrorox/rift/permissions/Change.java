package io.github.jotrorox.rift.permissions;

import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Objects;
import java.util.UUID;

/**
 * One write. Every surface (commands, Lua, HTTP, backends) builds a {@code Change}; the proxy applies
 * it in one transaction and one revision. The records normalise names and check structural rules
 * on construction, as the Rust {@code Change::normalized} does; store limits are the proxy's job.
 */
public sealed interface Change {
    /**
     * @param subject the owner
     * @param node the node, replacing the one in its slot
     * @return a {@link SetNode}
     */
    static Change setNode(SubjectRef subject, Node node) {
        return new SetNode(subject, node);
    }

    /**
     * @param subject the owner
     * @param node a node whose slot (kind, storage key, contexts) to clear
     * @return an {@link UnsetNode}
     */
    static Change unsetNode(SubjectRef subject, Node node) {
        return new UnsetNode(subject, node.kind(), node.payload().storageKey(), node.contexts());
    }

    /**
     * @param filter which nodes
     * @param patch what to do with them
     * @return a {@link Bulk}
     */
    static Change bulk(Filter filter, Patch patch) {
        return new Bulk(filter, patch);
    }

    /**
     * @param changes several changes, applied in one transaction and one revision
     * @return a {@link Batch}
     */
    static Change batch(Change... changes) {
        return new Batch(List.of(changes));
    }

    /**
     * Insert a node, replacing the one in its slot.
     *
     * @param subject the owner; {@code weight} and {@code display_name} nodes only on groups
     * @param node the node
     */
    record SetNode(SubjectRef subject, Node node) implements Change {
        public SetNode {
            Objects.requireNonNull(subject, "subject");
            Objects.requireNonNull(node, "node");
            if (node.kind().groupOnly() && subject instanceof SubjectRef.User) {
                throw Names.invalid("node", node.kind().wireName(), "applies to groups only");
            }
        }
    }

    /**
     * Remove the node in one slot.
     *
     * @param subject the owner
     * @param kind the node kind
     * @param key the storage key, see {@link Node.Payload#storageKey()}
     * @param contexts the slot's contexts
     */
    record UnsetNode(SubjectRef subject, NodeKind kind, String key, ContextSet contexts) implements Change {
        public UnsetNode {
            Objects.requireNonNull(subject, "subject");
            Objects.requireNonNull(contexts, "contexts");
            key = switch (kind) {
                case PERMISSION -> Names.node(key);
                case INHERIT -> Names.group(key);
                case PREFIX, SUFFIX -> {
                    try {
                        yield Integer.toString(Integer.parseInt(key));
                    } catch (NumberFormatException e) {
                        throw Names.invalid("priority", key, "expected a 32-bit integer");
                    }
                }
                case META -> Names.key("meta key", key);
                case WEIGHT, DISPLAY_NAME -> {
                    if (!key.equals(kind.wireName())) {
                        throw Names.invalid("node key", key, "must equal the kind name");
                    }
                    yield key;
                }
            };
        }
    }

    /**
     * Remove every node of a subject, or only those with exactly {@code contexts}.
     *
     * @param subject the owner
     * @param contexts only nodes with exactly these contexts, or null for all
     */
    record ClearSubject(SubjectRef subject, ContextSet contexts) implements Change {
        public ClearSubject {
            Objects.requireNonNull(subject, "subject");
        }
    }

    /**
     * Create a group.
     *
     * @param name the group
     * @param weight its weight
     * @param displayName its display name, or null
     */
    record CreateGroup(String name, int weight, String displayName) implements Change {
        public CreateGroup {
            name = Names.group(name);
            if (displayName != null) {
                new Node.DisplayName(displayName);
            }
        }
    }

    /**
     * Delete a group and its nodes; {@code inherit} nodes naming it stay and grant nothing.
     *
     * @param name the group, never {@code default}
     */
    record DeleteGroup(String name) implements Change {
        public DeleteGroup {
            name = Names.group(name);
            if (name.equals(RiftPermissions.DEFAULT_GROUP)) {
                throw Names.invalid("group", name, "the default group cannot be deleted");
            }
        }
    }

    /**
     * Set a group's weight.
     *
     * @param group the group
     * @param weight the weight
     */
    record SetWeight(String group, int weight) implements Change {
        public SetWeight {
            group = Names.group(group);
        }
    }

    /**
     * Set or clear a group's display name.
     *
     * @param group the group
     * @param displayName the display name, or null to clear it
     */
    record SetDisplayName(String group, String displayName) implements Change {
        public SetDisplayName {
            group = Names.group(group);
            if (displayName != null) {
                new Node.DisplayName(displayName);
            }
        }
    }

    /**
     * Set or clear a user's explicit primary group.
     *
     * @param user the player
     * @param group the group, or null to clear it
     */
    record SetPrimaryGroup(UUID user, String group) implements Change {
        public SetPrimaryGroup {
            Objects.requireNonNull(user, "user");
            if (group != null) {
                group = Names.group(group);
            }
        }
    }

    /**
     * Create or replace a track.
     *
     * @param name the track
     * @param groups its distinct groups, in order
     */
    record SetTrack(String name, List<String> groups) implements Change {
        public SetTrack {
            name = Names.track(name);
            List<String> normalized = new ArrayList<>(groups.size());
            for (String group : groups) {
                normalized.add(Names.group(group));
            }
            if (new HashSet<>(normalized).size() != normalized.size()) {
                throw Names.invalid("track", name, "lists a group twice");
            }
            groups = List.copyOf(normalized);
        }
    }

    /**
     * Delete a track.
     *
     * @param name the track
     */
    record DeleteTrack(String name) implements Change {
        public DeleteTrack {
            name = Names.track(name);
        }
    }

    /**
     * Move a user to the next group on a track, in exactly {@code contexts}.
     *
     * @param user the player
     * @param track the track
     * @param contexts the contexts of the moved inherit node
     */
    record Promote(UUID user, String track, ContextSet contexts) implements Change {
        public Promote {
            Objects.requireNonNull(user, "user");
            Objects.requireNonNull(contexts, "contexts");
            track = Names.track(track);
        }
    }

    /**
     * Move a user to the previous group on a track, in exactly {@code contexts}.
     *
     * @param user the player
     * @param track the track
     * @param contexts the contexts of the moved inherit node
     */
    record Demote(UUID user, String track, ContextSet contexts) implements Change {
        public Demote {
            Objects.requireNonNull(user, "user");
            Objects.requireNonNull(contexts, "contexts");
            track = Names.track(track);
        }
    }

    /**
     * Patch every node a filter selects.
     *
     * @param filter which nodes
     * @param patch what to do; must set something
     */
    record Bulk(Filter filter, Patch patch) implements Change {
        public Bulk {
            Objects.requireNonNull(filter, "filter");
            if (patch.isEmpty()) {
                throw Names.invalid("bulk patch", "{}", "sets nothing");
            }
        }
    }

    /**
     * Several changes in one transaction and one revision.
     *
     * @param changes at least one change, none of them a batch
     */
    record Batch(List<Change> changes) implements Change {
        public Batch {
            changes = List.copyOf(changes);
            if (changes.isEmpty()) {
                throw Names.invalid("batch", "[]", "is empty");
            }
            if (changes.stream().anyMatch(Batch.class::isInstance)) {
                throw Names.invalid("batch", "batch", "cannot be nested");
            }
        }
    }
}
