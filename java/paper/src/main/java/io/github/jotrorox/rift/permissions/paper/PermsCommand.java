package io.github.jotrorox.rift.permissions.paper;

import io.github.jotrorox.rift.permissions.GroupInfo;
import io.github.jotrorox.rift.permissions.PermissionException;
import io.github.jotrorox.rift.permissions.PermissionTimeoutException;
import io.github.jotrorox.rift.permissions.client.PermissionsClient;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.UUID;
import java.util.concurrent.CompletionException;
import java.util.stream.Stream;
import net.kyori.adventure.text.Component;
import net.kyori.adventure.text.format.NamedTextColor;
import net.kyori.adventure.text.serializer.gson.GsonComponentSerializer;
import org.bukkit.Bukkit;
import org.bukkit.command.Command;
import org.bukkit.command.CommandSender;
import org.bukkit.command.ConsoleCommandSender;
import org.bukkit.command.TabExecutor;
import org.bukkit.entity.Player;
import org.bukkit.permissions.Permission;
import org.bukkit.plugin.Plugin;
import org.bukkit.util.StringUtil;

/**
 * {@code /perms} (alias {@code /rp}): parsed, authorised, applied and audited by the proxy. This
 * side only forwards the arguments and prints the Adventure component the proxy answers with. Tab
 * completion comes from the replica's group table, online players and registered permissions.
 */
final class PermsCommand implements TabExecutor {
    private static final List<String> VERBS = List.of(
            "user", "group", "track", "check", "trace", "listgroups", "search", "bulk", "import", "export", "sync");
    private static final Map<String, List<String>> SUBJECT_VERBS = Map.of(
            "user", List.of("info", "permission", "parent", "meta", "promote", "demote", "clear"),
            "group", List.of("info", "create", "delete", "permission", "parent", "meta", "setweight",
                    "setdisplayname", "clear"),
            "track", List.of("info", "create", "delete", "append", "insert", "remove"));
    private static final Map<String, List<String>> ACTIONS = Map.of(
            "permission", List.of("set", "unset", "check"),
            "parent", List.of("add", "remove", "set"),
            "meta", List.of("set", "unset", "setprefix", "setsuffix", "removeprefix", "removesuffix"));

    private final Plugin plugin;
    private final PermissionsClient client;

    PermsCommand(Plugin plugin, PermissionsClient client) {
        this.plugin = plugin;
        this.client = client;
    }

    @Override
    public boolean onCommand(CommandSender sender, Command command, String label, String[] args) {
        UUID actor;
        if (sender instanceof Player player) {
            actor = player.getUniqueId();
        } else if (sender instanceof ConsoleCommandSender) {
            actor = null;
        } else {
            sender.sendMessage(Component.text("/perms runs for players and the console only", NamedTextColor.RED));
            return true;
        }
        client.command(actor, Arrays.asList(args)).whenComplete((reply, error) -> {
            Component message;
            if (error == null) {
                message = GsonComponentSerializer.gson().deserialize(reply.component());
            } else {
                Throwable cause = error instanceof CompletionException && error.getCause() != null ? error.getCause() : error;
                message = Component.text(switch (cause) {
                    case PermissionTimeoutException timeout -> "The Rift proxy did not answer in time; the command may still apply.";
                    case PermissionException refused -> "The Rift proxy refused the command: " + refused.getMessage();
                    default -> "Cannot reach the Rift proxy: " + cause.getMessage();
                }, NamedTextColor.RED);
            }
            if (sender instanceof Player player) {
                player.getScheduler().run(plugin, task -> player.sendMessage(message), null);
            } else {
                sender.sendMessage(message);
            }
        });
        return true;
    }

    @Override
    public List<String> onTabComplete(CommandSender sender, Command command, String label, String[] args) {
        String last = args[args.length - 1];
        List<String> options = switch (args.length) {
            case 1 -> VERBS;
            case 2 -> switch (args[0].toLowerCase(Locale.ROOT)) {
                case "user", "check", "trace" -> players();
                case "group" -> groups();
                default -> List.of();
            };
            case 3 -> switch (args[0].toLowerCase(Locale.ROOT)) {
                case "check", "trace" -> permissions();
                default -> SUBJECT_VERBS.getOrDefault(args[0].toLowerCase(Locale.ROOT), List.of());
            };
            case 4 -> ACTIONS.getOrDefault(args[2].toLowerCase(Locale.ROOT), List.of());
            case 5 -> switch (args[2].toLowerCase(Locale.ROOT)) {
                case "permission" -> permissions();
                case "parent" -> groups();
                default -> List.of();
            };
            case 6 -> args[2].equalsIgnoreCase("permission") && args[3].equalsIgnoreCase("set")
                    ? List.of("true", "false") : List.of();
            default -> List.of();
        };
        return StringUtil.copyPartialMatches(last, options, new ArrayList<>());
    }

    private static List<String> players() {
        return Bukkit.getOnlinePlayers().stream().map(Player::getName).toList();
    }

    private List<String> groups() {
        return client.groups().stream().map(GroupInfo::name).toList();
    }

    private static List<String> permissions() {
        return Stream.concat(Bukkit.getPluginManager().getPermissions().stream().map(Permission::getName),
                        Stream.of("*"))
                .toList();
    }
}
