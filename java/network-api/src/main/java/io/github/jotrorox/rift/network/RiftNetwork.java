package io.github.jotrorox.rift.network;

import java.util.List;
import java.util.Optional;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;

/**
 * The Rift network as a backend sees it: which servers are up, who is online where, moving a player
 * to a server or to any instance of a service group, and messages to proxy plugins and other
 * servers.
 *
 * <pre>{@code
 * RiftNetwork network = RiftNetwork.get();
 * network.servers("lobby").thenAccept(lobbies -> lobbies.forEach(lobby ->
 *         menu.add(lobby.name(), lobby.players(), lobby.acceptsPlayers())));
 * network.connectToGroup(player.getUniqueId(), "bedwars");
 * }</pre>
 *
 * <p>Every call asks the proxy and returns a future that completes on a Rift thread, never on a
 * server thread: schedule back before touching game state. Futures fail with {@link
 * NetworkException}, and with {@link NetworkTimeoutException} when the proxy does not answer in
 * time, so nothing hangs.
 */
public interface RiftNetwork {
    /**
     * @return the installed service
     * @throws IllegalStateException when no implementation is installed
     */
    static RiftNetwork get() {
        return RiftNetworkProvider.get();
    }

    /** @return this server's name on the proxy */
    String localServer();

    /** @return the service group this server is an instance of, if any */
    Optional<String> localGroup();

    /**
     * @return every server that is up, sorted by name; servers that are stopped, starting or failing
     *     their health check are left out
     */
    CompletableFuture<List<ServerInfo>> servers();

    /**
     * @param serverOrGroup a server name, or a service group for its instances; ASCII case is
     *     ignored
     * @return the servers among them that are up, sorted by name; empty when none is. Fails with
     *     {@link NetworkException.Reason#REFUSED} when the proxy knows no such server or group
     */
    CompletableFuture<List<ServerInfo>> servers(String serverOrGroup);

    /** @return every online player, players hidden by a visibility rule included */
    CompletableFuture<List<NetworkPlayer>> players();

    /**
     * @param serverOrGroup a server name, or a service group for the players on its instances
     * @return the players there
     */
    CompletableFuture<List<NetworkPlayer>> players(String serverOrGroup);

    /**
     * @param uuid a player
     * @return the player, or empty when they are offline
     */
    CompletableFuture<Optional<NetworkPlayer>> player(UUID uuid);

    /**
     * @param name a player name; ASCII case is ignored
     * @return the player, or empty when they are offline
     */
    CompletableFuture<Optional<NetworkPlayer>> player(String name);

    /**
     * Ask the proxy to move a player to a server. The proxy applies its access rules, capacity and
     * transfer hooks, as for {@code /server}.
     *
     * <p>A server may move the players that are on it. Moving anyone else needs a bus login with
     * {@code players = true} and fails with {@link NetworkException.Reason#REFUSED} otherwise.
     *
     * @param player the player
     * @param server a server name; a service group's name is accepted too
     * @return true when the transfer was queued, false when the player is offline. A queued
     *     transfer can still be refused (a full or unreachable server, an access rule); the player
     *     then stays where they are, which {@link #player(UUID)} shows
     */
    CompletableFuture<Boolean> connect(UUID player, String server);

    /**
     * Ask the proxy to move a player to any instance of a service group. The proxy picks the
     * instance as it does for a route to the group, and starts or queues by the group's rules.
     *
     * @param player the player
     * @param group a service group; a plain server's name is refused
     * @return as {@link #connect(UUID, String)}
     */
    CompletableFuture<Boolean> connectToGroup(UUID player, String group);

    /** @return publish, request and subscribe on the proxy's message bus */
    Messenger messages();
}
