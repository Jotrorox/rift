package io.github.jotrorox.rift.permissions;

import java.util.UUID;

/**
 * A player's compiled subject was replaced on this server, so cached decisions about them are
 * stale. Fired after the new snapshot is readable.
 *
 * @param uuid the player
 * @param revision the revision the new snapshot was built from
 */
public record SubjectUpdatedEvent(UUID uuid, long revision) {}
