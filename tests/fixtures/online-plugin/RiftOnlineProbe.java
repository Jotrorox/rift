import com.destroystokyo.paper.profile.ProfileProperty;
import io.papermc.paper.command.brigadier.Commands;
import io.papermc.paper.command.brigadier.argument.ArgumentTypes;
import io.papermc.paper.command.brigadier.argument.SignedMessageResolver;
import io.papermc.paper.event.player.AsyncChatEvent;
import io.papermc.paper.plugin.lifecycle.event.types.LifecycleEvents;
import java.io.IOException;
import java.net.URI;
import java.nio.file.Files;
import java.nio.file.StandardOpenOption;
import java.time.Instant;
import java.util.HexFormat;
import java.util.UUID;
import net.kyori.adventure.chat.SignedMessage;
import net.kyori.adventure.text.serializer.plain.PlainTextComponentSerializer;
import org.bukkit.command.Command;
import org.bukkit.command.CommandSender;
import org.bukkit.command.ConsoleCommandSender;
import org.bukkit.entity.Player;
import org.bukkit.event.EventHandler;
import org.bukkit.event.EventPriority;
import org.bukkit.event.Listener;
import org.bukkit.event.player.PlayerCommandPreprocessEvent;
import org.bukkit.event.player.PlayerJoinEvent;
import org.bukkit.event.player.PlayerResourcePackStatusEvent;
import org.bukkit.inventory.ItemStack;
import org.bukkit.plugin.Plugin;
import org.bukkit.plugin.java.JavaPlugin;

/** Test fixture only: observes what Paper actually accepted from the proxy. */
public final class RiftOnlineProbe extends JavaPlugin implements Listener {
    @Override
    public void onEnable() {
        getServer().getPluginManager().registerEvents(this, this);
        // A real signed-message argument lets the vanilla client sign commands
        // too. A preprocess event alone cannot establish that a command was signed.
        getLifecycleManager().registerEventHandler(LifecycleEvents.COMMANDS, event ->
                event.registrar().register(Commands.literal("riftsigned")
                        .requires(source -> source.getSender() instanceof Player)
                        .then(Commands.argument("message", ArgumentTypes.signedMessage())
                                .executes(context -> {
                                    Player player = (Player) context.getSource().getSender();
                                    String name = player.getName();
                                    UUID uuid = player.getUniqueId();
                                    SignedMessageResolver resolver = context.getArgument(
                                            "message", SignedMessageResolver.class);
                                    resolver.resolveSignedMessage("message", context).thenAccept(message -> {
                                        StringBuilder record = playerRecord("signed_command", name, uuid)
                                                .append(",\"message\":").append(quote(resolver.content()));
                                        signedFields(record, message);
                                        write(record.append('}'));
                                        getServer().getScheduler().runTask(this, () -> {
                                            if (player.isOnline()) player.sendPlainMessage(
                                                    "Rift command recorded: " + resolver.content());
                                        });
                                    }).exceptionally(failure -> {
                                        write(playerRecord("signed_command_error", name, uuid)
                                                .append(",\"error\":").append(quote(failure.toString())).append('}'));
                                        return null;
                                    });
                                    return 1;
                                })).build(), "Record a signed command for the acceptance fixture"));
    }

    @EventHandler(priority = EventPriority.MONITOR)
    public void onJoin(PlayerJoinEvent event) {
        snapshot(event.getPlayer(), "join");
    }

    @EventHandler(priority = EventPriority.MONITOR)
    public void onChat(AsyncChatEvent event) {
        PlainTextComponentSerializer plain = PlainTextComponentSerializer.plainText();
        StringBuilder record = playerRecord("chat", event.getPlayer())
                .append(",\"message\":").append(quote(plain.serialize(event.originalMessage())))
                .append(",\"current_message\":").append(quote(plain.serialize(event.message())))
                .append(",\"cancelled\":").append(event.isCancelled())
                .append(",\"asynchronous\":").append(event.isAsynchronous());
        signedFields(record, event.signedMessage());
        write(record.append('}'));
    }

    @EventHandler(priority = EventPriority.MONITOR)
    public void onCommand(PlayerCommandPreprocessEvent event) {
        // Observational only: this event does not expose command signatures.
        write(playerRecord("command", event.getPlayer())
                .append(",\"message\":").append(quote(event.getMessage()))
                .append(",\"cancelled\":").append(event.isCancelled()).append('}'));
    }

    @EventHandler(priority = EventPriority.MONITOR)
    public void onResourcePack(PlayerResourcePackStatusEvent event) {
        write(playerRecord("resource_pack_status", event.getPlayer())
                .append(",\"pack_id\":").append(quote(event.getID().toString()))
                .append(",\"status\":").append(quote(event.getStatus().name())).append('}'));
    }

    @Override
    public boolean onCommand(CommandSender sender, Command command, String label, String[] args) {
        if (!(sender instanceof ConsoleCommandSender) || args.length == 0) return false;
        if (args.length == 1 && args[0].equals("plugins")) {
            StringBuilder record = record("plugins").append(",\"plugins\":[");
            boolean first = true;
            for (Plugin plugin : getServer().getPluginManager().getPlugins()) {
                if (!first) record.append(',');
                first = false;
                record.append("{\"name\":").append(quote(plugin.getName()))
                        .append(",\"version\":").append(quote(plugin.getPluginMeta().getVersion()))
                        .append(",\"enabled\":").append(plugin.isEnabled()).append('}');
            }
            write(record.append("]}"));
            return true;
        }
        Player player = getServer().getPlayerExact(args.length == 1 ? args[0] : args[1]);
        if (player == null) return false;
        if (args.length == 1 || (args.length == 2 && args[0].equals("snapshot"))) {
            snapshot(player, "snapshot");
            return true;
        }
        if (args.length == 3 && args[0].equals("permission")) {
            write(playerRecord("permission", player)
                    .append(",\"permission\":").append(quote(args[2]))
                    .append(",\"allowed\":").append(player.hasPermission(args[2]))
                    .append(",\"is_set\":").append(player.isPermissionSet(args[2])).append('}'));
            return true;
        }
        try {
            if (args.length == 6 && args[0].equals("pack")) {
                UUID id = UUID.fromString(args[2]);
                URI uri = URI.create(args[3]);
                if (!("http".equals(uri.getScheme()) || "https".equals(uri.getScheme()))
                        || uri.getHost() == null || !args[4].matches("[0-9a-fA-F]{40}")
                        || !(args[5].equals("true") || args[5].equals("false"))) return false;
                boolean required = Boolean.parseBoolean(args[5]);
                player.addResourcePack(id, args[3], HexFormat.of().parseHex(args[4]),
                        "Rift authenticated compatibility pack", required);
                write(playerRecord("resource_pack_request", player)
                        .append(",\"pack_id\":").append(quote(id.toString()))
                        .append(",\"url\":").append(quote(args[3]))
                        .append(",\"sha1\":").append(quote(args[4]))
                        .append(",\"required\":").append(required).append('}'));
                return true;
            }
            if (args.length == 3 && args[0].equals("remove-pack")) {
                UUID id = UUID.fromString(args[2]);
                player.removeResourcePack(id);
                write(playerRecord("resource_pack_remove", player)
                        .append(",\"pack_id\":").append(quote(id.toString())).append('}'));
                return true;
            }
        } catch (IllegalArgumentException invalid) {
            sender.sendMessage("Invalid probe arguments: " + invalid.getMessage());
        }
        return false;
    }

    private void snapshot(Player player, String event) {
        StringBuilder record = playerRecord(event, player)
                .append(",\"ip\":").append(quote(player.getAddress().getAddress().getHostAddress()))
                .append(",\"properties\":[");
        boolean first = true;
        for (ProfileProperty property : player.getPlayerProfile().getProperties()) {
            if (!first) record.append(',');
            first = false;
            record.append("{\"name\":").append(quote(property.getName()))
                    .append(",\"value\":").append(quote(property.getValue()));
            if (property.getSignature() != null) {
                record.append(",\"signature\":").append(quote(property.getSignature()));
            }
            record.append('}');
        }
        record.append("],\"inventory\":[");
        first = true;
        ItemStack[] items = player.getInventory().getContents();
        for (int slot = 0; slot < items.length; slot++) {
            ItemStack item = items[slot];
            if (item == null || item.getType().isAir()) continue;
            if (!first) record.append(',');
            first = false;
            record.append("{\"slot\":").append(slot)
                    .append(",\"item\":").append(quote(item.getType().getKey().toString()))
                    .append(",\"count\":").append(item.getAmount()).append('}');
        }
        write(record.append("]}"));
    }

    private static StringBuilder record(String event) {
        return new StringBuilder("{\"event\":").append(quote(event))
                .append(",\"utc\":").append(quote(Instant.now().toString()));
    }

    private static StringBuilder playerRecord(String event, Player player) {
        return playerRecord(event, player.getName(), player.getUniqueId());
    }

    private static StringBuilder playerRecord(String event, String name, UUID uuid) {
        return record(event).append(",\"name\":").append(quote(name))
                .append(",\"uuid\":").append(quote(uuid.toString()));
    }

    private static void signedFields(StringBuilder record, SignedMessage message) {
        // Paper has already processed the signed chat/session before this API
        // observer runs. These fields expose its result, not a second verifier.
        SignedMessage.Signature signature = message == null ? null : message.signature();
        int length = signature == null ? 0 : signature.bytes().length;
        record.append(",\"signed\":").append(message != null && !message.isSystem() && length > 0)
                .append(",\"signature_bytes\":").append(length)
                .append(",\"signed_message\":").append(quote(message == null ? null : message.message()))
                .append(",\"signed_identity\":").append(quote(message == null ? null : message.identity().uuid().toString()))
                .append(",\"signed_timestamp\":").append(quote(message == null ? null : message.timestamp().toString()));
    }

    // Chat and signed-command completions can arrive on different threads.
    // Serialise full JSONL appends so readers never observe interleaved records.
    private synchronized void write(StringBuilder record) {
        try {
            Files.createDirectories(getDataFolder().toPath());
            Files.writeString(getDataFolder().toPath().resolve("profiles.jsonl"),
                    record.toString() + "\n", StandardOpenOption.CREATE, StandardOpenOption.APPEND);
        } catch (IOException failure) {
            throw new RuntimeException("Could not record authenticated acceptance evidence", failure);
        }
    }

    private static String quote(String value) {
        if (value == null) return "null";
        StringBuilder quoted = new StringBuilder("\"");
        for (char ch : value.toCharArray()) {
            if (ch == '"' || ch == '\\') quoted.append('\\').append(ch);
            else if (ch < 32) quoted.append(String.format("\\u%04x", (int) ch));
            else quoted.append(ch);
        }
        return quoted.append('"').toString();
    }

}
