package io.github.jotrorox.rift.permissions;

import java.util.List;
import java.util.Objects;
import java.util.Optional;
import java.util.OptionalLong;
import java.util.UUID;

/**
 * The flattened, ranked view of one player that the proxy compiles and every backend evaluates:
 * the payload of {@code rift.perms.update.<uuid>} and the reply to {@code rift.perms.subject.get}.
 * Rank 0 is the player's own nodes, then 1, 2, ... per group in precedence order.
 *
 * @param revision the store revision it was built from
 * @param uuid the player
 * @param primaryGroup the player's primary group
 * @param entries permissions and group memberships
 * @param prefix ranked prefixes
 * @param suffix ranked suffixes
 * @param meta ranked meta values
 */
public record CompiledSubject(
        long revision,
        UUID uuid,
        String primaryGroup,
        List<Entry> entries,
        List<Decoration> prefix,
        List<Decoration> suffix,
        List<MetaEntry> meta) {
    public CompiledSubject {
        Objects.requireNonNull(uuid, "uuid");
        Objects.requireNonNull(primaryGroup, "primaryGroup");
        entries = List.copyOf(entries);
        prefix = List.copyOf(prefix);
        suffix = List.copyOf(suffix);
        meta = List.copyOf(meta);
    }

    /** What an {@link Entry} is about. */
    public enum EntryKind {
        /** A permission pattern ({@code "perm"}). */
        PERMISSION("perm"),
        /** Membership of the group named by the key ({@code "group"}); matches {@code group.<key>}. */
        GROUP("group");

        private final String wireName;

        EntryKind(String wireName) {
            this.wireName = wireName;
        }

        /** @return {@code perm} or {@code group} */
        public String wireName() {
            return wireName;
        }
    }

    /**
     * One ranked permission or group membership.
     *
     * @param kind permission or group
     * @param key the pattern, or the group name
     * @param value the value; always true for groups
     * @param contexts where it applies
     * @param rank 0 for the player's own nodes, then per group
     * @param expiresAt Unix seconds, or empty when permanent
     */
    public record Entry(
            EntryKind kind, String key, boolean value, ContextSet contexts, int rank, OptionalLong expiresAt) {
        public Entry {
            Objects.requireNonNull(kind, "kind");
            Objects.requireNonNull(key, "key");
            Objects.requireNonNull(contexts, "contexts");
            Objects.requireNonNull(expiresAt, "expiresAt");
        }
    }

    /**
     * A ranked prefix or suffix.
     *
     * @param priority the highest priority wins
     * @param text the decoration
     * @param contexts where it applies
     * @param rank the subject's rank
     * @param expiresAt Unix seconds, or empty when permanent
     */
    public record Decoration(int priority, String text, ContextSet contexts, int rank, OptionalLong expiresAt) {
        public Decoration {
            Objects.requireNonNull(text, "text");
            Objects.requireNonNull(contexts, "contexts");
            Objects.requireNonNull(expiresAt, "expiresAt");
        }
    }

    /**
     * A ranked meta value.
     *
     * @param key the meta key
     * @param value the value
     * @param contexts where it applies
     * @param rank the subject's rank
     * @param expiresAt Unix seconds, or empty when permanent
     */
    public record MetaEntry(String key, String value, ContextSet contexts, int rank, OptionalLong expiresAt) {
        public MetaEntry {
            Objects.requireNonNull(key, "key");
            Objects.requireNonNull(value, "value");
            Objects.requireNonNull(contexts, "contexts");
            Objects.requireNonNull(expiresAt, "expiresAt");
        }
    }

    /** @return the number of items in all four lists, which the proxy caps at 4096 */
    public int size() {
        return entries.size() + prefix.size() + suffix.size() + meta.size();
    }

    /**
     * @param rank a rank above 0
     * @return the group behind {@code rank}, as {@code /perms trace} names it; empty for rank 0
     */
    public Optional<String> rankSubject(int rank) {
        return entries.stream()
                .filter(e -> e.kind == EntryKind.GROUP && e.rank == rank && rank > 0)
                .map(Entry::key)
                .findFirst();
    }
}
