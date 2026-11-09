package io.github.jotrorox.rift.permissions.paper;

import io.github.jotrorox.rift.messaging.MessagingClient;
import io.github.jotrorox.rift.permissions.RiftPermissions;
import io.github.jotrorox.rift.permissions.RiftPermissionsProvider;
import io.github.jotrorox.rift.permissions.SubjectUpdatedEvent;
import io.github.jotrorox.rift.permissions.client.PermissionsClient;
import java.io.IOException;
import java.nio.file.Path;
import java.util.Objects;
import java.util.logging.Level;
import org.bukkit.command.PluginCommand;
import org.bukkit.entity.Player;
import org.bukkit.plugin.ServicePriority;
import org.bukkit.plugin.java.JavaPlugin;

/**
 * RiftPerms: Rift's permission service on Paper, Folia and Purpur. It follows the proxy over the
 * messaging bus, answers {@code hasPermission} from the replica through an injected permissible,
 * registers {@link RiftPermissions} with the services manager, and bridges Vault, PlaceholderAPI and
 * {@code /perms}.
 */
public final class RiftPermsPlugin extends JavaPlugin {
    private MessagingClient bus;
    private PermissionsClient client;
    private PlayerListener players;

    @Override
    public void onEnable() {
        saveDefaultConfig();
        Path folder = getDataFolder().toPath();
        PaperConfig config;
        try {
            config = PaperConfig.load(getConfig(), folder);
        } catch (IllegalArgumentException e) {
            getLogger().severe("Invalid config.yml: " + e.getMessage() + "; RiftPerms stays disabled");
            getServer().getPluginManager().disablePlugin(this);
            return;
        }
        String token = System.getenv(config.tokenEnv());
        if (token == null || token.isEmpty()) {
            getLogger().severe("Set the bus token in the environment variable " + config.tokenEnv()
                    + "; RiftPerms stays disabled");
            getServer().getPluginManager().disablePlugin(this);
            return;
        }
        try {
            bus = MessagingClient.builder()
                    .address(config.host(), config.port())
                    .serverName(config.serverName())
                    .trust(config.certificate())
                    .token(token)
                    .build();
            client = PermissionsClient.builder()
                    .messaging(bus)
                    .server(config.server())
                    .group(config.group())
                    .joinTimeout(config.joinTimeout())
                    .requestTimeout(config.requestTimeout())
                    .offlinePolicy(config.offlinePolicy())
                    .logger(getLogger())
                    .platform(permissions -> new PaperPlatform(permissions, config.server(), getLogger()))
                    .build();
        } catch (IOException | RuntimeException e) {
            getLogger().log(Level.SEVERE, "Cannot start Rift permissions; RiftPerms stays disabled", e);
            if (bus != null) {
                bus.close();
            }
            getServer().getPluginManager().disablePlugin(this);
            return;
        }
        PaperPlatform paper = (PaperPlatform) client.platform();

        getServer().getServicesManager().register(RiftPermissions.class, client, this, ServicePriority.Highest);
        RiftPermissionsProvider.register(client);

        players = new PlayerListener(this, client, paper, config.joinTimeout());
        getServer().getPluginManager().registerEvents(players, this);
        for (Player online : getServer().getOnlinePlayers()) {
            client.playerJoined(online.getUniqueId());
            players.inject(online);
        }
        client.events().subscribe(SubjectUpdatedEvent.class, event -> {
            Player player = getServer().getPlayer(event.uuid());
            if (player != null) {
                players.refresh(player);
            }
        });

        PermsCommand perms = new PermsCommand(this, client);
        PluginCommand command = Objects.requireNonNull(getCommand("perms"), "perms is declared in plugin.yml");
        command.setExecutor(perms);
        command.setTabCompleter(perms);

        Lookups lookups = new Lookups(client, paper, config.server(), config.requestTimeout(), getLogger());
        if (config.vault() && getServer().getPluginManager().getPlugin("Vault") != null) {
            VaultHook.register(this, lookups);
            getLogger().info("Registered Vault permission and chat providers");
        }
        if (config.placeholders() && getServer().getPluginManager().getPlugin("PlaceholderAPI") != null) {
            PlaceholderHook.register(this, client, lookups);
            getLogger().info("Registered the %rift_...% placeholders");
        }
        // One tick after startup every plugin is enabled, so every plugin.yml permission is known.
        getServer().getGlobalRegionScheduler().runDelayed(this,
                task -> client.register(PermissionRegistrar.collect(getServer().getPluginManager().getPlugins())), 1);
    }

    @Override
    public void onDisable() {
        if (client == null) {
            return;
        }
        players.restoreAll(getServer().getOnlinePlayers());
        getServer().getServicesManager().unregisterAll(this);
        RiftPermissionsProvider.unregister(client);
        client.close();
        bus.close();
        client = null;
    }
}
