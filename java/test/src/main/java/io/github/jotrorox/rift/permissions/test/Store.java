package io.github.jotrorox.rift.permissions.test;

import io.github.jotrorox.rift.permissions.Change;
import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.ErrorCode;
import io.github.jotrorox.rift.permissions.Filter;
import io.github.jotrorox.rift.permissions.Node;
import io.github.jotrorox.rift.permissions.NodeKind;
import io.github.jotrorox.rift.permissions.Patch;
import io.github.jotrorox.rift.permissions.PermissionChangeEvent;
import io.github.jotrorox.rift.permissions.PermissionChangeEvent.GroupAttributes;
import io.github.jotrorox.rift.permissions.PermissionException;
import io.github.jotrorox.rift.permissions.RiftPermissions;
import io.github.jotrorox.rift.permissions.SubjectRef;
import io.github.jotrorox.rift.permissions.client.Compiler;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.OptionalLong;
import java.util.TreeMap;
import java.util.UUID;

/**
 * The in-memory store: users, groups and tracks, and the authority's write rules applied to them.
 * Each {@link #apply} records the effects as change-event ops; a failing change leaves the store
 * untouched.
 */
final class Store {
    private Map<UUID, Compiler.User> users = new LinkedHashMap<>();
    private Map<String, Compiler.Group> groups = new TreeMap<>();
    private Map<String, List<String>> tracks = new TreeMap<>();

    Store() {
        groups.put(RiftPermissions.DEFAULT_GROUP, new Compiler.Group(RiftPermissions.DEFAULT_GROUP, 0, null, List.of()));
    }

    Map<UUID, Compiler.User> users() {
        return users;
    }

    Map<String, Compiler.Group> groups() {
        return groups;
    }

    Compiler.User user(UUID uuid) {
        return users.getOrDefault(uuid, new Compiler.User(uuid, null, List.of()));
    }

    void putUser(Compiler.User user) {
        users.put(user.uuid(), user);
    }

    void putGroup(Compiler.Group group) {
        groups.put(group.name(), group);
    }

    void putTrack(String name, List<String> track) {
        tracks.put(name, List.copyOf(track));
    }

    /** Apply a change atomically and return its effects; throws {@link PermissionException}. */
    List<PermissionChangeEvent.Op> apply(Change change, long now) {
        Map<UUID, Compiler.User> savedUsers = new LinkedHashMap<>(users);
        Map<String, Compiler.Group> savedGroups = new TreeMap<>(groups);
        Map<String, List<String>> savedTracks = new TreeMap<>(tracks);
        List<PermissionChangeEvent.Op> ops = new ArrayList<>();
        try {
            if (change instanceof Change.Batch batch) {
                batch.changes().forEach(each -> one(each, now, ops));
            } else {
                one(change, now, ops);
            }
            return ops;
        } catch (RuntimeException e) {
            users = savedUsers;
            groups = savedGroups;
            tracks = savedTracks;
            throw e;
        }
    }

    private static PermissionException error(ErrorCode code, String message) {
        return new PermissionException(code, message);
    }

    private Compiler.Group existing(String name) {
        Compiler.Group group = groups.get(name);
        if (group == null) {
            throw error(ErrorCode.NOT_FOUND, "unknown group \"" + name + "\"");
        }
        return group;
    }

    private List<Node> nodes(SubjectRef subject) {
        return switch (subject) {
            case SubjectRef.User user -> user(user.uuid()).nodes();
            case SubjectRef.Group group -> existing(group.name()).nodes();
        };
    }

    private void setNodes(SubjectRef subject, List<Node> nodes) {
        switch (subject) {
            case SubjectRef.User user -> {
                Compiler.User old = user(user.uuid());
                users.put(user.uuid(), new Compiler.User(old.uuid(), old.primaryGroup(), nodes));
            }
            case SubjectRef.Group group -> {
                Compiler.Group old = existing(group.name());
                int weight = old.weight();
                String displayName = old.displayName();
                for (Node node : nodes) {
                    if (node.contexts().isEmpty() && node.payload() instanceof Node.Weight w) {
                        weight = w.weight();
                    }
                    if (node.contexts().isEmpty() && node.payload() instanceof Node.DisplayName d) {
                        displayName = d.name();
                    }
                }
                groups.put(old.name(), new Compiler.Group(old.name(), weight, displayName, nodes));
            }
        }
    }

    private static boolean sameSlot(Node a, Node b) {
        return a.kind() == b.kind() && a.payload().storageKey().equals(b.payload().storageKey())
                && a.contexts().equals(b.contexts());
    }

    /** Insert or replace in its slot; returns the replaced node. */
    private Node set(SubjectRef subject, Node node, List<PermissionChangeEvent.Op> ops) {
        List<Node> nodes = new ArrayList<>(nodes(subject));
        Node before = null;
        boolean replaced = false;
        for (int i = 0; i < nodes.size(); i++) {
            if (sameSlot(nodes.get(i), node)) {
                before = nodes.set(i, node);
                replaced = true;
            }
        }
        if (!replaced) {
            nodes.add(node);
        }
        if (!node.equals(before)) {
            setNodes(subject, nodes);
            ops.add(new PermissionChangeEvent.NodeChange(subject, before, node));
        }
        return before;
    }

    private void unset(SubjectRef subject, NodeKind kind, String key, ContextSet contexts,
            List<PermissionChangeEvent.Op> ops) {
        List<Node> nodes = new ArrayList<>(nodes(subject));
        for (int i = 0; i < nodes.size(); i++) {
            Node node = nodes.get(i);
            if (node.kind() == kind && node.payload().storageKey().equals(key) && node.contexts().equals(contexts)) {
                nodes.remove(i);
                setNodes(subject, nodes);
                ops.add(new PermissionChangeEvent.NodeChange(subject, node, null));
                return;
            }
        }
    }

    private void one(Change change, long now, List<PermissionChangeEvent.Op> ops) {
        switch (change) {
            case Change.SetNode c -> set(c.subject(), c.node(), ops);
            case Change.UnsetNode c -> unset(c.subject(), c.kind(), c.key(), c.contexts(), ops);
            case Change.ClearSubject c -> {
                List<Node> kept = new ArrayList<>();
                for (Node node : nodes(c.subject())) {
                    if (c.contexts() == null || node.contexts().equals(c.contexts())) {
                        ops.add(new PermissionChangeEvent.NodeChange(c.subject(), node, null));
                    } else {
                        kept.add(node);
                    }
                }
                setNodes(c.subject(), kept);
            }
            case Change.CreateGroup c -> {
                if (groups.containsKey(c.name())) {
                    throw error(ErrorCode.CONFLICT, "group \"" + c.name() + "\" exists");
                }
                groups.put(c.name(), new Compiler.Group(c.name(), c.weight(), c.displayName(), List.of()));
                ops.add(new PermissionChangeEvent.GroupChange(c.name(), null,
                        new GroupAttributes(c.weight(), c.displayName())));
            }
            case Change.DeleteGroup c -> {
                Compiler.Group old = existing(c.name());
                groups.remove(c.name());
                ops.add(new PermissionChangeEvent.GroupChange(c.name(), attributes(old), null));
            }
            case Change.SetWeight c -> {
                Compiler.Group old = existing(c.group());
                if (old.weight() != c.weight()) {
                    groups.put(old.name(), new Compiler.Group(old.name(), c.weight(), old.displayName(), old.nodes()));
                    ops.add(new PermissionChangeEvent.GroupChange(old.name(), attributes(old),
                            new GroupAttributes(c.weight(), old.displayName())));
                }
            }
            case Change.SetDisplayName c -> {
                Compiler.Group old = existing(c.group());
                if (!Objects.equals(old.displayName(), c.displayName())) {
                    groups.put(old.name(), new Compiler.Group(old.name(), old.weight(), c.displayName(), old.nodes()));
                    ops.add(new PermissionChangeEvent.GroupChange(old.name(), attributes(old),
                            new GroupAttributes(old.weight(), c.displayName())));
                }
            }
            case Change.SetPrimaryGroup c -> {
                Compiler.User old = user(c.user());
                if (!Objects.equals(old.primaryGroup(), c.group())) {
                    users.put(c.user(), new Compiler.User(c.user(), c.group(), old.nodes()));
                    ops.add(new PermissionChangeEvent.PrimaryGroupChange(c.user(), old.primaryGroup(), c.group()));
                }
            }
            case Change.SetTrack c -> {
                List<String> before = tracks.put(c.name(), c.groups());
                if (!c.groups().equals(before)) {
                    ops.add(new PermissionChangeEvent.TrackChange(c.name(), before, c.groups()));
                }
            }
            case Change.DeleteTrack c -> {
                List<String> before = tracks.remove(c.name());
                if (before == null) {
                    throw error(ErrorCode.NOT_FOUND, "unknown track \"" + c.name() + "\"");
                }
                ops.add(new PermissionChangeEvent.TrackChange(c.name(), before, null));
            }
            case Change.Promote c -> move(c.user(), c.track(), c.contexts(), now, true, ops);
            case Change.Demote c -> move(c.user(), c.track(), c.contexts(), now, false, ops);
            case Change.Bulk c -> bulk(c.filter(), c.patch(), ops);
            case Change.Batch c -> throw error(ErrorCode.INVALID_REQUEST, "batches cannot be nested");
        }
    }

    private static GroupAttributes attributes(Compiler.Group group) {
        return new GroupAttributes(group.weight(), group.displayName());
    }

    /** Promote or demote along a track, as the Rust {@code Track::promote}/{@code demote}. */
    private void move(UUID uuid, String trackName, ContextSet contexts, long now, boolean up,
            List<PermissionChangeEvent.Op> ops) {
        List<String> track = tracks.get(trackName);
        if (track == null) {
            throw error(ErrorCode.NOT_FOUND, "unknown track \"" + trackName + "\"");
        }
        List<Node> nodes = user(uuid).nodes();
        Integer position = null;
        for (Node node : nodes) {
            if (node.payload() instanceof Node.Inherit inherit && inherit.value() && node.contexts().equals(contexts)
                    && !node.isExpired(now) && track.contains(inherit.group())) {
                int index = track.indexOf(inherit.group());
                if (position != null && position != index) {
                    throw error(ErrorCode.CONFLICT, "the user is on more than one of the track's groups");
                }
                position = index;
            }
        }
        if (position == null && track.contains(RiftPermissions.DEFAULT_GROUP)) {
            position = track.indexOf(RiftPermissions.DEFAULT_GROUP);
        }
        String remove;
        String add;
        if (up) {
            if (track.isEmpty()) {
                throw error(ErrorCode.CONFLICT, "the track is empty");
            }
            if (position == null) {
                remove = null;
                add = track.getFirst();
            } else if (position + 1 >= track.size()) {
                throw error(ErrorCode.CONFLICT, "the user is already on the track's last group");
            } else {
                remove = explicit(nodes, track.get(position), contexts);
                add = track.get(position + 1);
            }
        } else {
            if (position == null) {
                throw error(ErrorCode.CONFLICT, "the user is not on this track");
            }
            remove = explicit(nodes, track.get(position), contexts);
            if (remove == null) {
                throw error(ErrorCode.CONFLICT, "the user is already on the track's first group");
            }
            add = position == 0 || track.get(position - 1).equals(RiftPermissions.DEFAULT_GROUP)
                    ? null : track.get(position - 1);
        }
        SubjectRef subject = SubjectRef.user(uuid);
        if (remove != null) {
            unset(subject, NodeKind.INHERIT, remove, contexts, ops);
        }
        if (add != null) {
            set(subject, Node.inherit(add, true).contexts(contexts), ops);
        }
    }

    private static String explicit(List<Node> nodes, String group, ContextSet contexts) {
        for (Node node : nodes) {
            if (node.contexts().equals(contexts) && node.payload() instanceof Node.Inherit inherit && inherit.value()
                    && inherit.group().equals(group)) {
                return group;
            }
        }
        return null;
    }

    private void bulk(Filter filter, Patch patch, List<PermissionChangeEvent.Op> ops) {
        List<SubjectRef> subjects = new ArrayList<>();
        users.keySet().forEach(uuid -> subjects.add(SubjectRef.user(uuid)));
        groups.keySet().forEach(name -> subjects.add(SubjectRef.group(name)));
        for (SubjectRef subject : subjects) {
            for (Node node : nodes(subject)) {
                if (!matches(filter, subject, node)) {
                    continue;
                }
                Node patched = apply(patch, node);
                if (patched == null) {
                    unset(subject, node.kind(), node.payload().storageKey(), node.contexts(), ops);
                } else if (!patched.equals(node)) {
                    if (!patched.contexts().equals(node.contexts())) {
                        unset(subject, node.kind(), node.payload().storageKey(), node.contexts(), ops);
                    }
                    set(subject, patched, ops);
                }
            }
        }
    }

    private static Node apply(Patch patch, Node node) {
        if (patch.delete()) {
            return null;
        }
        Node patched = node;
        if (patch.value() != null) {
            patched = switch (node.payload()) {
                case Node.Permission p -> new Node(new Node.Permission(p.key(), patch.value()), node.contexts(),
                        node.expiresAt(), node.source());
                case Node.Inherit p -> new Node(new Node.Inherit(p.group(), patch.value()), node.contexts(),
                        node.expiresAt(), node.source());
                default -> patched;
            };
        }
        if (patch.contexts() != null) {
            patched = patched.contexts(patch.contexts());
        }
        if (patch.permanent()) {
            patched = new Node(patched.payload(), patched.contexts(), OptionalLong.empty(), patched.source());
        } else if (patch.expiresAt() != null) {
            patched = patched.expiresAt(patch.expiresAt());
        }
        if (patch.source() != null) {
            patched = patched.source(patch.source());
        }
        return patched;
    }

    private static boolean matches(Filter filter, SubjectRef subject, Node node) {
        boolean scope = switch (filter.subjects()) {
            case ANY -> true;
            case USERS -> subject instanceof SubjectRef.User;
            case GROUPS -> subject instanceof SubjectRef.Group;
        };
        return scope
                && (filter.subject() == null || filter.subject().equals(subject))
                && (filter.kind() == null || filter.kind() == node.kind())
                && (filter.keyLike() == null || like(filter.keyLike(), node.payload().storageKey()))
                && (filter.value() == null || filter.value().equals(node.payload().storageValue()))
                && (filter.contexts() == null || filter.contexts().equals(node.contexts()))
                && (filter.source() == null || filter.source().equals(node.source()));
    }

    /**
     * SQL {@code LIKE} as SQLite evaluates it with {@code ESCAPE '\'}: {@code %} any run, {@code _}
     * one character, ASCII letters case-insensitive.
     */
    static boolean like(String pattern, String text) {
        List<Integer> tokens = new ArrayList<>();
        final int any = -1;
        final int one = -2;
        for (int i = 0; i < pattern.length(); ) {
            int c = pattern.codePointAt(i);
            i += Character.charCount(c);
            if (c == '%') {
                tokens.add(any);
            } else if (c == '_') {
                tokens.add(one);
            } else if (c == '\\') {
                if (i < pattern.length()) {
                    int escaped = pattern.codePointAt(i);
                    i += Character.charCount(escaped);
                    tokens.add(escaped);
                } else {
                    tokens.add((int) '\\');
                }
            } else {
                tokens.add(c);
            }
        }
        boolean[] matched = new boolean[tokens.size() + 1];
        matched[0] = true;
        for (int j = 0; j < tokens.size(); j++) {
            matched[j + 1] = matched[j] && tokens.get(j) == any;
        }
        for (int i = 0; i < text.length(); ) {
            int c = text.codePointAt(i);
            i += Character.charCount(c);
            boolean[] next = new boolean[tokens.size() + 1];
            for (int j = 0; j < tokens.size(); j++) {
                int token = tokens.get(j);
                next[j + 1] = token == any ? next[j] || matched[j + 1]
                        : token == one ? matched[j]
                        : matched[j] && lower(token) == lower(c);
            }
            matched = next;
        }
        return matched[tokens.size()];
    }

    private static int lower(int c) {
        return c >= 'A' && c <= 'Z' ? c + ('a' - 'A') : c;
    }
}
