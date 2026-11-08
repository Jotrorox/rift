package io.github.jotrorox.rift.permissions;

import java.util.List;
import java.util.UUID;

/**
 * The proxy's answer to a committed {@link Change}.
 *
 * @param revision the new revision, or the current one when nothing changed
 * @param ops how many effects were recorded; 0 means nothing changed
 * @param changedUsers players whose compiled subjects changed
 * @param changedGroups groups whose own data changed
 */
public record Applied(long revision, int ops, List<UUID> changedUsers, List<String> changedGroups) {
    public Applied {
        changedUsers = List.copyOf(changedUsers);
        changedGroups = List.copyOf(changedGroups);
    }
}
