import com.destroystokyo.paper.profile.ProfileProperty;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.StandardOpenOption;
import org.bukkit.command.Command;
import org.bukkit.command.CommandSender;
import org.bukkit.command.ConsoleCommandSender;
import org.bukkit.entity.Player;
import org.bukkit.event.EventHandler;
import org.bukkit.event.EventPriority;
import org.bukkit.event.Listener;
import org.bukkit.event.player.PlayerJoinEvent;
import org.bukkit.inventory.ItemStack;
import org.bukkit.plugin.java.JavaPlugin;

/** Test fixture only: observes what Paper actually accepted from the proxy. */
public final class RiftOnlineProbe extends JavaPlugin implements Listener {
    @Override
    public void onEnable() {
        getServer().getPluginManager().registerEvents(this, this);
    }

    @EventHandler(priority = EventPriority.MONITOR)
    public void onJoin(PlayerJoinEvent event) {
        snapshot(event.getPlayer(), "join");
    }

    @Override
    public boolean onCommand(CommandSender sender, Command command, String label, String[] args) {
        if (!(sender instanceof ConsoleCommandSender) || args.length != 1) return false;
        Player player = getServer().getPlayerExact(args[0]);
        if (player == null) return false;
        snapshot(player, "snapshot");
        return true;
    }

    private void snapshot(Player player, String event) {
        StringBuilder record = new StringBuilder("{\"event\":" + quote(event)
                + ",\"name\":" + quote(player.getName())
                + ",\"uuid\":" + quote(player.getUniqueId().toString())
                + ",\"ip\":" + quote(player.getAddress().getAddress().getHostAddress())
                + ",\"properties\":[");
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
        record.append("]}");
        try {
            Files.createDirectories(getDataFolder().toPath());
            Files.writeString(getDataFolder().toPath().resolve("profiles.jsonl"),
                    record.toString() + "\n", StandardOpenOption.CREATE, StandardOpenOption.APPEND);
        } catch (IOException failure) {
            throw new RuntimeException("Could not record authenticated acceptance evidence", failure);
        }
    }

    private static String quote(String value) {
        StringBuilder quoted = new StringBuilder("\"");
        for (char ch : value.toCharArray()) {
            if (ch == '"' || ch == '\\') quoted.append('\\').append(ch);
            else if (ch < 32) quoted.append(String.format("\\u%04x", (int) ch));
            else quoted.append(ch);
        }
        return quoted.append('"').toString();
    }

}
