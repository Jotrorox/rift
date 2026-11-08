package io.github.jotrorox.rift.permissions.client;

import java.util.List;
import java.util.UUID;

/** What an applied change event asks the caller to do. */
public sealed interface UpdateOutcome {
    /** Not newer than the replica's revision: a duplicate or reordered event. Nothing to do. */
    record Stale() implements UpdateOutcome {}

    /**
     * The next revision.
     *
     * @param refetch stored players to refetch unless their pushed snapshot arrives first
     * @param groups whether to refetch the group table
     */
    record Applied(List<UUID> refetch, boolean groups) implements UpdateOutcome {
        public Applied {
            refetch = List.copyOf(refetch);
        }
    }

    /**
     * A revision was skipped, so an event was lost: refetch every stored player and the groups.
     *
     * @param refetch every stored player
     */
    record Gap(List<UUID> refetch) implements UpdateOutcome {
        public Gap {
            refetch = List.copyOf(refetch);
        }
    }
}
