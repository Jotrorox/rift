package io.github.jotrorox.rift.permissions.client;

import io.github.jotrorox.rift.permissions.CompiledSubject;
import io.github.jotrorox.rift.permissions.CompiledSubject.Decoration;
import io.github.jotrorox.rift.permissions.CompiledSubject.Entry;
import io.github.jotrorox.rift.permissions.CompiledSubject.EntryKind;
import io.github.jotrorox.rift.permissions.CompiledSubject.MetaEntry;
import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.Node;
import io.github.jotrorox.rift.permissions.RiftPermissions;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;
import java.util.Map;
import java.util.OptionalLong;
import java.util.TreeMap;
import java.util.UUID;

/**
 * Stage 1 of resolution, a port of the Rust crate's {@code compiled} module: flattens a user's
 * inheritance into the {@link CompiledSubject} the proxy would send. Backends never compile what
 * the proxy serves; the client uses it for {@link OfflinePolicy#fallbackFile fallback files} and
 * {@code rift-permissions-test} for its in-memory store. The fixture corpus checks it entry for
 * entry.
 */
public final class Compiler {
    /** Default cap on all entries of a compiled subject. */
    public static final int MAX_ENTRIES = 4096;

    /**
     * A user as the resolver sees it.
     *
     * @param uuid the player
     * @param primaryGroup the explicit primary group, or null
     * @param nodes own nodes in insertion order
     */
    public record User(UUID uuid, String primaryGroup, List<Node> nodes) {
        public User {
            nodes = List.copyOf(nodes);
        }
    }

    /**
     * A group as the resolver sees it.
     *
     * @param name the normalised name
     * @param weight higher weights rank first
     * @param displayName the display name, or null
     * @param nodes own nodes in insertion order
     */
    public record Group(String name, int weight, String displayName, List<Node> nodes) {
        public Group {
            nodes = List.copyOf(nodes);
        }
    }

    /** One way a group is reached; ordered like the Rust derive (contexts, expiry, depth). */
    private record Path(ContextSet contexts, Long expiresAt, int depth) {}

    private record Block(String group, ContextSet contexts) {}

    private record Edge(String group, Path path) {}

    private static final Comparator<ContextSet> CONTEXTS = (a, b) -> {
        List<ContextSet.Pair> x = a.pairs();
        List<ContextSet.Pair> y = b.pairs();
        for (int i = 0; i < Math.min(x.size(), y.size()); i++) {
            int order = x.get(i).compareTo(y.get(i));
            if (order != 0) {
                return order;
            }
        }
        return Integer.compare(x.size(), y.size());
    };

    private static final Comparator<Path> PATHS = Comparator.comparing(Path::contexts, CONTEXTS)
            .thenComparing(Path::expiresAt, Comparator.nullsFirst(Comparator.naturalOrder()))
            .thenComparingInt(Path::depth);

    private Compiler() {}

    private static Long earliest(Long a, Long b) {
        if (a == null) {
            return b;
        }
        return b == null ? a : Math.min(a, b);
    }

    private static OptionalLong optional(Long value) {
        return value == null ? OptionalLong.empty() : OptionalLong.of(value);
    }

    private static Long boxed(OptionalLong value) {
        return value.isPresent() ? value.getAsLong() : null;
    }

    /** Whether {@code a} applies wherever and whenever {@code b} does. */
    private static boolean dominates(Path a, Path b) {
        boolean lasts = a.expiresAt() == null || (b.expiresAt() != null && a.expiresAt() >= b.expiresAt());
        return lasts && a.contexts().isSubsetOf(b.contexts());
    }

    /** Breadth-first walk of inheritance: the surviving paths per group and the negations seen. */
    private static TreeMap<String, List<Path>> walk(
            User user, Map<String, Group> groups, long now, List<Block> blocks, List<Block> found, int limit) {
        TreeMap<String, List<Path>> paths = new TreeMap<>();
        ArrayDeque<Edge> queue = new ArrayDeque<>();
        int[] total = {0};
        Path root = new Path(ContextSet.empty(), null, 0);
        for (Edge edge : edges(user.nodes(), root, groups, now, found)) {
            visit(edge, blocks, paths, queue, total, limit);
        }
        visit(new Edge(RiftPermissions.DEFAULT_GROUP, new Path(ContextSet.empty(), null, 1)), blocks, paths, queue,
                total, limit);
        while (!queue.isEmpty()) {
            Edge from = queue.pollFirst();
            Group group = groups.get(from.group());
            if (group != null) {
                for (Edge edge : edges(group.nodes(), from.path(), groups, now, found)) {
                    visit(edge, blocks, paths, queue, total, limit);
                }
            }
        }
        for (Map.Entry<String, List<Path>> entry : paths.entrySet()) {
            List<Path> all = entry.getValue();
            List<Path> kept = new ArrayList<>();
            for (int index = 0; index < all.size(); index++) {
                Path path = all.get(index);
                boolean dominated = false;
                for (int other = 0; other < all.size() && !dominated; other++) {
                    Path known = all.get(other);
                    dominated = other != index && dominates(known, path) && (!dominates(path, known) || other < index);
                }
                if (!dominated) {
                    kept.add(path);
                }
            }
            kept.sort(PATHS);
            entry.setValue(kept);
        }
        paths.values().removeIf(List::isEmpty);
        return paths;
    }

    private static void visit(Edge edge, List<Block> blocks, Map<String, List<Path>> paths, ArrayDeque<Edge> queue,
            int[] total, int limit) {
        for (Block block : blocks) {
            if (block.group().equals(edge.group()) && block.contexts().isSubsetOf(edge.path().contexts())) {
                return;
            }
        }
        List<Path> list = paths.computeIfAbsent(edge.group(), name -> new ArrayList<>());
        for (Path known : list) {
            if (dominates(known, edge.path())) {
                return;
            }
        }
        if (++total[0] > limit) {
            throw new IllegalStateException("compiled subject exceeds " + limit + " entries");
        }
        list.add(edge.path());
        queue.addLast(edge);
    }

    private static List<Edge> edges(List<Node> nodes, Path from, Map<String, Group> groups, long now,
            List<Block> found) {
        List<Edge> next = new ArrayList<>();
        for (Node node : nodes) {
            if (!(node.payload() instanceof Node.Inherit inherit) || node.isExpired(now)) {
                continue;
            }
            ContextSet contexts = from.contexts().union(node.contexts());
            if (!contexts.isSatisfiable()) {
                continue;
            }
            if (!inherit.value()) {
                found.add(new Block(inherit.group(), contexts));
            } else if (inherit.group().equals(RiftPermissions.DEFAULT_GROUP) || groups.containsKey(inherit.group())) {
                next.add(new Edge(inherit.group(),
                        new Path(contexts, earliest(from.expiresAt(), boxed(node.expiresAt())), from.depth() + 1)));
            }
        }
        return next;
    }

    /** Accumulates the four lists and enforces the entry cap. */
    private static final class Builder {
        final List<Entry> entries = new ArrayList<>();
        final List<Decoration> prefix = new ArrayList<>();
        final List<Decoration> suffix = new ArrayList<>();
        final List<MetaEntry> meta = new ArrayList<>();
        final int limit;
        final long now;

        Builder(int limit, long now) {
            this.limit = limit;
            this.now = now;
        }

        void check() {
            if (entries.size() + prefix.size() + suffix.size() + meta.size() > limit) {
                throw new IllegalStateException("compiled subject exceeds " + limit + " entries");
            }
        }

        void nodes(List<Node> nodes, int rank, Path path) {
            for (Node node : nodes) {
                if (node.isExpired(now)) {
                    continue;
                }
                ContextSet contexts = path.contexts().union(node.contexts());
                if (!contexts.isSatisfiable()) {
                    continue;
                }
                OptionalLong expiresAt = optional(earliest(path.expiresAt(), boxed(node.expiresAt())));
                switch (node.payload()) {
                    case Node.Permission p ->
                            entries.add(new Entry(EntryKind.PERMISSION, p.key(), p.value(), contexts, rank, expiresAt));
                    case Node.Prefix p -> prefix.add(new Decoration(p.priority(), p.text(), contexts, rank, expiresAt));
                    case Node.Suffix p -> suffix.add(new Decoration(p.priority(), p.text(), contexts, rank, expiresAt));
                    case Node.Meta p -> meta.add(new MetaEntry(p.key(), p.value(), contexts, rank, expiresAt));
                    case Node.Inherit ignored -> {
                        continue;
                    }
                    case Node.Weight ignored -> {
                        continue;
                    }
                    case Node.DisplayName ignored -> {
                        continue;
                    }
                }
                check();
            }
        }
    }

    /**
     * Compile {@code user} against {@code groups} at {@code now}, tagged with {@code revision}.
     *
     * @param user the user
     * @param groups every group by normalised name
     * @param now Unix seconds; nodes expired by then are left out
     * @param revision the revision to tag the subject with
     * @return the compiled subject
     * @throws IllegalStateException beyond {@link #MAX_ENTRIES}
     */
    public static CompiledSubject compile(User user, Map<String, Group> groups, long now, long revision) {
        List<Block> blocks = new ArrayList<>();
        walk(user, groups, now, List.of(), blocks, MAX_ENTRIES);
        TreeMap<String, List<Path>> paths = walk(user, groups, now, blocks, new ArrayList<>(), MAX_ENTRIES);
        List<Map.Entry<String, List<Path>>> ranked = new ArrayList<>(paths.entrySet());
        ranked.sort(Comparator.<Map.Entry<String, List<Path>>, Boolean>comparing(
                        e -> e.getKey().equals(RiftPermissions.DEFAULT_GROUP))
                .thenComparing(e -> weight(groups, e.getKey()), Comparator.reverseOrder())
                .thenComparingInt(e -> e.getValue().stream().mapToInt(Path::depth).min().orElseThrow())
                .thenComparing(Map.Entry::getKey));
        Builder builder = new Builder(MAX_ENTRIES, now);
        builder.nodes(user.nodes(), 0, new Path(ContextSet.empty(), null, 0));
        int rank = 1;
        for (Map.Entry<String, List<Path>> entry : ranked) {
            Group group = groups.get(entry.getKey());
            for (Path path : entry.getValue()) {
                builder.entries.add(new Entry(EntryKind.GROUP, entry.getKey(), true, path.contexts(), rank,
                        optional(path.expiresAt())));
                builder.check();
                if (group != null) {
                    builder.nodes(group.nodes(), rank, path);
                }
            }
            rank++;
        }
        return new CompiledSubject(revision, user.uuid(), primaryGroup(user, groups, paths, now), builder.entries,
                builder.prefix, builder.suffix, builder.meta);
    }

    private static int weight(Map<String, Group> groups, String name) {
        Group group = groups.get(name);
        return group == null ? 0 : group.weight();
    }

    /**
     * The explicit primary group when it exists, otherwise the heaviest group among the user's own
     * unexpired, global, positive inherit nodes that survived blocking (ties by name), otherwise
     * {@code default}.
     */
    private static String primaryGroup(User user, Map<String, Group> groups, Map<String, List<Path>> paths, long now) {
        String explicit = user.primaryGroup();
        if (explicit != null && (explicit.equals(RiftPermissions.DEFAULT_GROUP) || groups.containsKey(explicit))) {
            return explicit;
        }
        String best = null;
        for (Node node : user.nodes()) {
            if (node.contexts().isEmpty() && !node.isExpired(now) && node.payload() instanceof Node.Inherit inherit
                    && inherit.value() && paths.containsKey(inherit.group())) {
                String name = inherit.group();
                if (best == null || weight(groups, name) > weight(groups, best)
                        || (weight(groups, name) == weight(groups, best) && name.compareTo(best) <= 0)) {
                    best = name;
                }
            }
        }
        return best == null ? RiftPermissions.DEFAULT_GROUP : best;
    }
}
