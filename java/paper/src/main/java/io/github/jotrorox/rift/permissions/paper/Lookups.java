package io.github.jotrorox.rift.permissions.paper;

import io.github.jotrorox.rift.permissions.Applied;
import io.github.jotrorox.rift.permissions.Change;
import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.GroupInfo;
import io.github.jotrorox.rift.permissions.client.PermissionsClient;
import java.time.Duration;
import java.util.Optional;
import java.util.UUID;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.logging.Level;
import java.util.logging.Logger;
import org.bukkit.Bukkit;
import org.bukkit.OfflinePlayer;
import org.bukkit.entity.Player;

/**
 * Blocking reads and writes for the synchronous integrations (Vault, PlaceholderAPI): contexts for
 * a player and an optional world, snapshots of offline players fetched on demand, and writes that
 * wait for the proxy with the configured timeout.
 */
final class Lookups {
    private final PermissionsClient client;
    private final PaperPlatform platform;
    private final String server;
    private final Duration timeout;
    private final Logger logger;

    Lookups(PermissionsClient client, PaperPlatform platform, String server, Duration timeout, Logger logger) {
        this.client = client;
        this.platform = platform;
        this.server = server;
        this.timeout = timeout;
        this.logger = logger;
    }

    PermissionsClient client() {
        return client;
    }

    /** The player's live contexts when online in {@code world} (or any world for null). */
    ContextSet contexts(OfflinePlayer player, String world) {
        Player online = player.getPlayer();
        if (online != null && (world == null || online.getWorld().getName().equalsIgnoreCase(world))) {
            return platform.contexts(online);
        }
        return contexts(world);
    }

    /** {@code server}, plus {@code world} when given: the contexts of a write or offline read. */
    ContextSet contexts(String world) {
        ContextSet.Builder contexts = ContextSet.builder().add(ContextSet.SERVER, server);
        if (world != null) {
            contexts.add(ContextSet.WORLD, world);
        }
        return contexts.build();
    }

    /** Make sure the replica holds the player, fetching an offline player's snapshot. */
    boolean load(UUID uuid) {
        return client.compiled(uuid).isPresent() || await(client.load(uuid), "load " + uuid) != null;
    }

    Optional<GroupInfo> group(String name) {
        Optional<GroupInfo> group = await(client.fetchGroup(name), "fetch group " + name);
        return group == null ? Optional.empty() : group;
    }

    /** Apply and wait; false when the proxy refused, failed or timed out. */
    boolean apply(Change change) {
        Applied applied = await(client.apply(change), "apply " + change);
        return applied != null;
    }

    @SuppressWarnings("deprecation")
    static OfflinePlayer player(String name) {
        Player online = Bukkit.getPlayerExact(name);
        return online != null ? online : Bukkit.getOfflinePlayer(name);
    }

    private <T> T await(Future<T> future, String what) {
        try {
            return future.get(timeout.toMillis() + 1000, TimeUnit.MILLISECONDS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            return null;
        } catch (ExecutionException | TimeoutException e) {
            Throwable cause = e instanceof ExecutionException && e.getCause() != null ? e.getCause() : e;
            logger.log(Level.WARNING, "Rift permissions: cannot " + what + ": " + cause.getMessage());
            return null;
        }
    }
}
