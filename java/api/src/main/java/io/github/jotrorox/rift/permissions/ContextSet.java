package io.github.jotrorox.rift.permissions;

import java.util.ArrayList;
import java.util.Collections;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.Set;

/**
 * An immutable, sorted, duplicate-free set of {@code key=value} pairs: the conditions a node applies
 * under, or the active contexts a check runs in.
 *
 * <p>Keys are 1 to 64 bytes of {@code [a-z0-9_.:-]}; values are 1 to 256 bytes without control
 * characters, {@code ;} or {@code =}. ASCII letters in both are lowercased. A key may have several
 * values. A node's contexts apply when every pair is in the active set ({@link #isSubsetOf}).
 *
 * <p>{@link #toString()} is the canonical form {@code k=v;k2=v2}, sorted by key then value; the
 * empty set is {@code ""}.
 */
public final class ContextSet implements Iterable<ContextSet.Pair> {
    /** The backend name a player is on. */
    public static final String SERVER = "server";
    /** The world name. */
    public static final String WORLD = "world";
    /** The dimension, for example {@code minecraft:the_nether}. */
    public static final String DIMENSION = "dimension";
    /** The game mode: {@code survival}, {@code creative}, {@code adventure} or {@code spectator}. */
    public static final String GAMEMODE = "gamemode";
    /** Keys Rift and the adapters fill themselves; each has one value in an active set. */
    public static final Set<String> RESERVED = Set.of(SERVER, WORLD, DIMENSION, GAMEMODE);

    private static final ContextSet EMPTY = new ContextSet(List.of());

    /**
     * One normalised pair.
     *
     * @param key the context key
     * @param value the context value
     */
    public record Pair(String key, String value) implements Comparable<Pair> {
        @Override
        public int compareTo(Pair other) {
            int byKey = Names.compare(key, other.key);
            return byKey != 0 ? byKey : Names.compare(value, other.value);
        }

        @Override
        public String toString() {
            return key + "=" + value;
        }
    }

    private final List<Pair> pairs;

    private ContextSet(List<Pair> pairs) {
        this.pairs = pairs;
    }

    /** @return the empty (global) set */
    public static ContextSet empty() {
        return EMPTY;
    }

    /**
     * @param key the context key
     * @param value the context value
     * @return a set with one pair
     */
    public static ContextSet of(String key, String value) {
        return builder().add(key, value).build();
    }

    /**
     * Parse the canonical form; the empty string is the empty set.
     *
     * @param canonical {@code k=v;k2=v2}
     * @return the set
     * @throws IllegalArgumentException when a pair is malformed or invalid
     */
    public static ContextSet parse(String canonical) {
        Builder builder = builder();
        if (canonical.isEmpty()) {
            return EMPTY;
        }
        for (String pair : canonical.split(";", -1)) {
            int equals = pair.indexOf('=');
            if (equals < 0) {
                throw Names.invalid("context set", canonical, "expected k=v;k2=v2");
            }
            builder.add(pair.substring(0, equals), pair.substring(equals + 1));
        }
        return builder.build();
    }

    /** @return an empty builder */
    public static Builder builder() {
        return new Builder();
    }

    /** @return a builder holding this set's pairs */
    public Builder toBuilder() {
        Builder builder = new Builder();
        builder.pairs.addAll(pairs);
        return builder;
    }

    /**
     * @param key the context key
     * @param value the context value
     * @return this set plus one pair
     */
    public ContextSet with(String key, String value) {
        return toBuilder().add(key, value).build();
    }

    /**
     * @param key the context key
     * @param value its only value
     * @return this set with every value of {@code key} replaced by {@code value}
     */
    public ContextSet withReplaced(String key, String value) {
        return toBuilder().set(key, value).build();
    }

    /**
     * @param key the context key, compared case-insensitively
     * @return this set without any value of {@code key}
     */
    public ContextSet without(String key) {
        return toBuilder().remove(key).build();
    }

    /**
     * @param key a normalised key
     * @param value a normalised value
     * @return whether the pair is present
     */
    public boolean contains(String key, String value) {
        return Collections.binarySearch(pairs, new Pair(key, value)) >= 0;
    }

    /**
     * @param key a normalised key
     * @return the values of {@code key}, sorted
     */
    public List<String> values(String key) {
        return pairs.stream().filter(p -> p.key.equals(key)).map(Pair::value).toList();
    }

    /**
     * @param key a normalised key
     * @return the first value of {@code key}; for reserved keys, the only one
     */
    public Optional<String> get(String key) {
        return pairs.stream().filter(p -> p.key.equals(key)).map(Pair::value).findFirst();
    }

    /** @return the number of pairs, which the "more contexts win" tie-break counts */
    public int size() {
        return pairs.size();
    }

    /** @return whether this is the global set */
    public boolean isEmpty() {
        return pairs.isEmpty();
    }

    /**
     * @param active the active contexts
     * @return whether every pair of this set is in {@code active}: the node-applies test
     */
    public boolean isSubsetOf(ContextSet active) {
        if (pairs.size() > active.pairs.size()) {
            return false;
        }
        for (Pair pair : pairs) {
            if (Collections.binarySearch(active.pairs, pair) < 0) {
                return false;
            }
        }
        return true;
    }

    /**
     * @param other another set
     * @return both sets' pairs
     */
    public ContextSet union(ContextSet other) {
        if (other.isEmpty()) {
            return this;
        }
        if (isEmpty()) {
            return other;
        }
        Builder builder = toBuilder();
        for (Pair pair : other.pairs) {
            builder.insert(pair);
        }
        return builder.build();
    }

    /** @return false when a reserved key has two values, which no active set can satisfy */
    public boolean isSatisfiable() {
        for (int i = 1; i < pairs.size(); i++) {
            String key = pairs.get(i).key;
            if (key.equals(pairs.get(i - 1).key) && RESERVED.contains(key)) {
                return false;
            }
        }
        return true;
    }

    /** @return the pairs in canonical order */
    public List<Pair> pairs() {
        return pairs;
    }

    /** @return keys in order, each with its sorted values */
    public Map<String, List<String>> asMap() {
        Map<String, List<String>> map = new LinkedHashMap<>();
        for (Pair pair : pairs) {
            map.computeIfAbsent(pair.key, key -> new ArrayList<>()).add(pair.value);
        }
        map.replaceAll((key, values) -> List.copyOf(values));
        return Collections.unmodifiableMap(map);
    }

    @Override
    public Iterator<Pair> iterator() {
        return pairs.iterator();
    }

    @Override
    public boolean equals(Object other) {
        return other instanceof ContextSet set && pairs.equals(set.pairs);
    }

    @Override
    public int hashCode() {
        return pairs.hashCode();
    }

    /** @return the canonical form {@code k=v;k2=v2} */
    @Override
    public String toString() {
        StringBuilder text = new StringBuilder();
        for (Pair pair : pairs) {
            if (!text.isEmpty()) {
                text.append(';');
            }
            text.append(pair.key).append('=').append(pair.value);
        }
        return text.toString();
    }

    /** Collects pairs, normalising and validating each. */
    public static final class Builder {
        private final List<Pair> pairs = new ArrayList<>();

        private Builder() {}

        /**
         * @param key the context key
         * @param value the context value
         * @return this builder
         * @throws IllegalArgumentException when the key or value is invalid
         */
        public Builder add(String key, String value) {
            insert(new Pair(normalizeKey(key), normalizeValue(value)));
            return this;
        }

        /**
         * Replace every value of {@code key}, as an adapter does when a player changes world.
         *
         * @param key the context key
         * @param value its only value
         * @return this builder
         */
        public Builder set(String key, String value) {
            Pair pair = new Pair(normalizeKey(key), normalizeValue(value));
            pairs.removeIf(p -> p.key.equals(pair.key));
            insert(pair);
            return this;
        }

        /**
         * @param key the context key, compared case-insensitively
         * @return this builder
         */
        public Builder remove(String key) {
            pairs.removeIf(p -> p.key.equalsIgnoreCase(key));
            return this;
        }

        /**
         * @param other pairs to add
         * @return this builder
         */
        public Builder addAll(ContextSet other) {
            other.pairs.forEach(this::insert);
            return this;
        }

        /** @return the immutable set */
        public ContextSet build() {
            return pairs.isEmpty() ? EMPTY : new ContextSet(List.copyOf(pairs));
        }

        private void insert(Pair pair) {
            int at = Collections.binarySearch(pairs, pair);
            if (at < 0) {
                pairs.add(-at - 1, pair);
            }
        }
    }

    private static String normalizeKey(String key) {
        return Names.key("context key", key);
    }

    private static String normalizeValue(String value) {
        if (value.isEmpty() || Names.utf8Length(value) > 256 || Names.hasControl(value)
                || value.indexOf(';') >= 0 || value.indexOf('=') >= 0) {
            throw Names.invalid("context value", value,
                    "use 1 to 256 bytes without control characters, ';' or '='");
        }
        return Names.asciiLower(value);
    }
}
