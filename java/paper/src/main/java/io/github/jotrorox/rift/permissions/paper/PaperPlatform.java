package io.github.jotrorox.rift.permissions.paper;

import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.client.PermissionsClient;
import io.github.jotrorox.rift.permissions.client.Platform;
import java.util.Locale;
import java.util.Map;
import java.util.UUID;
import java.util.concurrent.ConcurrentHashMap;
import java.util.logging.Logger;
import org.bukkit.World;
import org.bukkit.entity.Player;

/**
 * Players are Bukkit {@link Player}s. Their contexts are {@code server} from the configuration,
 * {@code world} and {@code gamemode} from the player, {@code dimension} from the world's
 * environment, plus every registered calculator's pairs; they are cached for 50 ms like LuckPerms
 * does, and dropped on world and game mode changes. Custom pairs are reported to the proxy when
 * they change.
 */
final class PaperPlatform implements Platform {
    private static final long CACHE_NANOS = 50_000_000;

    private record Cached(ContextSet contexts, long at) {}

    private final PermissionsClient client;
    private final String server;
    private final Logger logger;
    private final Map<UUID, Cached> cache = new ConcurrentHashMap<>();

    PaperPlatform(PermissionsClient client, String server, Logger logger) {
        this.client = client;
        this.server = server;
        this.logger = logger;
    }

    @Override
    public UUID uuid(Object player) {
        return player(player).getUniqueId();
    }

    @Override
    public ContextSet contexts(Object object) {
        Player player = player(object);
        long now = System.nanoTime();
        Cached cached = cache.get(player.getUniqueId());
        if (cached != null && now - cached.at() < CACHE_NANOS) {
            return cached.contexts();
        }
        ContextSet contexts = compute(player);
        cache.put(player.getUniqueId(), new Cached(contexts, now));
        if (player.isOnline()) {
            client.reportContexts(player.getUniqueId(), custom(contexts));
        }
        return contexts;
    }

    /** Forget a player's cached contexts, after a world or game mode change. */
    void invalidate(UUID uuid) {
        cache.remove(uuid);
    }

    private static Player player(Object player) {
        if (player instanceof Player bukkit) {
            return bukkit;
        }
        throw new IllegalArgumentException("players are org.bukkit.entity.Player on Paper");
    }

    private ContextSet compute(Player player) {
        ContextSet.Builder contexts = ContextSet.builder().add(ContextSet.SERVER, server);
        client.group().ifPresent(group -> contexts.add(ContextSet.GROUP, group));
        World world = player.getWorld();
        add(contexts, ContextSet.WORLD, world.getName());
        add(contexts, ContextSet.DIMENSION, switch (world.getEnvironment()) {
            case NORMAL -> "minecraft:overworld";
            case NETHER -> "minecraft:the_nether";
            case THE_END -> "minecraft:the_end";
            default -> world.getKey().asString();
        });
        add(contexts, ContextSet.GAMEMODE, player.getGameMode().name().toLowerCase(Locale.ROOT));
        return client.applyCalculators(player, contexts).build();
    }

    private void add(ContextSet.Builder contexts, String key, String value) {
        try {
            contexts.set(key, value);
        } catch (IllegalArgumentException e) {
            logger.fine(() -> "Skipping context " + key + "=" + value + ": " + e.getMessage());
        }
    }

    private static ContextSet custom(ContextSet contexts) {
        ContextSet.Builder custom = ContextSet.builder();
        for (ContextSet.Pair pair : contexts) {
            if (!ContextSet.RESERVED.contains(pair.key())) {
                custom.add(pair.key(), pair.value());
            }
        }
        return custom.build();
    }
}
