package io.github.jotrorox.rift.permissions;

import java.util.Objects;

/**
 * Selects nodes for {@link Change.Bulk}. Every non-null field must match; {@link #all()} matches
 * every node.
 *
 * <pre>{@code
 * Filter.kind(NodeKind.PERMISSION).keyLike("essentials.%").groups()
 * }</pre>
 *
 * @param subjects which kinds of subject
 * @param subject only this subject, or null
 * @param kind only this node kind, or null
 * @param keyLike SQL {@code LIKE} against the storage key ({@code %} any run, {@code _} one
 *     character, {@code \} escapes; ASCII letters case-insensitive), or null
 * @param value the exact storage value, such as {@code "false"}, or null
 * @param contexts exactly these contexts, or null
 * @param source only this source, or null
 */
public record Filter(
        Subjects subjects,
        SubjectRef subject,
        NodeKind kind,
        String keyLike,
        String value,
        ContextSet contexts,
        String source) {
    /** Which kinds of subject a filter covers. */
    public enum Subjects {
        ANY,
        USERS,
        GROUPS
    }

    public Filter {
        Objects.requireNonNull(subjects, "subjects");
        if (source != null) {
            Names.source(source);
        }
    }

    /** @return a filter matching every node */
    public static Filter all() {
        return new Filter(Subjects.ANY, null, null, null, null, null, null);
    }

    /**
     * @param kind the node kind
     * @return a filter matching nodes of {@code kind}
     */
    public static Filter kind(NodeKind kind) {
        return all().ofKind(kind);
    }

    /**
     * @param kind the node kind
     * @return this filter restricted to {@code kind}
     */
    public Filter ofKind(NodeKind kind) {
        return new Filter(subjects, subject, kind, keyLike, value, contexts, source);
    }

    /** @return this filter restricted to users */
    public Filter users() {
        return new Filter(Subjects.USERS, subject, kind, keyLike, value, contexts, source);
    }

    /** @return this filter restricted to groups */
    public Filter groups() {
        return new Filter(Subjects.GROUPS, subject, kind, keyLike, value, contexts, source);
    }

    /**
     * @param subject the only subject
     * @return this filter restricted to {@code subject}
     */
    public Filter subject(SubjectRef subject) {
        return new Filter(subjects, subject, kind, keyLike, value, contexts, source);
    }

    /**
     * @param pattern a SQL {@code LIKE} pattern for the storage key
     * @return this filter restricted to matching keys
     */
    public Filter keyLike(String pattern) {
        return new Filter(subjects, subject, kind, pattern, value, contexts, source);
    }

    /**
     * @param value the exact storage value
     * @return this filter restricted to {@code value}
     */
    public Filter value(String value) {
        return new Filter(subjects, subject, kind, keyLike, value, contexts, source);
    }

    /**
     * @param contexts exactly these contexts
     * @return this filter restricted to {@code contexts}
     */
    public Filter contexts(ContextSet contexts) {
        return new Filter(subjects, subject, kind, keyLike, value, contexts, source);
    }

    /**
     * @param source the node source
     * @return this filter restricted to {@code source}
     */
    public Filter source(String source) {
        return new Filter(subjects, subject, kind, keyLike, value, contexts, source);
    }
}
