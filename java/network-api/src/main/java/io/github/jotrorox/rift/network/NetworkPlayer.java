package io.github.jotrorox.rift.network;

import java.util.Objects;
import java.util.Optional;
import java.util.UUID;

/**
 * An online player.
 *
 * @param uuid the player's UUID
 * @param name the player's name
 * @param server the server they are on
 * @param visibleTo the permission node a viewer needs to see this player, when a visibility rule
 *     hides them; leave such players out of what other players are shown unless the viewer holds
 *     the node
 */
public record NetworkPlayer(UUID uuid, String name, String server, Optional<String> visibleTo) {
    public NetworkPlayer {
        Objects.requireNonNull(uuid, "uuid");
        Objects.requireNonNull(name, "name");
        Objects.requireNonNull(server, "server");
        Objects.requireNonNull(visibleTo, "visibleTo");
    }

    /** @return whether a visibility rule hides this player */
    public boolean hidden() {
        return visibleTo.isPresent();
    }
}
