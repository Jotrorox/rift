package io.github.jotrorox.rift.permissions.paper;

import io.github.jotrorox.rift.permissions.Change;
import io.github.jotrorox.rift.permissions.CompiledSubject;
import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.Filter;
import io.github.jotrorox.rift.permissions.GroupInfo;
import io.github.jotrorox.rift.permissions.Node;
import io.github.jotrorox.rift.permissions.NodeKind;
import io.github.jotrorox.rift.permissions.Patch;
import io.github.jotrorox.rift.permissions.SubjectRef;
import java.util.List;
import java.util.Optional;
import java.util.function.Function;
import net.milkbowl.vault.chat.Chat;
import net.milkbowl.vault.permission.Permission;
import org.bukkit.OfflinePlayer;
import org.bukkit.entity.Player;
import org.bukkit.plugin.Plugin;
import org.bukkit.plugin.ServicePriority;

/**
 * Vault {@code Permission} and {@code Chat} providers. Reads come from the replica (offline players
 * are fetched once); writes become {@link Change}s and block for the proxy's reply with the
 * configured timeout, because Vault is synchronous. A {@code world} argument adds
 * {@code server=<this server>} and {@code world=<world>} contexts to writes.
 *
 * <p>Kept apart from the plugin class so that Vault's classes load only when Vault is installed.
 */
final class VaultHook {
    private VaultHook() {}

    static void register(Plugin plugin, Lookups lookups) {
        VaultPermission permission = new VaultPermission(plugin, lookups);
        plugin.getServer().getServicesManager().register(Permission.class, permission, plugin, ServicePriority.Highest);
        plugin.getServer().getServicesManager()
                .register(Chat.class, new VaultChat(permission, plugin, lookups), plugin, ServicePriority.Highest);
    }

    private static SubjectRef user(OfflinePlayer player) {
        return SubjectRef.user(player.getUniqueId());
    }

    /** Write contexts: global without a world, otherwise this server and that world. */
    private static ContextSet scope(Lookups lookups, String world) {
        return world == null ? ContextSet.empty() : lookups.scope(world);
    }

    @SuppressWarnings("deprecation")
    private static final class VaultPermission extends Permission {
        private final Lookups lookups;

        VaultPermission(Plugin plugin, Lookups lookups) {
            this.plugin = plugin;
            this.lookups = lookups;
        }

        @Override
        public String getName() {
            return plugin.getName();
        }

        @Override
        public boolean isEnabled() {
            return plugin.isEnabled();
        }

        @Override
        public boolean hasSuperPermsCompat() {
            return true;
        }

        @Override
        public boolean hasGroupSupport() {
            return true;
        }

        @Override
        public boolean playerHas(String world, OfflinePlayer player, String permission) {
            Player online = player.getPlayer();
            if (online != null && (world == null || online.getWorld().getName().equalsIgnoreCase(world))) {
                return online.hasPermission(permission);
            }
            return lookups.load(player.getUniqueId()) && lookups.client()
                    .check(player.getUniqueId(), permission, lookups.contexts(player, world)).asBoolean(false);
        }

        @Override
        public boolean playerHas(String world, String player, String permission) {
            return playerHas(world, Lookups.player(player), permission);
        }

        @Override
        public boolean playerAdd(String world, OfflinePlayer player, String permission) {
            return lookups.apply(Change.setNode(user(player),
                    Node.permission(permission, true).contexts(scope(lookups, world))));
        }

        @Override
        public boolean playerAdd(String world, String player, String permission) {
            return playerAdd(world, Lookups.player(player), permission);
        }

        @Override
        public boolean playerRemove(String world, OfflinePlayer player, String permission) {
            return lookups.apply(Change.unsetNode(user(player),
                    Node.permission(permission, true).contexts(scope(lookups, world))));
        }

        @Override
        public boolean playerRemove(String world, String player, String permission) {
            return playerRemove(world, Lookups.player(player), permission);
        }

        /** The group's own permission nodes only: inherited ones are resolved per player. */
        @Override
        public boolean groupHas(String world, String group, String permission) {
            String node = Node.normalizePattern(permission);
            ContextSet active = lookups.contexts(world);
            return lookups.group(group).stream()
                    .flatMap(info -> info.nodes().stream())
                    .filter(n -> n.payload() instanceof Node.Permission p && p.key().equals(node))
                    .filter(n -> n.contexts().isSubsetOf(active))
                    .anyMatch(n -> ((Node.Permission) n.payload()).value());
        }

        @Override
        public boolean groupAdd(String world, String group, String permission) {
            return lookups.apply(Change.setNode(SubjectRef.group(group),
                    Node.permission(permission, true).contexts(scope(lookups, world))));
        }

        @Override
        public boolean groupRemove(String world, String group, String permission) {
            return lookups.apply(Change.unsetNode(SubjectRef.group(group),
                    Node.permission(permission, true).contexts(scope(lookups, world))));
        }

        @Override
        public boolean playerInGroup(String world, OfflinePlayer player, String group) {
            return lookups.load(player.getUniqueId())
                    && lookups.client().inGroup(player.getUniqueId(), group, lookups.contexts(player, world));
        }

        @Override
        public boolean playerInGroup(String world, String player, String group) {
            return playerInGroup(world, Lookups.player(player), group);
        }

        @Override
        public boolean playerAddGroup(String world, OfflinePlayer player, String group) {
            return lookups.apply(Change.setNode(user(player), Node.inherit(group, true).contexts(scope(lookups, world))));
        }

        @Override
        public boolean playerAddGroup(String world, String player, String group) {
            return playerAddGroup(world, Lookups.player(player), group);
        }

        @Override
        public boolean playerRemoveGroup(String world, OfflinePlayer player, String group) {
            return lookups.apply(Change.unsetNode(user(player), Node.inherit(group, true).contexts(scope(lookups, world))));
        }

        @Override
        public boolean playerRemoveGroup(String world, String player, String group) {
            return playerRemoveGroup(world, Lookups.player(player), group);
        }

        /** Every group the player is in for these contexts, inherited ones included, in rank order. */
        @Override
        public String[] getPlayerGroups(String world, OfflinePlayer player) {
            if (!lookups.load(player.getUniqueId())) {
                return new String[0];
            }
            return lookups.client().groupsOf(player.getUniqueId(), lookups.contexts(player, world)).toArray(String[]::new);
        }

        @Override
        public String[] getPlayerGroups(String world, String player) {
            return getPlayerGroups(world, Lookups.player(player));
        }

        @Override
        public String getPrimaryGroup(String world, OfflinePlayer player) {
            lookups.load(player.getUniqueId());
            return lookups.client().meta(player.getUniqueId(), lookups.contexts(player, world)).primaryGroup();
        }

        @Override
        public String getPrimaryGroup(String world, String player) {
            return getPrimaryGroup(world, Lookups.player(player));
        }

        @Override
        public String[] getGroups() {
            return lookups.client().groups().stream().map(GroupInfo::name).toArray(String[]::new);
        }
    }

    @SuppressWarnings("deprecation")
    private static final class VaultChat extends Chat {
        private final Plugin plugin;
        private final Lookups lookups;

        VaultChat(Permission permission, Plugin plugin, Lookups lookups) {
            super(permission);
            this.plugin = plugin;
            this.lookups = lookups;
        }

        @Override
        public String getName() {
            return plugin.getName();
        }

        @Override
        public boolean isEnabled() {
            return plugin.isEnabled();
        }

        private <T> T meta(String world, OfflinePlayer player, Function<io.github.jotrorox.rift.permissions.MetaView, T> read) {
            lookups.load(player.getUniqueId());
            return read.apply(lookups.client().meta(player.getUniqueId(), lookups.contexts(player, world)));
        }

        private Optional<String> metaValue(String world, OfflinePlayer player, String key) {
            return meta(world, player, view -> view.value(key.toLowerCase(java.util.Locale.ROOT)));
        }

        private Optional<String> groupMeta(String group, String key) {
            String normalized = key.toLowerCase(java.util.Locale.ROOT);
            return lookups.group(group).flatMap(info -> info.nodes().stream()
                    .filter(node -> node.contexts().isEmpty() && node.payload() instanceof Node.Meta meta
                            && meta.key().equals(normalized))
                    .map(node -> ((Node.Meta) node.payload()).value())
                    .findFirst());
        }

        private Optional<String> groupDecoration(String group, NodeKind kind) {
            return lookups.group(group).flatMap(info -> info.nodes().stream()
                    .filter(node -> node.contexts().isEmpty() && node.kind() == kind)
                    .max(java.util.Comparator.comparingInt(VaultChat::priority))
                    .map(node -> node.payload().storageValue()));
        }

        private static int priority(Node node) {
            return switch (node.payload()) {
                case Node.Prefix p -> p.priority();
                case Node.Suffix s -> s.priority();
                default -> Integer.MIN_VALUE;
            };
        }

        /**
         * Set a decoration so that it wins: ten above the highest priority the subject can see, as
         * LuckPerms' Vault hook does. {@code null} removes the subject's own decorations of that kind.
         */
        private boolean decorate(SubjectRef subject, String world, NodeKind kind, String text, List<Integer> visible) {
            ContextSet contexts = scope(lookups, world);
            if (text == null || text.isEmpty()) {
                return lookups.apply(Change.bulk(Filter.kind(kind).subject(subject).contexts(contexts), Patch.remove()));
            }
            int priority = visible.stream().mapToInt(Integer::intValue).max().orElse(90) + 10;
            Node node = kind == NodeKind.PREFIX ? Node.prefix(priority, text) : Node.suffix(priority, text);
            return lookups.apply(Change.setNode(subject, node.contexts(contexts)));
        }

        private List<Integer> playerPriorities(OfflinePlayer player, NodeKind kind) {
            lookups.load(player.getUniqueId());
            Optional<CompiledSubject> subject = lookups.client().compiled(player.getUniqueId());
            return subject.map(s -> (kind == NodeKind.PREFIX ? s.prefix() : s.suffix()).stream()
                    .map(CompiledSubject.Decoration::priority).toList()).orElse(List.of());
        }

        private List<Integer> groupPriorities(String group, NodeKind kind) {
            return lookups.group(group).map(info -> info.nodes().stream()
                    .filter(node -> node.kind() == kind).map(VaultChat::priority).toList()).orElse(List.of());
        }

        @Override
        public String getPlayerPrefix(String world, OfflinePlayer player) {
            return meta(world, player, view -> view.prefix() == null ? "" : view.prefix());
        }

        @Override
        public String getPlayerPrefix(String world, String player) {
            return getPlayerPrefix(world, Lookups.player(player));
        }

        @Override
        public void setPlayerPrefix(String world, OfflinePlayer player, String prefix) {
            decorate(user(player), world, NodeKind.PREFIX, prefix, playerPriorities(player, NodeKind.PREFIX));
        }

        @Override
        public void setPlayerPrefix(String world, String player, String prefix) {
            setPlayerPrefix(world, Lookups.player(player), prefix);
        }

        @Override
        public String getPlayerSuffix(String world, OfflinePlayer player) {
            return meta(world, player, view -> view.suffix() == null ? "" : view.suffix());
        }

        @Override
        public String getPlayerSuffix(String world, String player) {
            return getPlayerSuffix(world, Lookups.player(player));
        }

        @Override
        public void setPlayerSuffix(String world, OfflinePlayer player, String suffix) {
            decorate(user(player), world, NodeKind.SUFFIX, suffix, playerPriorities(player, NodeKind.SUFFIX));
        }

        @Override
        public void setPlayerSuffix(String world, String player, String suffix) {
            setPlayerSuffix(world, Lookups.player(player), suffix);
        }

        @Override
        public String getGroupPrefix(String world, String group) {
            return groupDecoration(group, NodeKind.PREFIX).orElse("");
        }

        @Override
        public void setGroupPrefix(String world, String group, String prefix) {
            decorate(SubjectRef.group(group), world, NodeKind.PREFIX, prefix, groupPriorities(group, NodeKind.PREFIX));
        }

        @Override
        public String getGroupSuffix(String world, String group) {
            return groupDecoration(group, NodeKind.SUFFIX).orElse("");
        }

        @Override
        public void setGroupSuffix(String world, String group, String suffix) {
            decorate(SubjectRef.group(group), world, NodeKind.SUFFIX, suffix, groupPriorities(group, NodeKind.SUFFIX));
        }

        private void setPlayerMeta(String world, OfflinePlayer player, String key, Object value) {
            lookups.apply(Change.setNode(user(player), Node.meta(key, String.valueOf(value)).contexts(scope(lookups, world))));
        }

        private void setGroupMeta(String world, String group, String key, Object value) {
            lookups.apply(Change.setNode(SubjectRef.group(group),
                    Node.meta(key, String.valueOf(value)).contexts(scope(lookups, world))));
        }

        private static int parseInt(Optional<String> value, int fallback) {
            try {
                return value.map(Integer::parseInt).orElse(fallback);
            } catch (NumberFormatException e) {
                return fallback;
            }
        }

        private static double parseDouble(Optional<String> value, double fallback) {
            try {
                return value.map(Double::parseDouble).orElse(fallback);
            } catch (NumberFormatException e) {
                return fallback;
            }
        }

        private static boolean parseBoolean(Optional<String> value, boolean fallback) {
            return value.filter(v -> v.equalsIgnoreCase("true") || v.equalsIgnoreCase("false"))
                    .map(Boolean::parseBoolean).orElse(fallback);
        }

        @Override
        public int getPlayerInfoInteger(String world, OfflinePlayer player, String node, int defaultValue) {
            return parseInt(metaValue(world, player, node), defaultValue);
        }

        @Override
        public int getPlayerInfoInteger(String world, String player, String node, int defaultValue) {
            return getPlayerInfoInteger(world, Lookups.player(player), node, defaultValue);
        }

        @Override
        public void setPlayerInfoInteger(String world, OfflinePlayer player, String node, int value) {
            setPlayerMeta(world, player, node, value);
        }

        @Override
        public void setPlayerInfoInteger(String world, String player, String node, int value) {
            setPlayerInfoInteger(world, Lookups.player(player), node, value);
        }

        @Override
        public int getGroupInfoInteger(String world, String group, String node, int defaultValue) {
            return parseInt(groupMeta(group, node), defaultValue);
        }

        @Override
        public void setGroupInfoInteger(String world, String group, String node, int value) {
            setGroupMeta(world, group, node, value);
        }

        @Override
        public double getPlayerInfoDouble(String world, OfflinePlayer player, String node, double defaultValue) {
            return parseDouble(metaValue(world, player, node), defaultValue);
        }

        @Override
        public double getPlayerInfoDouble(String world, String player, String node, double defaultValue) {
            return getPlayerInfoDouble(world, Lookups.player(player), node, defaultValue);
        }

        @Override
        public void setPlayerInfoDouble(String world, OfflinePlayer player, String node, double value) {
            setPlayerMeta(world, player, node, value);
        }

        @Override
        public void setPlayerInfoDouble(String world, String player, String node, double value) {
            setPlayerInfoDouble(world, Lookups.player(player), node, value);
        }

        @Override
        public double getGroupInfoDouble(String world, String group, String node, double defaultValue) {
            return parseDouble(groupMeta(group, node), defaultValue);
        }

        @Override
        public void setGroupInfoDouble(String world, String group, String node, double value) {
            setGroupMeta(world, group, node, value);
        }

        @Override
        public boolean getPlayerInfoBoolean(String world, OfflinePlayer player, String node, boolean defaultValue) {
            return parseBoolean(metaValue(world, player, node), defaultValue);
        }

        @Override
        public boolean getPlayerInfoBoolean(String world, String player, String node, boolean defaultValue) {
            return getPlayerInfoBoolean(world, Lookups.player(player), node, defaultValue);
        }

        @Override
        public void setPlayerInfoBoolean(String world, OfflinePlayer player, String node, boolean value) {
            setPlayerMeta(world, player, node, value);
        }

        @Override
        public void setPlayerInfoBoolean(String world, String player, String node, boolean value) {
            setPlayerInfoBoolean(world, Lookups.player(player), node, value);
        }

        @Override
        public boolean getGroupInfoBoolean(String world, String group, String node, boolean defaultValue) {
            return parseBoolean(groupMeta(group, node), defaultValue);
        }

        @Override
        public void setGroupInfoBoolean(String world, String group, String node, boolean value) {
            setGroupMeta(world, group, node, value);
        }

        @Override
        public String getPlayerInfoString(String world, OfflinePlayer player, String node, String defaultValue) {
            return metaValue(world, player, node).orElse(defaultValue);
        }

        @Override
        public String getPlayerInfoString(String world, String player, String node, String defaultValue) {
            return getPlayerInfoString(world, Lookups.player(player), node, defaultValue);
        }

        @Override
        public void setPlayerInfoString(String world, OfflinePlayer player, String node, String value) {
            setPlayerMeta(world, player, node, value);
        }

        @Override
        public void setPlayerInfoString(String world, String player, String node, String value) {
            setPlayerInfoString(world, Lookups.player(player), node, value);
        }

        @Override
        public String getGroupInfoString(String world, String group, String node, String defaultValue) {
            return groupMeta(group, node).orElse(defaultValue);
        }

        @Override
        public void setGroupInfoString(String world, String group, String node, String value) {
            setGroupMeta(world, group, node, value);
        }
    }
}
