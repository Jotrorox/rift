package io.github.jotrorox.rift.permissions;

/**
 * Adds custom contexts for a player, such as {@code region=spawn}. The platform calls it with its
 * own player type ({@code org.bukkit.entity.Player} on Paper) whenever it computes a player's
 * active contexts, possibly off the main thread, so it must be fast and thread-safe. Keys that are
 * not reserved are reported to the proxy when they change.
 *
 * @param <T> the platform's player type
 */
@FunctionalInterface
public interface ContextCalculator<T> {
    /**
     * @param player the player
     * @param contexts the builder to add pairs to
     */
    void calculate(T player, ContextSet.Builder contexts);
}
