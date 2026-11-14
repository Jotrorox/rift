package io.github.jotrorox.rift.network.client;

import io.github.jotrorox.rift.network.NetworkPlayer;
import io.github.jotrorox.rift.network.ServerInfo;
import io.github.jotrorox.rift.network.ServerState;
import io.github.jotrorox.rift.permissions.client.Json;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.OptionalInt;
import java.util.UUID;
import java.util.function.Function;

/**
 * The JSON of {@code rift.network} replies. Unknown members are ignored, so the proxy can add
 * some; a missing or mistyped one is an {@link IllegalArgumentException}.
 */
final class Wire {
    private Wire() {}

    static Json.Obj object(Json json) {
        if (json instanceof Json.Obj object) {
            return object;
        }
        throw new IllegalArgumentException("expected an object");
    }

    private static <T> List<T> list(Json json, Function<Json, T> item) {
        if (!(json instanceof Json.Arr array)) {
            throw new IllegalArgumentException("expected an array");
        }
        List<T> out = new ArrayList<>(array.items().size());
        for (Json element : array.items()) {
            out.add(item.apply(element));
        }
        return List.copyOf(out);
    }

    private static String string(Json.Obj object, String name) {
        if (object.get(name) instanceof Json.Str text) {
            return text.value();
        }
        throw new IllegalArgumentException(name + " must be a string");
    }

    private static Optional<String> optionalString(Json.Obj object, String name) {
        Json value = object.get(name);
        if (value == null || value instanceof Json.Null) {
            return Optional.empty();
        }
        return Optional.of(string(object, name));
    }

    private static int count(Json.Obj object, String name) {
        if (object.get(name) instanceof Json.Num number) {
            try {
                int value = number.value().intValueExact();
                if (value >= 0) {
                    return value;
                }
            } catch (ArithmeticException e) {
                // Reported below.
            }
        }
        throw new IllegalArgumentException(name + " must be a non-negative integer");
    }

    static boolean bool(Json json) {
        if (json instanceof Json.Bool value) {
            return value.value();
        }
        throw new IllegalArgumentException("expected a boolean");
    }

    static List<ServerInfo> servers(Json json) {
        return list(json, Wire::server);
    }

    private static ServerInfo server(Json json) {
        Json.Obj object = object(json);
        int limit = count(object, "max_players");
        ServerState state = switch (string(object, "state")) {
            case "running" -> ServerState.RUNNING;
            case "draining" -> ServerState.DRAINING;
            default -> ServerState.UNKNOWN;
        };
        return new ServerInfo(
                string(object, "name"),
                optionalString(object, "group"),
                state,
                count(object, "players"),
                limit == 0 ? OptionalInt.empty() : OptionalInt.of(limit));
    }

    static List<NetworkPlayer> players(Json json) {
        return list(json, Wire::player);
    }

    static Optional<NetworkPlayer> optionalPlayer(Json json) {
        return json == null || json instanceof Json.Null ? Optional.empty() : Optional.of(player(json));
    }

    private static NetworkPlayer player(Json json) {
        Json.Obj object = object(json);
        return new NetworkPlayer(
                uuid(string(object, "uuid")),
                string(object, "name"),
                string(object, "server"),
                optionalString(object, "visible_to"));
    }

    /** The protocol's UUID form: 32 lowercase hex digits without dashes. */
    static String hex(UUID uuid) {
        return uuid.toString().replace("-", "");
    }

    static UUID uuid(String hex) {
        if (hex.length() != 32) {
            throw new IllegalArgumentException("uuid must be 32 hex digits");
        }
        try {
            return new UUID(Long.parseUnsignedLong(hex, 0, 16, 16), Long.parseUnsignedLong(hex, 16, 32, 16));
        } catch (NumberFormatException e) {
            throw new IllegalArgumentException("uuid must be 32 hex digits", e);
        }
    }
}
