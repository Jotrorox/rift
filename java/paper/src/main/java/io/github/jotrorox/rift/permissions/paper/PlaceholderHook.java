package io.github.jotrorox.rift.permissions.paper;

import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.MetaView;
import io.github.jotrorox.rift.permissions.client.PermissionsClient;
import java.util.Locale;
import java.util.UUID;
import me.clip.placeholderapi.expansion.PlaceholderExpansion;
import org.bukkit.OfflinePlayer;
import org.bukkit.entity.Player;
import org.bukkit.plugin.Plugin;

/**
 * The {@code %rift_...%} PlaceholderAPI expansion: {@code prefix}, {@code suffix},
 * {@code primary_group}, {@code meta_<key>}, {@code has_permission_<node>} and
 * {@code in_group_<name>}. Online players use their live contexts; offline players held by the
 * replica use {@code server} only. Kept apart so PlaceholderAPI's classes load only when installed.
 */
final class PlaceholderHook extends PlaceholderExpansion {
    private final Plugin plugin;
    private final PermissionsClient client;
    private final Lookups lookups;

    private PlaceholderHook(Plugin plugin, PermissionsClient client, Lookups lookups) {
        this.plugin = plugin;
        this.client = client;
        this.lookups = lookups;
    }

    static void register(Plugin plugin, PermissionsClient client, Lookups lookups) {
        new PlaceholderHook(plugin, client, lookups).register();
    }

    @Override
    public String getIdentifier() {
        return "rift";
    }

    @Override
    public String getAuthor() {
        return "Rift";
    }

    @Override
    public String getVersion() {
        return plugin.getPluginMeta().getVersion();
    }

    @Override
    public boolean persist() {
        return true;
    }

    @Override
    public String onRequest(OfflinePlayer player, String params) {
        if (player == null) {
            return null;
        }
        UUID uuid = player.getUniqueId();
        Player online = player.getPlayer();
        ContextSet contexts = online != null ? client.contexts().of(online) : lookups.contexts((String) null);
        String request = params.toLowerCase(Locale.ROOT);
        if (request.startsWith("has_permission_")) {
            return Boolean.toString(online != null
                    ? online.hasPermission(request.substring("has_permission_".length()))
                    : client.check(uuid, request.substring("has_permission_".length()), contexts).asBoolean(false));
        }
        if (request.startsWith("in_group_")) {
            return Boolean.toString(client.inGroup(uuid, request.substring("in_group_".length()), contexts));
        }
        MetaView meta = client.meta(uuid, contexts);
        if (request.startsWith("meta_")) {
            return meta.value(request.substring("meta_".length())).orElse("");
        }
        return switch (request) {
            case "prefix" -> meta.prefix() == null ? "" : meta.prefix();
            case "suffix" -> meta.suffix() == null ? "" : meta.suffix();
            case "primary_group" -> meta.primaryGroup();
            default -> null;
        };
    }
}
