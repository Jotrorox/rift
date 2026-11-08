package io.github.jotrorox.rift.permissions;

import java.util.List;
import java.util.Objects;

/**
 * A group as {@code rift.perms.groups.get} lists it.
 *
 * @param name the group
 * @param weight its weight
 * @param displayName its display name, or null
 * @param nodes its inherit, prefix, suffix and meta nodes (and permission nodes when requested)
 */
public record GroupInfo(String name, int weight, String displayName, List<Node> nodes) {
    public GroupInfo {
        Objects.requireNonNull(name, "name");
        nodes = List.copyOf(nodes);
    }
}
