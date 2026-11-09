package io.github.jotrorox.rift.permissions;

import java.util.Collections;
import java.util.Map;
import java.util.Objects;
import java.util.Optional;
import java.util.TreeMap;

/**
 * Chat metadata for one player in one set of contexts.
 *
 * @param prefix the winning prefix, or null
 * @param suffix the winning suffix, or null
 * @param meta the winning value per meta key, sorted by key
 * @param primaryGroup the player's primary group
 */
public record MetaView(String prefix, String suffix, Map<String, String> meta, String primaryGroup) {
    public MetaView {
        meta = Collections.unmodifiableMap(new TreeMap<>(meta));
        Objects.requireNonNull(primaryGroup, "primaryGroup");
    }

    /**
     * @param key a meta key
     * @return its value, if set
     */
    public Optional<String> value(String key) {
        return Optional.ofNullable(meta.get(key));
    }
}
