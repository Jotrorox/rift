package io.github.jotrorox.rift.permissions.client;

import io.github.jotrorox.rift.permissions.CompiledSubject;
import io.github.jotrorox.rift.permissions.CompiledSubject.Decoration;
import io.github.jotrorox.rift.permissions.CompiledSubject.Entry;
import io.github.jotrorox.rift.permissions.CompiledSubject.EntryKind;
import io.github.jotrorox.rift.permissions.CompiledSubject.MetaEntry;
import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.MetaView;
import io.github.jotrorox.rift.permissions.Node;
import io.github.jotrorox.rift.permissions.Trace;
import io.github.jotrorox.rift.permissions.Trace.Candidate;
import io.github.jotrorox.rift.permissions.Trace.Outcome;
import io.github.jotrorox.rift.permissions.Trace.Specificity;
import io.github.jotrorox.rift.permissions.Tristate;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.OptionalLong;

/**
 * Stage 2 of resolution, a port of the Rust crate's {@code check} module: answer checks from a
 * compiled subject. Same rules, same results, held to the shared fixture corpus.
 *
 * <ol>
 *   <li>Entries that are expired ({@code exp <= now}) or whose contexts are not a subset of the
 *       active contexts are ignored.
 *   <li>The most specific match wins: the exact node, then the longest {@code prefix.*}, then
 *       {@code *}. A {@code group} entry for {@code vip} matches {@code group.vip} exactly.
 *   <li>Ties: more context pairs win, then the lower rank, then {@code false} beats {@code true}.
 *   <li>No match, or a node that is not a valid pattern after ASCII lowercasing, is undefined.
 * </ol>
 */
public final class Resolver {
    private Resolver() {}

    /**
     * @param pattern a compiled permission pattern
     * @param node a normalised node
     * @return how {@code pattern} matches {@code node}, or null
     */
    static Specificity patternMatch(String pattern, String node) {
        if (pattern.equals(node)) {
            return Specificity.EXACT;
        }
        if (pattern.equals("*")) {
            return Specificity.ANY;
        }
        if (!pattern.endsWith("*")) {
            return null;
        }
        String prefix = pattern.substring(0, pattern.length() - 1);
        return prefix.endsWith(".") && node.length() > prefix.length() && node.startsWith(prefix)
                ? Specificity.prefix(prefix.length())
                : null;
    }

    private static Specificity entryMatch(Entry entry, String node) {
        return switch (entry.kind()) {
            case PERMISSION -> patternMatch(entry.key(), node);
            case GROUP -> node.startsWith("group.") && node.substring("group.".length()).equals(entry.key())
                    ? Specificity.EXACT
                    : null;
        };
    }

    private static boolean expired(OptionalLong expiresAt, long now) {
        return expiresAt.isPresent() && expiresAt.getAsLong() <= now;
    }

    private static boolean applies(Entry entry, ContextSet active, long now) {
        return !expired(entry.expiresAt(), now) && entry.contexts().isSubsetOf(active);
    }

    /** The ordering key of a matching entry; the greatest wins. */
    private record Precedence(Specificity specificity, int contexts, int rank, boolean deny)
            implements Comparable<Precedence> {
        static Precedence of(Entry entry, Specificity specificity) {
            return new Precedence(specificity, entry.contexts().size(), entry.rank(), !entry.value());
        }

        @Override
        public int compareTo(Precedence other) {
            int result = specificity.compareTo(other.specificity);
            if (result == 0) {
                result = Integer.compare(contexts, other.contexts);
            }
            if (result == 0) {
                result = Integer.compare(other.rank, rank);
            }
            if (result == 0) {
                result = Boolean.compare(deny, other.deny);
            }
            return result;
        }
    }

    private static String normalized(String node) {
        if (Node.isValidPattern(node)) {
            return node;
        }
        try {
            return Node.normalizePattern(node);
        } catch (IllegalArgumentException e) {
            return null;
        }
    }

    /**
     * @param subject the player's compiled subject
     * @param node the permission as typed
     * @param active the active contexts
     * @param now Unix seconds
     * @return the answer
     */
    public static Tristate check(CompiledSubject subject, String node, ContextSet active, long now) {
        String normalized = normalized(node);
        if (normalized == null) {
            return Tristate.UNDEFINED;
        }
        Precedence best = null;
        boolean value = false;
        for (Entry entry : subject.entries()) {
            Specificity specificity = entryMatch(entry, normalized);
            if (specificity == null || !applies(entry, active, now)) {
                continue;
            }
            Precedence key = Precedence.of(entry, specificity);
            if (best == null || key.compareTo(best) > 0) {
                best = key;
                value = entry.value();
            }
        }
        return best == null ? Tristate.UNDEFINED : Tristate.of(value);
    }

    /**
     * {@link #check} with every matching entry and why it won or lost.
     *
     * @param subject the player's compiled subject
     * @param node the permission as typed
     * @param active the active contexts
     * @param now Unix seconds
     * @return the explanation
     */
    public static Trace trace(CompiledSubject subject, String node, ContextSet active, long now) {
        String normalized = normalized(node);
        if (normalized == null) {
            return new Trace(node, Tristate.UNDEFINED, null, List.of());
        }
        record Match(Entry entry, Specificity specificity, Outcome outcome) {}
        List<Match> matching = new ArrayList<>();
        int bestIndex = -1;
        Precedence best = null;
        for (Entry entry : subject.entries()) {
            Specificity specificity = entryMatch(entry, normalized);
            if (specificity == null) {
                continue;
            }
            Outcome outcome = null;
            if (expired(entry.expiresAt(), now)) {
                outcome = Outcome.EXPIRED;
            } else if (!entry.contexts().isSubsetOf(active)) {
                outcome = Outcome.CONTEXT_MISMATCH;
            } else {
                Precedence key = Precedence.of(entry, specificity);
                if (best == null || key.compareTo(best) > 0) {
                    best = key;
                    bestIndex = matching.size();
                }
            }
            matching.add(new Match(entry, specificity, outcome));
        }
        Candidate winner = null;
        List<Candidate> losers = new ArrayList<>();
        for (int index = 0; index < matching.size(); index++) {
            Match match = matching.get(index);
            Outcome outcome = match.outcome();
            if (outcome == null) {
                Entry entry = match.entry();
                if (index == bestIndex) {
                    outcome = Outcome.WON;
                } else if (match.specificity().compareTo(best.specificity()) < 0) {
                    outcome = Outcome.LESS_SPECIFIC;
                } else if (entry.contexts().size() < best.contexts()) {
                    outcome = Outcome.FEWER_CONTEXTS;
                } else if (entry.rank() > best.rank()) {
                    outcome = Outcome.HIGHER_RANK;
                } else if (!entry.value() != best.deny()) {
                    outcome = Outcome.DENY_WINS;
                } else {
                    outcome = Outcome.DUPLICATE;
                }
            }
            Candidate candidate = new Candidate(match.entry(), match.specificity(), outcome);
            if (outcome == Outcome.WON) {
                winner = candidate;
            } else {
                losers.add(candidate);
            }
        }
        Tristate result = winner == null ? Tristate.UNDEFINED : Tristate.of(winner.entry().value());
        return new Trace(normalized, result, winner, losers);
    }

    /**
     * Prefix, suffix, meta and primary group. Prefix and suffix: the highest priority wins, then the
     * lower rank, then more context pairs, then the first listed. Meta, per key: the lowest rank
     * wins, then more context pairs, then the first listed.
     *
     * @param subject the player's compiled subject
     * @param active the active contexts
     * @param now Unix seconds
     * @return the meta view
     */
    public static MetaView meta(CompiledSubject subject, ContextSet active, long now) {
        record Best(String value, int rank, int contexts) {}
        Map<String, Best> best = new LinkedHashMap<>();
        for (MetaEntry item : subject.meta()) {
            if (expired(item.expiresAt(), now) || !item.contexts().isSubsetOf(active)) {
                continue;
            }
            Best known = best.get(item.key());
            int size = item.contexts().size();
            if (known == null || item.rank() < known.rank() || (item.rank() == known.rank() && size > known.contexts())) {
                best.put(item.key(), new Best(item.value(), item.rank(), size));
            }
        }
        Map<String, String> meta = new LinkedHashMap<>();
        best.forEach((key, value) -> meta.put(key, value.value()));
        return new MetaView(decoration(subject.prefix(), active, now), decoration(subject.suffix(), active, now), meta,
                subject.primaryGroup());
    }

    private static String decoration(List<Decoration> list, ContextSet active, long now) {
        Decoration best = null;
        for (Decoration item : list) {
            if (expired(item.expiresAt(), now) || !item.contexts().isSubsetOf(active)) {
                continue;
            }
            if (best == null || item.priority() > best.priority()
                    || (item.priority() == best.priority() && (item.rank() < best.rank()
                            || (item.rank() == best.rank() && item.contexts().size() > best.contexts().size())))) {
                best = item;
            }
        }
        return best == null ? null : best.text();
    }

    /**
     * @param subject the player's compiled subject
     * @param active the active contexts
     * @param now Unix seconds
     * @return the groups the player is in, in rank order, each once
     */
    public static List<String> groups(CompiledSubject subject, ContextSet active, long now) {
        List<String> names = new ArrayList<>();
        for (Entry entry : subject.entries()) {
            if (entry.kind() == EntryKind.GROUP && applies(entry, active, now) && !names.contains(entry.key())) {
                names.add(entry.key());
            }
        }
        return names;
    }

    /**
     * @param subject the player's compiled subject
     * @param group a group name, compared ignoring ASCII case
     * @param active the active contexts
     * @param now Unix seconds
     * @return whether the player is in {@code group}, directly or inherited
     */
    public static boolean inGroup(CompiledSubject subject, String group, ContextSet active, long now) {
        for (Entry entry : subject.entries()) {
            if (entry.kind() == EntryKind.GROUP && asciiEqualsIgnoreCase(entry.key(), group) && applies(entry, active, now)) {
                return true;
            }
        }
        return false;
    }

    private static boolean asciiEqualsIgnoreCase(String a, String b) {
        if (a.length() != b.length()) {
            return false;
        }
        for (int i = 0; i < a.length(); i++) {
            if (asciiLower(a.charAt(i)) != asciiLower(b.charAt(i))) {
                return false;
            }
        }
        return true;
    }

    private static char asciiLower(char c) {
        return c >= 'A' && c <= 'Z' ? (char) (c + ('a' - 'A')) : c;
    }
}
