package io.github.jotrorox.rift.network;

import java.util.Map;
import java.util.Objects;
import java.util.Optional;
import java.util.OptionalInt;

/**
 * A server that is up.
 *
 * @param name the server's name on the proxy
 * @param group the service group it is an instance of, if any
 * @param state whether it accepts players
 * @param serviceState the label the server reported about itself on {@code rift.server.state},
 *     such as {@code LOBBY} or {@code INGAME}, if any
 * @param properties what else it reported, such as its map; empty when nothing
 * @param players the players on it, without those a visibility rule hides
 * @param maxPlayers the player limit Rift knows for it (its queue capacity, else its group's
 *     capacity per instance); empty when Rift knows none, in which case the server's own {@code
 *     max-players} still applies
 */
public record ServerInfo(
        String name,
        Optional<String> group,
        ServerState state,
        Optional<String> serviceState,
        Map<String, String> properties,
        int players,
        OptionalInt maxPlayers) {
    public ServerInfo {
        Objects.requireNonNull(name, "name");
        Objects.requireNonNull(group, "group");
        Objects.requireNonNull(state, "state");
        Objects.requireNonNull(serviceState, "serviceState");
        properties = Map.copyOf(properties);
        Objects.requireNonNull(maxPlayers, "maxPlayers");
    }

    /** @return whether the server takes new players: it is {@link ServerState#RUNNING} */
    public boolean acceptsPlayers() {
        return state == ServerState.RUNNING;
    }

    /** @return whether a known player limit is reached */
    public boolean isFull() {
        return maxPlayers.isPresent() && players >= maxPlayers.getAsInt();
    }
}
