package io.github.jotrorox.rift.permissions.paper;

import io.github.jotrorox.rift.permissions.client.OfflinePolicy;
import java.nio.file.Path;
import java.time.Duration;
import org.bukkit.configuration.ConfigurationSection;

/**
 * {@code plugins/RiftPerms/config.yml}.
 *
 * @param server this backend's name, the {@code server} context
 * @param host the messaging listener's host
 * @param port the messaging listener's UDP port
 * @param serverName the DNS name in the listener's certificate
 * @param certificate the trusted certificate file
 * @param tokenEnv the environment variable holding the bus token
 * @param joinTimeout how long a joining player waits for a snapshot
 * @param requestTimeout how long requests to the proxy wait
 * @param offlinePolicy what players without a snapshot get
 * @param vault whether to register Vault providers
 * @param placeholders whether to register the PlaceholderAPI expansion
 */
record PaperConfig(
        String server,
        String host,
        int port,
        String serverName,
        Path certificate,
        String tokenEnv,
        Duration joinTimeout,
        Duration requestTimeout,
        OfflinePolicy offlinePolicy,
        boolean vault,
        boolean placeholders) {

    /**
     * @param config the loaded configuration
     * @param folder the plugin's data folder, which relative paths resolve against
     * @throws IllegalArgumentException for a missing or malformed setting
     */
    static PaperConfig load(ConfigurationSection config, Path folder) {
        String address = require(config, "bus.address");
        int colon = address.lastIndexOf(':');
        if (colon <= 0) {
            throw new IllegalArgumentException("bus.address must be host:port, got \"" + address + "\"");
        }
        int port;
        try {
            port = Integer.parseInt(address.substring(colon + 1));
        } catch (NumberFormatException e) {
            throw new IllegalArgumentException("bus.address must be host:port, got \"" + address + "\"", e);
        }
        String host = address.substring(0, colon).replace("[", "").replace("]", "");
        OfflinePolicy policy = switch (config.getString("offline_policy", "deny")) {
            case "deny" -> OfflinePolicy.deny();
            case "last_snapshot" -> OfflinePolicy.lastSnapshot(folder.resolve(config.getString("snapshot_file", "replica.json")));
            case "fallback_file" -> OfflinePolicy.fallbackFile(folder.resolve(config.getString("fallback_file", "fallback.json")));
            default -> throw new IllegalArgumentException("offline_policy must be deny, last_snapshot or fallback_file");
        };
        return new PaperConfig(
                require(config, "server"),
                host,
                port,
                config.getString("bus.server_name", host),
                folder.resolve(require(config, "bus.certificate")),
                require(config, "bus.token_env"),
                Duration.ofMillis(config.getLong("join_timeout_ms", 2000)),
                Duration.ofMillis(config.getLong("request_timeout_ms", 5000)),
                policy,
                config.getBoolean("vault", true),
                config.getBoolean("placeholders", true));
    }

    private static String require(ConfigurationSection config, String path) {
        String value = config.getString(path);
        if (value == null || value.isBlank()) {
            throw new IllegalArgumentException(path + " is required in config.yml");
        }
        return value;
    }
}
