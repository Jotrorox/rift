package io.github.jotrorox.rift.permissions;

import java.time.Duration;
import java.time.Instant;
import java.util.Objects;
import java.util.OptionalLong;

/**
 * A node on a user or group: a typed {@link Payload} with the contexts it applies in, an optional
 * expiry and its source. Every constructor normalises and validates, so a {@code Node} is always
 * valid.
 *
 * <pre>{@code
 * Node.permission("essentials.fly", true).context("server", "survival").expiresIn(Duration.ofHours(1))
 * }</pre>
 *
 * @param payload what the node says
 * @param contexts where it applies; empty is global
 * @param expiresAt Unix seconds after which it no longer applies; empty when permanent
 * @param source {@code store}, {@code config}, {@code import} or {@code plugin:<name>}
 */
public record Node(Payload payload, ContextSet contexts, OptionalLong expiresAt, String source) {
    /** Edited at runtime. */
    public static final String SOURCE_STORE = "store";
    /** Mirrored from Lua configuration. */
    public static final String SOURCE_CONFIG = "config";
    /** Written by a LuckPerms import. */
    public static final String SOURCE_IMPORT = "import";

    /**
     * @throws IllegalArgumentException when a reserved context key has two values, which could
     *     never apply, or the source is invalid
     */
    public Node {
        Objects.requireNonNull(payload, "payload");
        Objects.requireNonNull(contexts, "contexts");
        Objects.requireNonNull(expiresAt, "expiresAt");
        Names.source(Objects.requireNonNull(source, "source"));
        if (!contexts.isSatisfiable()) {
            throw Names.invalid("node contexts", contexts.toString(),
                    "a reserved key has more than one value, so the node could never apply");
        }
    }

    /**
     * @param payload what the node says
     * @return a global, permanent {@code store} node
     */
    public static Node of(Payload payload) {
        return new Node(payload, ContextSet.empty(), OptionalLong.empty(), SOURCE_STORE);
    }

    /**
     * @param pattern {@code a.b}, {@code a.*} or {@code *}; lowercased
     * @param value grant or deny
     * @return a permission node
     */
    public static Node permission(String pattern, boolean value) {
        return of(new Permission(pattern, value));
    }

    /**
     * @param group the parent group
     * @param value {@code false} blocks the group on every path
     * @return an inherit node
     */
    public static Node inherit(String group, boolean value) {
        return of(new Inherit(group, value));
    }

    /**
     * @param priority the highest priority wins
     * @param text the prefix
     * @return a prefix node
     */
    public static Node prefix(int priority, String text) {
        return of(new Prefix(priority, text));
    }

    /**
     * @param priority the highest priority wins
     * @param text the suffix
     * @return a suffix node
     */
    public static Node suffix(int priority, String text) {
        return of(new Suffix(priority, text));
    }

    /**
     * @param key the meta key; lowercased
     * @param value the value
     * @return a meta node
     */
    public static Node meta(String key, String value) {
        return of(new Meta(key, value));
    }

    /**
     * @param weight higher weights rank first
     * @return a weight node, valid on groups only
     */
    public static Node weight(int weight) {
        return of(new Weight(weight));
    }

    /**
     * @param name the display name
     * @return a display name node, valid on groups only
     */
    public static Node displayName(String name) {
        return of(new DisplayName(name));
    }

    /**
     * @param pattern a permission node as typed
     * @return the lowercased pattern
     * @throws IllegalArgumentException when it is not dot-separated segments of {@code [a-z0-9_:-]},
     *     at most 256 bytes, with {@code *} only as a whole trailing segment
     */
    public static String normalizePattern(String pattern) {
        return Names.node(pattern);
    }

    /**
     * @param pattern a pattern
     * @return whether it is already normalised and valid
     */
    public static boolean isValidPattern(String pattern) {
        return Names.validNode(pattern);
    }

    /** @return the payload's kind */
    public NodeKind kind() {
        return payload.kind();
    }

    /**
     * @param key the context key
     * @param value the context value
     * @return this node with one more context pair
     */
    public Node context(String key, String value) {
        return contexts(contexts.with(key, value));
    }

    /**
     * @param contexts the new contexts
     * @return this node applying in {@code contexts}
     */
    public Node contexts(ContextSet contexts) {
        return new Node(payload, contexts, expiresAt, source);
    }

    /**
     * @param epochSecond Unix seconds
     * @return this node expiring at {@code epochSecond}
     */
    public Node expiresAt(long epochSecond) {
        return new Node(payload, contexts, OptionalLong.of(epochSecond), source);
    }

    /**
     * @param instant the expiry
     * @return this node expiring at {@code instant}
     */
    public Node expiresAt(Instant instant) {
        return expiresAt(instant.getEpochSecond());
    }

    /**
     * @param duration how long from now
     * @return this node expiring {@code duration} from now
     */
    public Node expiresIn(Duration duration) {
        return expiresAt(Instant.now().plus(duration));
    }

    /** @return this node without an expiry */
    public Node permanent() {
        return new Node(payload, contexts, OptionalLong.empty(), source);
    }

    /**
     * @param source {@code store}, {@code config}, {@code import} or {@code plugin:<name>}
     * @return this node with another source
     */
    public Node source(String source) {
        return new Node(payload, contexts, expiresAt, source);
    }

    /**
     * @param now Unix seconds
     * @return whether the node no longer applies at {@code now}
     */
    public boolean isExpired(long now) {
        return expiresAt.isPresent() && expiresAt.getAsLong() <= now;
    }

    /** What a node says. Each record normalises and validates its fields. */
    public sealed interface Payload permits Permission, Inherit, Prefix, Suffix, Meta, Weight, DisplayName {
        /** @return the kind */
        NodeKind kind();

        /**
         * The storage key: one node exists per (kind, key, contexts) on a subject.
         *
         * @return the pattern, group, decimal priority, meta key, or the kind name for
         *     {@code weight} and {@code display_name}
         */
        String storageKey();

        /** @return {@code true}/{@code false}, the text, or the decimal weight */
        String storageValue();
    }

    /**
     * Grant or deny a pattern.
     *
     * @param key the pattern
     * @param value grant or deny
     */
    public record Permission(String key, boolean value) implements Payload {
        public Permission {
            key = Names.node(key);
        }

        @Override
        public NodeKind kind() {
            return NodeKind.PERMISSION;
        }

        @Override
        public String storageKey() {
            return key;
        }

        @Override
        public String storageValue() {
            return Boolean.toString(value);
        }
    }

    /**
     * Inherit a group, or with {@code false} block it.
     *
     * @param group the group
     * @param value inherit or block
     */
    public record Inherit(String group, boolean value) implements Payload {
        public Inherit {
            group = Names.group(group);
        }

        @Override
        public NodeKind kind() {
            return NodeKind.INHERIT;
        }

        @Override
        public String storageKey() {
            return group;
        }

        @Override
        public String storageValue() {
            return Boolean.toString(value);
        }
    }

    /**
     * Chat prefix; the highest priority wins.
     *
     * @param priority the priority
     * @param text at most 256 bytes without control characters
     */
    public record Prefix(int priority, String text) implements Payload {
        public Prefix {
            Names.text("prefix", text, Names.MAX_TEXT);
        }

        @Override
        public NodeKind kind() {
            return NodeKind.PREFIX;
        }

        @Override
        public String storageKey() {
            return Integer.toString(priority);
        }

        @Override
        public String storageValue() {
            return text;
        }
    }

    /**
     * Chat suffix; the highest priority wins.
     *
     * @param priority the priority
     * @param text at most 256 bytes without control characters
     */
    public record Suffix(int priority, String text) implements Payload {
        public Suffix {
            Names.text("suffix", text, Names.MAX_TEXT);
        }

        @Override
        public NodeKind kind() {
            return NodeKind.SUFFIX;
        }

        @Override
        public String storageKey() {
            return Integer.toString(priority);
        }

        @Override
        public String storageValue() {
            return text;
        }
    }

    /**
     * Arbitrary key/value metadata.
     *
     * @param key 1 to 64 bytes of {@code [a-z0-9_.:-]}
     * @param value at most 1024 bytes without control characters
     */
    public record Meta(String key, String value) implements Payload {
        public Meta {
            Names.text("meta value", value, Names.MAX_META_VALUE);
            key = Names.key("meta key", key);
        }

        @Override
        public NodeKind kind() {
            return NodeKind.META;
        }

        @Override
        public String storageKey() {
            return key;
        }

        @Override
        public String storageValue() {
            return value;
        }
    }

    /**
     * Group weight.
     *
     * @param weight higher weights rank first
     */
    public record Weight(int weight) implements Payload {
        @Override
        public NodeKind kind() {
            return NodeKind.WEIGHT;
        }

        @Override
        public String storageKey() {
            return NodeKind.WEIGHT.wireName();
        }

        @Override
        public String storageValue() {
            return Integer.toString(weight);
        }
    }

    /**
     * Group display name.
     *
     * @param name 1 to 256 bytes without control characters
     */
    public record DisplayName(String name) implements Payload {
        public DisplayName {
            if (name.isEmpty()) {
                throw Names.invalid("display name", name, "must not be empty");
            }
            Names.text("display name", name, Names.MAX_TEXT);
        }

        @Override
        public NodeKind kind() {
            return NodeKind.DISPLAY_NAME;
        }

        @Override
        public String storageKey() {
            return NodeKind.DISPLAY_NAME.wireName();
        }

        @Override
        public String storageValue() {
            return name;
        }
    }
}
