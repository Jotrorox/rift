package io.github.jotrorox.rift.permissions.test;

import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.RiftPermissions;
import io.github.jotrorox.rift.permissions.RiftPermissionsProvider;
import io.github.jotrorox.rift.permissions.Tristate;
import java.lang.reflect.Field;
import java.util.Locale;
import java.util.UUID;
import org.bukkit.Bukkit;
import org.bukkit.World;
import org.bukkit.entity.Player;
import org.bukkit.permissions.PermissibleBase;
import org.bukkit.permissions.Permission;
import org.bukkit.permissions.PermissionAttachmentInfo;
import org.bukkit.plugin.Plugin;
import org.bukkit.plugin.ServicePriority;
import org.mockbukkit.mockbukkit.entity.PlayerMock;

/**
 * Wires {@link InMemoryRiftPermissions} into a MockBukkit server the way RiftPerms wires the real
 * service into Paper: players are {@link Player}s with {@code server}, {@code world},
 * {@code dimension} and {@code gamemode} contexts, the service is registered with the services
 * manager and {@link RiftPermissions#get()}, and injected players answer {@code hasPermission}
 * from it.
 *
 * <pre>{@code
 * ServerMock server = MockBukkit.mock();
 * InMemoryRiftPermissions perms = MockBukkitPermissions.builder("survival")
 *         .group("default", group -> group.permission("kits.starter", true)).build();
 * MockBukkitPermissions.install(MockBukkit.createMockPlugin(), perms);
 * PlayerMock player = server.addPlayer();
 * MockBukkitPermissions.inject(player, perms);
 * assertTrue(player.hasPermission("kits.starter"));
 * }</pre>
 *
 * <p>Requires MockBukkit and the Paper API on the test classpath.
 */
public final class MockBukkitPermissions {
    private MockBukkitPermissions() {}

    /**
     * @param server the {@code server} context of every player
     * @return an in-memory builder whose players are Bukkit {@link Player}s
     */
    public static InMemoryRiftPermissions.Builder builder(String server) {
        return InMemoryRiftPermissions.builder()
                .server(server)
                .players(player -> player(player).getUniqueId(), player -> contexts(server, player(player)));
    }

    private static Player player(Object player) {
        if (player instanceof Player bukkit) {
            return bukkit;
        }
        throw new IllegalArgumentException("players are org.bukkit.entity.Player");
    }

    private static ContextSet contexts(String server, Player player) {
        World world = player.getWorld();
        String dimension = switch (world.getEnvironment()) {
            case NORMAL -> "minecraft:overworld";
            case NETHER -> "minecraft:the_nether";
            case THE_END -> "minecraft:the_end";
            default -> world.getKey().asString();
        };
        return ContextSet.builder()
                .add(ContextSet.SERVER, server)
                .add(ContextSet.WORLD, world.getName())
                .add(ContextSet.DIMENSION, dimension)
                .add(ContextSet.GAMEMODE, player.getGameMode().name().toLowerCase(Locale.ROOT))
                .build();
    }

    /**
     * Register {@code permissions} with the services manager (priority {@code Highest}) and behind
     * {@link RiftPermissions#get()}.
     *
     * @param plugin the owning plugin
     * @param permissions the service
     */
    public static void install(Plugin plugin, RiftPermissions permissions) {
        Bukkit.getServicesManager().register(RiftPermissions.class, permissions, plugin, ServicePriority.Highest);
        RiftPermissionsProvider.register(permissions);
    }

    /**
     * Undo {@link #install}.
     *
     * @param permissions the installed service
     */
    public static void uninstall(RiftPermissions permissions) {
        Bukkit.getServicesManager().unregister(permissions);
        RiftPermissionsProvider.unregister(permissions);
    }

    /**
     * Make {@code player.hasPermission} answer from {@code permissions}: attachments first, then the
     * service, then Bukkit's defaults and operator status, as RiftPerms does on Paper.
     *
     * @param player a MockBukkit player
     * @param permissions the service
     */
    public static void inject(PlayerMock player, RiftPermissions permissions) {
        for (Class<?> type = player.getClass(); type != null; type = type.getSuperclass()) {
            try {
                Field field = type.getDeclaredField("perms");
                field.setAccessible(true);
                field.set(player, new Injected(player, permissions));
                return;
            } catch (NoSuchFieldException e) {
                // Keep walking up the hierarchy.
            } catch (IllegalAccessException e) {
                throw new IllegalStateException("cannot replace the player's permissible", e);
            }
        }
        throw new IllegalStateException("this MockBukkit version has no PermissibleBase field named perms");
    }

    /** A permissible that consults attachments, then Rift, then Bukkit's own defaults. */
    private static final class Injected extends PermissibleBase {
        private final Player player;
        private final RiftPermissions permissions;

        Injected(Player player, RiftPermissions permissions) {
            super(player);
            this.player = player;
            this.permissions = permissions;
        }

        private Tristate attached(String name) {
            for (PermissionAttachmentInfo info : super.getEffectivePermissions()) {
                if (info.getAttachment() != null && info.getPermission().equalsIgnoreCase(name)) {
                    return Tristate.of(info.getValue());
                }
            }
            return Tristate.UNDEFINED;
        }

        private Tristate rift(String name) {
            UUID uuid = player.getUniqueId();
            Tristate attached = attached(name);
            return attached.isDefined() ? attached : permissions.check(uuid, name, permissions.contexts().of(player));
        }

        @Override
        public boolean hasPermission(String name) {
            Tristate answer = rift(name);
            return answer.isDefined() ? answer.asBoolean(false) : super.hasPermission(name);
        }

        @Override
        public boolean hasPermission(Permission permission) {
            Tristate answer = rift(permission.getName());
            return answer.isDefined() ? answer.asBoolean(false) : super.hasPermission(permission);
        }

        @Override
        public boolean isPermissionSet(String name) {
            return rift(name).isDefined() || super.isPermissionSet(name);
        }

        @Override
        public boolean isPermissionSet(Permission permission) {
            return isPermissionSet(permission.getName());
        }
    }
}
