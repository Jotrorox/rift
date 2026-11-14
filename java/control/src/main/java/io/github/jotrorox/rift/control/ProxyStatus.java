package io.github.jotrorox.rift.control;

import io.github.jotrorox.rift.permissions.client.Json;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.Optional;

/**
 * A snapshot of the proxy returned by {@code rift.control.status}.
 *
 * @param maintenance whether new logins are refused for maintenance
 * @param connections open client connections, including those still logging in
 * @param playersOnline players that completed login
 * @param players online players with their current backend
 * @param backends configured backends with health and draining state
 * @param managedServers the servers Rift supervises, as the proxy reports them
 * @param raw the complete status object, including fields added by newer Rift versions
 */
public record ProxyStatus(
        boolean maintenance,
        long connections,
        long playersOnline,
        List<Player> players,
        List<Backend> backends,
        Json.Arr managedServers,
        Json.Obj raw) {
    public ProxyStatus {
        players = List.copyOf(players);
        backends = List.copyOf(backends);
        Objects.requireNonNull(managedServers, "managedServers");
        Objects.requireNonNull(raw, "raw");
    }

    /**
     * An online player.
     *
     * @param connectionId the connection identifier {@link RiftControl#transfer(long, String)} takes
     * @param name the player name
     * @param backend the backend the player is connected to
     * @param protocol the client's Minecraft protocol version number
     */
    public record Player(long connectionId, String name, String backend, int protocol) {}

    /**
     * A configured backend.
     *
     * @param name the backend name
     * @param up whether the latest health check succeeded
     * @param draining whether new players are kept away from the backend
     * @param serviceState the label the server reported about itself, such as {@code INGAME}
     * @param properties the properties the server reported about itself
     */
    public record Backend(
            String name, boolean up, boolean draining, Optional<String> serviceState, Map<String, String> properties) {
        public Backend {
            Objects.requireNonNull(serviceState, "serviceState");
            properties = Collections.unmodifiableMap(new LinkedHashMap<>(properties));
        }
    }

    /**
     * @param name a player name; ASCII case is ignored
     * @return the online player of that name
     */
    public Optional<Player> player(String name) {
        return players.stream().filter(player -> player.name().equalsIgnoreCase(name)).findFirst();
    }

    /**
     * @param name a backend name
     * @return the backend of that name
     */
    public Optional<Backend> backend(String name) {
        return backends.stream().filter(backend -> backend.name().equals(name)).findFirst();
    }

    /**
     * @param backend a backend name
     * @return the players connected to it
     */
    public List<Player> playersOn(String backend) {
        return players.stream().filter(player -> player.backend().equals(backend)).toList();
    }

    static ProxyStatus parse(Json.Obj data) {
        return new ProxyStatus(
                Wire.bool(data, "maintenance"),
                Wire.number(data, "connections"),
                Wire.number(data, "players_online"),
                Wire.objects(data, "players", player -> new Player(
                        Wire.id(player, "connection_id"),
                        Wire.string(player, "name"),
                        Wire.string(player, "backend"),
                        (int) Wire.number(player, "protocol"))),
                Wire.objects(data, "backends", backend -> new Backend(
                        Wire.string(backend, "backend"),
                        Wire.bool(backend, "up"),
                        Wire.bool(backend, "draining"),
                        Wire.optionalString(backend, "service_state"),
                        Wire.properties(backend, "properties"))),
                Wire.array(data, "managed_servers"),
                data);
    }
}
