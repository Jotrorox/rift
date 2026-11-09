package io.github.jotrorox.rift.permissions.client;

import io.github.jotrorox.rift.permissions.CompiledSubject;
import io.github.jotrorox.rift.permissions.Node;
import io.github.jotrorox.rift.permissions.SubjectRef;
import io.github.jotrorox.rift.permissions.Uuids;
import io.github.jotrorox.rift.permissions.client.Json.Arr;
import io.github.jotrorox.rift.permissions.client.Json.Null;
import io.github.jotrorox.rift.permissions.client.Json.Num;
import io.github.jotrorox.rift.permissions.client.Json.Obj;
import io.github.jotrorox.rift.permissions.client.Json.Str;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeMap;
import java.util.UUID;

/**
 * A parsed {@link OfflinePolicy#fallbackFile fallback file}: groups and users in the typed node
 * format, compiled locally exactly as the proxy compiles. The format is the Rust client's:
 *
 * <pre>{@code
 * {"groups": [{"name": "default", "nodes": [{"kind": "permission", "key": "essentials.spawn", "value": true}]},
 *             {"name": "staff", "weight": 100, "nodes": [...]}],
 *  "users": [{"uuid": "0123456789abcdef0123456789abcdef", "nodes": [{"kind": "inherit", "group": "staff", "value": true}]}]}
 * }</pre>
 */
final class Fallback {
    private static final Set<String> MEMBERS = Set.of("groups", "users");

    private final Map<String, Compiler.Group> groups;
    private final Map<UUID, Compiler.User> users;

    private Fallback(Map<String, Compiler.Group> groups, Map<UUID, Compiler.User> users) {
        this.groups = groups;
        this.users = users;
    }

    /**
     * Parse and validate a fallback file: names are normalised and every listed user must compile.
     *
     * @param json the file's JSON
     * @return the fallback
     * @throws IllegalArgumentException for a malformed file
     */
    static Fallback parse(Json json) {
        if (!(json instanceof Obj file)) {
            throw new IllegalArgumentException("expected {\"groups\": [...], \"users\": [...]}");
        }
        for (String member : file.members().keySet()) {
            if (!MEMBERS.contains(member)) {
                throw new IllegalArgumentException("unknown member \"" + member + "\"; expected groups and users");
            }
        }
        Map<String, Compiler.Group> groups = new TreeMap<>();
        for (Json item : array(file.get("groups"), "groups")) {
            Obj group = object(item, "group");
            String name = new SubjectRef.Group(string(group.get("name"), "group name")).name();
            Json weight = group.get("weight");
            Json displayName = group.get("display_name");
            Compiler.Group parsed = new Compiler.Group(name,
                    weight == null || weight instanceof Null ? 0 : int32(weight, "weight"),
                    displayName == null || displayName instanceof Null ? null : string(displayName, "display_name"),
                    nodes(group));
            if (groups.put(name, parsed) != null) {
                throw new IllegalArgumentException("group " + name + " is listed twice");
            }
        }
        Map<UUID, Compiler.User> users = new TreeMap<>();
        for (Json item : array(file.get("users"), "users")) {
            Obj user = object(item, "user");
            UUID uuid = Uuids.parse(string(user.get("uuid"), "uuid"));
            Json primary = user.get("primary_group");
            users.put(uuid, new Compiler.User(uuid,
                    primary == null || primary instanceof Null ? null : string(primary, "primary_group"), nodes(user)));
        }
        Fallback fallback = new Fallback(Map.copyOf(groups), Map.copyOf(users));
        for (UUID uuid : users.keySet()) {
            try {
                fallback.subject(uuid, 0);
            } catch (IllegalStateException e) {
                throw new IllegalArgumentException("user " + Uuids.toHex(uuid) + ": " + e.getMessage(), e);
            }
        }
        return fallback;
    }

    /**
     * The player's compiled subject at revision 0, so any snapshot from the proxy replaces it. A
     * player the file does not list gets {@code default}.
     *
     * @param uuid the player
     * @param now Unix seconds
     * @return the compiled subject
     */
    CompiledSubject subject(UUID uuid, long now) {
        Compiler.User user = users.getOrDefault(uuid, new Compiler.User(uuid, null, List.of()));
        return Compiler.compile(user, groups, now, 0);
    }

    private static List<Json> array(Json json, String what) {
        if (json == null || json instanceof Null) {
            return List.of();
        }
        if (!(json instanceof Arr arr)) {
            throw new IllegalArgumentException(what + " must be an array");
        }
        return arr.items();
    }

    private static Obj object(Json json, String what) {
        if (!(json instanceof Obj obj)) {
            throw new IllegalArgumentException("each " + what + " must be an object");
        }
        return obj;
    }

    private static String string(Json json, String what) {
        if (!(json instanceof Str str)) {
            throw new IllegalArgumentException(what + " must be a string");
        }
        return str.value();
    }

    private static int int32(Json json, String what) {
        if (!(json instanceof Num num)) {
            throw new IllegalArgumentException(what + " must be an integer");
        }
        try {
            return num.value().intValueExact();
        } catch (ArithmeticException e) {
            throw new IllegalArgumentException(what + " must be a 32-bit integer", e);
        }
    }

    private static List<Node> nodes(Obj subject) {
        Json nodes = subject.get("nodes");
        return nodes == null || nodes instanceof Null ? List.of() : Codec.readNodes(nodes);
    }
}
