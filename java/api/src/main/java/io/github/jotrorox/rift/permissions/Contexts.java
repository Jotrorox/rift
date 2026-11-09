package io.github.jotrorox.rift.permissions;

import java.util.UUID;

/** Platform players, their active contexts and the custom calculators that contribute to them. */
public interface Contexts {
    /**
     * @param player a platform player ({@code org.bukkit.entity.Player} on Paper)
     * @return the player's active contexts: {@code server}, {@code world}, {@code dimension},
     *     {@code gamemode} and every calculator's pairs
     * @throws IllegalArgumentException when {@code player} is not a player of this platform
     */
    ContextSet of(Object player);

    /**
     * @param player a platform player
     * @return the player's UUID
     * @throws IllegalArgumentException when {@code player} is not a player of this platform
     */
    UUID uuid(Object player);

    /**
     * @param calculator a calculator called with the platform's player type
     * @return a subscription that unregisters it
     */
    Subscription register(ContextCalculator<?> calculator);
}
