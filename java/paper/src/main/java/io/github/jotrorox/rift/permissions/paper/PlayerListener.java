package io.github.jotrorox.rift.permissions.paper;

import io.github.jotrorox.rift.permissions.client.PermissionsClient;
import java.time.Duration;
import java.util.Map;
import java.util.UUID;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.logging.Level;
import java.util.logging.Logger;
import org.bukkit.entity.Player;
import org.bukkit.event.EventHandler;
import org.bukkit.event.EventPriority;
import org.bukkit.event.Listener;
import org.bukkit.event.player.AsyncPlayerPreLoginEvent;
import org.bukkit.event.player.PlayerChangedWorldEvent;
import org.bukkit.event.player.PlayerGameModeChangeEvent;
import org.bukkit.event.player.PlayerJoinEvent;
import org.bukkit.event.player.PlayerLoginEvent;
import org.bukkit.event.player.PlayerQuitEvent;
import org.bukkit.permissions.PermissibleBase;
import org.bukkit.plugin.Plugin;

/**
 * The player lifecycle: wait for the snapshot during pre-login, inject the permissible at login,
 * track who is online, and recompute contexts when the world or game mode changes.
 */
final class PlayerListener implements Listener {
    private final Plugin plugin;
    private final PermissionsClient client;
    private final PaperPlatform platform;
    private final Duration joinTimeout;
    private final Logger logger;
    private final Map<UUID, Injected> injected = new ConcurrentHashMap<>();

    private record Injected(RiftPermissible permissible, PermissibleBase previous) {}

    PlayerListener(Plugin plugin, PermissionsClient client, PaperPlatform platform, Duration joinTimeout) {
        this.plugin = plugin;
        this.client = client;
        this.platform = platform;
        this.joinTimeout = joinTimeout;
        this.logger = plugin.getLogger();
    }

    /** Wait (off the main thread) for the snapshot; on timeout the offline policy answers meanwhile. */
    @EventHandler(priority = EventPriority.LOW)
    public void onPreLogin(AsyncPlayerPreLoginEvent event) {
        if (event.getLoginResult() != AsyncPlayerPreLoginEvent.Result.ALLOWED) {
            return;
        }
        try {
            client.awaitSubject(event.getUniqueId()).get(joinTimeout.toMillis() + 500, TimeUnit.MILLISECONDS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        } catch (ExecutionException | TimeoutException e) {
            logger.warning("No permission snapshot for " + event.getName() + " within " + joinTimeout.toMillis()
                    + " ms; applying the offline policy and retrying every 5 s");
        }
    }

    @EventHandler(priority = EventPriority.LOWEST)
    @SuppressWarnings("deprecation")
    public void onLogin(PlayerLoginEvent event) {
        inject(event.getPlayer());
    }

    void inject(Player player) {
        RiftPermissible permissible = new RiftPermissible(player, client);
        try {
            PermissibleBase previous = PermissibleInjector.inject(player, permissible);
            injected.put(player.getUniqueId(), new Injected(permissible, previous));
        } catch (ReflectiveOperationException | RuntimeException e) {
            logger.log(Level.SEVERE, "Cannot inject Rift permissions for " + player.getName()
                    + "; Bukkit's own permissions answer for them", e);
        }
    }

    /** Give every player their original permissible back, when the plugin disables. */
    void restoreAll(Iterable<? extends Player> players) {
        for (Player player : players) {
            Injected entry = injected.remove(player.getUniqueId());
            if (entry != null && entry.previous() != null) {
                try {
                    PermissibleInjector.restore(player, entry.permissible(), entry.previous());
                } catch (ReflectiveOperationException | RuntimeException e) {
                    logger.log(Level.WARNING, "Cannot restore the permissible of " + player.getName(), e);
                }
            }
        }
    }

    @EventHandler(priority = EventPriority.MONITOR)
    public void onJoin(PlayerJoinEvent event) {
        client.playerJoined(event.getPlayer().getUniqueId());
    }

    @EventHandler(priority = EventPriority.MONITOR)
    public void onQuit(PlayerQuitEvent event) {
        UUID uuid = event.getPlayer().getUniqueId();
        client.playerLeft(uuid);
        platform.invalidate(uuid);
        injected.remove(uuid);
    }

    @EventHandler(priority = EventPriority.MONITOR)
    public void onWorldChange(PlayerChangedWorldEvent event) {
        refresh(event.getPlayer());
    }

    /** The event fires before the change, so recompute on the player's next tick. */
    @EventHandler(priority = EventPriority.MONITOR, ignoreCancelled = true)
    public void onGameModeChange(PlayerGameModeChangeEvent event) {
        Player player = event.getPlayer();
        player.getScheduler().run(plugin, task -> refresh(player), null);
    }

    /** Drop cached contexts and recalculate on the player's own thread (Folia-safe). */
    void refresh(Player player) {
        platform.invalidate(player.getUniqueId());
        player.getScheduler().run(plugin, task -> player.recalculatePermissions(), null);
    }
}
