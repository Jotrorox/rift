package io.github.jotrorox.rift.permissions.paper;

import io.github.jotrorox.rift.permissions.client.OfflinePolicy;
import java.nio.file.Path;
import java.time.Duration;
import java.util.Objects;
import java.util.function.Function;
import org.bukkit.configuration.ConfigurationSection;

/**
 * {@code plugins/RiftPerms/config.yml}.
 *
 * <p>On a server Rift manages with a {@code messaging} block, {@code server}, {@code group} and the
 * {@code bus} settings may be left out: they fall back to {@code RIFT_SERVER_NAME}, {@code
 * RIFT_SERVER_GROUP}, {@code RIFT_BUS_ADDRESS}, {@code RIFT_BUS_SERVER_NAME}, {@code RIFT_BUS_CERT}
 * and the token in {@code RIFT_BUS_TOKEN}, which Rift sets for each instance, so one template
 * configuration serves a whole group.
 *
 * @param server this backend's name, the {@code server} context
 * @param group this backend's service group, the {@code group} context; null outside a group
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
 * @param chat the chat format, off unless enabled
 * @param tab the tab list, off unless enabled
 */
record PaperConfig(
        String server,
        String group,
        String host,
        int port,
        String serverName,
        Path certificate,
        String tokenEnv,
        Duration joinTimeout,
        Duration requestTimeout,
        OfflinePolicy offlinePolicy,
        boolean vault,
        boolean placeholders,
        Chat chat,
        Tab tab) {

    /**
     * {@code chat}: RiftPerms formats chat itself.
     *
     * @param enabled whether to format chat
     * @param format the line, legacy text with placeholders; holds {@code {message}}
     */
    record Chat(boolean enabled, String format) {
        static final String DEFAULT_FORMAT = "{prefix}{name}{suffix}&7: &r{message}";
    }

    /**
     * {@code tab}: RiftPerms decorates and sorts the tab list itself.
     *
     * @param enabled whether to touch the tab list
     * @param format a player's list name, legacy text with placeholders; empty leaves names alone
     * @param sort whether to order the list by primary group weight
     */
    record Tab(boolean enabled, String format, boolean sort) {
        static final String DEFAULT_FORMAT = "{prefix}{name}{suffix}";
    }

    /**
     * @param config the loaded configuration
     * @param folder the plugin's data folder, which relative paths resolve against
     * @throws IllegalArgumentException for a missing or malformed setting
     */
    static PaperConfig load(ConfigurationSection config, Path folder) {
        return load(config, folder, System::getenv);
    }

    /**
     * @param config the loaded configuration
     * @param folder the plugin's data folder, which relative paths resolve against
     * @param variables an environment variable's value, or null
     * @throws IllegalArgumentException for a missing or malformed setting
     */
    static PaperConfig load(ConfigurationSection config, Path folder, Function<String, String> variables) {
        String address = setting(config, "bus.address", variables, "RIFT_BUS_ADDRESS");
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
        String serverName = optional(config, "bus.server_name");
        if (serverName == null) {
            serverName = Objects.requireNonNullElse(variable(variables, "RIFT_BUS_SERVER_NAME"), host);
        }
        String group = optional(config, "group");
        String tokenEnv = optional(config, "bus.token_env");
        Chat chat = new Chat(config.getBoolean("chat.enabled", false),
                config.getString("chat.format", Chat.DEFAULT_FORMAT));
        if (chat.enabled() && !chat.format().contains("{message}")) {
            throw new IllegalArgumentException("chat.format must contain {message}");
        }
        Tab tab = new Tab(config.getBoolean("tab.enabled", false),
                config.getString("tab.format", Tab.DEFAULT_FORMAT), config.getBoolean("tab.sort", true));
        return new PaperConfig(
                setting(config, "server", variables, "RIFT_SERVER_NAME"),
                group != null ? group : variable(variables, "RIFT_SERVER_GROUP"),
                host,
                port,
                serverName,
                folder.resolve(setting(config, "bus.certificate", variables, "RIFT_BUS_CERT")),
                tokenEnv != null ? tokenEnv : "RIFT_BUS_TOKEN",
                Duration.ofMillis(config.getLong("join_timeout_ms", 2000)),
                Duration.ofMillis(config.getLong("request_timeout_ms", 5000)),
                policy,
                config.getBoolean("vault", true),
                config.getBoolean("placeholders", true),
                chat,
                tab);
    }

    /** The setting, or the variable Rift sets for managed servers when the setting is absent. */
    private static String setting(
            ConfigurationSection config, String path, Function<String, String> variables, String variable) {
        String value = optional(config, path);
        if (value == null) {
            value = variable(variables, variable);
        }
        if (value == null) {
            throw new IllegalArgumentException(path + " is required in config.yml (or " + variable
                    + ", which Rift sets for managed servers with a messaging block)");
        }
        return value;
    }

    private static String variable(Function<String, String> variables, String name) {
        String value = variables.apply(name);
        return value == null || value.isBlank() ? null : value;
    }

    private static String optional(ConfigurationSection config, String path) {
        String value = config.getString(path);
        return value == null || value.isBlank() ? null : value;
    }
}
