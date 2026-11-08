package io.github.jotrorox.rift.permissions;

import java.util.List;
import java.util.Objects;

/**
 * Why a check answered what it did, for {@code /perms trace}: the winning entry and every other
 * entry matching the node, in compiled order, with the reason it lost.
 *
 * @param node the normalised node
 * @param result the answer
 * @param winner the deciding entry, or null when nothing applied
 * @param losers every other matching entry
 */
public record Trace(String node, Tristate result, Candidate winner, List<Candidate> losers) {
    public Trace {
        Objects.requireNonNull(node, "node");
        Objects.requireNonNull(result, "result");
        losers = List.copyOf(losers);
    }

    /**
     * A matching entry and how it fared.
     *
     * @param entry the entry
     * @param specificity how closely it matched
     * @param outcome why it won or lost
     */
    public record Candidate(CompiledSubject.Entry entry, Specificity specificity, Outcome outcome) {}

    /**
     * How closely a pattern matches a node; greater is more specific.
     *
     * @param level any, prefix or exact
     * @param length the prefix length in bytes including the dot, for {@link Level#PREFIX}
     */
    public record Specificity(Level level, int length) implements Comparable<Specificity> {
        /** {@code *}. */
        public static final Specificity ANY = new Specificity(Level.ANY, 0);
        /** The node itself. */
        public static final Specificity EXACT = new Specificity(Level.EXACT, 0);

        /** The match levels, least specific first. */
        public enum Level {
            ANY,
            PREFIX,
            EXACT
        }

        /**
         * @param length the prefix length in bytes, including the trailing dot
         * @return a prefix specificity
         */
        public static Specificity prefix(int length) {
            return new Specificity(Level.PREFIX, length);
        }

        @Override
        public int compareTo(Specificity other) {
            int byLevel = level.compareTo(other.level);
            return byLevel != 0 ? byLevel : Integer.compare(length, other.length);
        }
    }

    /** Why an entry did or did not decide a check. */
    public enum Outcome {
        /** It decided the check. */
        WON("won"),
        /** It had expired. */
        EXPIRED("expired"),
        /** Its contexts are not all active. */
        CONTEXT_MISMATCH("context_mismatch"),
        /** A more specific pattern won. */
        LESS_SPECIFIC("less_specific"),
        /** An entry with more context pairs won. */
        FEWER_CONTEXTS("fewer_contexts"),
        /** A lower rank won. */
        HIGHER_RANK("higher_rank"),
        /** Same specificity, contexts and rank: {@code false} beats {@code true}. */
        DENY_WINS("deny_wins"),
        /** Identical precedence and value to the winner, which came first. */
        DUPLICATE("duplicate");

        private final String wireName;

        Outcome(String wireName) {
            this.wireName = wireName;
        }

        /** @return the JSON spelling */
        public String wireName() {
            return wireName;
        }
    }
}
