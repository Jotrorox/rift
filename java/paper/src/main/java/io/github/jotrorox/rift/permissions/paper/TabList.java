package io.github.jotrorox.rift.permissions.paper;

import io.github.jotrorox.rift.permissions.MetaView;
import io.github.jotrorox.rift.permissions.RiftPermissions;
import java.util.Map;
import java.util.logging.Level;
import java.util.logging.Logger;
import net.kyori.adventure.text.Component;
import org.bukkit.entity.Player;
import org.bukkit.event.EventHandler;
import org.bukkit.event.EventPriority;
import org.bukkit.event.Listener;
import org.bukkit.event.player.PlayerJoinEvent;
import org.bukkit.plugin.Plugin;

/**
 * The opt-in tab list ({@code tab} in config.yml): each player's list name carries their prefix and
 * suffix, and the list is sorted by primary group weight, heaviest first, then by name.
 *
 * <p>Sorting uses the player list order the 1.21.2 protocol added, which the client sorts by before
 * the name; it needs no scoreboard teams, so it leaves name tags, collision and other plugins' teams
 * alone and works on Folia. A 1.21 or 1.21.1 server has no such order: names are still decorated
 * there and the list keeps the client's alphabetical order.
 */
final class TabList implements Listener {
    private final Plugin plugin;
    private final RiftPermissions permissions;
    private final PaperConfig.Tab config;
    private final boolean sorted;

    TabList(Plugin plugin, RiftPermissions permissions, PaperConfig.Tab config) {
        this.plugin = plugin;
        this.permissions = permissions;
        this.config = config;
        this.sorted = config.sort() && hasListOrder(plugin.getLogger());
    }

    private static boolean hasListOrder(Logger logger) {
        try {
            Player.class.getMethod("setPlayerListOrder", int.class);
            return true;
        } catch (NoSuchMethodException e) {
            logger.warning("tab.sort needs Paper 1.21.2 or newer; the tab list keeps the client's order");
            return false;
        }
    }

    /** The join event runs on the player's own thread, so the first list entry others see is right. */
    @EventHandler(priority = EventPriority.MONITOR)
    public void onJoin(PlayerJoinEvent event) {
        apply(event.getPlayer());
    }

    /** Redo one player's entry on their own thread (Folia-safe), after their snapshot or contexts changed. */
    void refresh(Player player) {
        player.getScheduler().run(plugin, task -> apply(player), null);
    }

    /** Redo every entry, after the group table and with it the weights changed. */
    void refreshAll() {
        plugin.getServer().getOnlinePlayers().forEach(this::refresh);
    }

    /** Must run on the player's thread. */
    void apply(Player player) {
        MetaView meta = permissions.meta(player);
        if (!config.format().isEmpty()) {
            player.playerListName(Decorations.render(config.format(), meta.prefix(), meta.suffix(), Map.of(
                    "name", Component.text(player.getName()),
                    "display_name", player.displayName(),
                    "group", Component.text(meta.primaryGroup()))));
        }
        if (sorted) {
            player.setPlayerListOrder(Decorations.listOrder(meta.primaryGroup(), permissions.groups()));
        }
    }

    /** Give every player their plain entry back, when the plugin disables. */
    void restoreAll(Iterable<? extends Player> players) {
        for (Player player : players) {
            try {
                if (!config.format().isEmpty()) {
                    player.playerListName(null);
                }
                if (sorted) {
                    player.setPlayerListOrder(0);
                }
            } catch (RuntimeException e) {
                plugin.getLogger().log(Level.WARNING, "Cannot restore the tab list entry of " + player.getName(), e);
            }
        }
    }
}
