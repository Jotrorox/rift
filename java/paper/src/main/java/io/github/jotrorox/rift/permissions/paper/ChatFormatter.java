package io.github.jotrorox.rift.permissions.paper;

import io.github.jotrorox.rift.permissions.MetaView;
import io.github.jotrorox.rift.permissions.RiftPermissions;
import io.papermc.paper.chat.ChatRenderer;
import io.papermc.paper.event.player.AsyncChatEvent;
import java.util.Map;
import net.kyori.adventure.text.Component;
import net.kyori.adventure.text.serializer.plain.PlainTextComponentSerializer;
import org.bukkit.entity.Player;
import org.bukkit.event.EventHandler;
import org.bukkit.event.EventPriority;
import org.bukkit.event.Listener;

/**
 * The opt-in chat format ({@code chat} in config.yml): the sender's prefix and suffix in their
 * current contexts, read from the replica like the Vault and PlaceholderAPI bridges read them. It
 * sets the renderer early, so a chat plugin that sets its own later still wins.
 */
final class ChatFormatter implements Listener {
    /** Lets a player use legacy colour codes in what they type. */
    static final String COLOR_PERMISSION = "rift.chat.color";

    private final RiftPermissions permissions;
    private final String format;

    ChatFormatter(RiftPermissions permissions, String format) {
        this.permissions = permissions;
        this.format = format;
    }

    @EventHandler(priority = EventPriority.LOW, ignoreCancelled = true)
    public void onChat(AsyncChatEvent event) {
        Player player = event.getPlayer();
        MetaView meta = permissions.meta(player);
        boolean coloured = player.hasPermission(COLOR_PERMISSION);
        Component name = Component.text(player.getName());
        Component group = Component.text(meta.primaryGroup());
        // The same line for every viewer, built once per message from what later handlers left of it.
        event.renderer(ChatRenderer.viewerUnaware((source, displayName, message) -> Decorations.render(
                format, meta.prefix(), meta.suffix(), Map.of(
                        "name", name,
                        "display_name", displayName,
                        "group", group,
                        "message", coloured
                                ? Decorations.coloured(PlainTextComponentSerializer.plainText().serialize(message))
                                : message))));
    }
}
