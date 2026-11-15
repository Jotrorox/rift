package io.github.jotrorox.rift.control;

import io.github.jotrorox.rift.permissions.client.Json;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.function.Function;

/**
 * Lenient readers for control replies and events: a member that is absent, null or of another type
 * reads as empty, so a newer or older Rift never breaks parsing.
 */
final class Wire {
    private Wire() {}

    static String string(Json.Obj object, String name) {
        return optionalString(object, name).orElse("");
    }

    static Optional<String> optionalString(Json.Obj object, String name) {
        return object.get(name) instanceof Json.Str text ? Optional.of(text.value()) : Optional.empty();
    }

    static boolean bool(Json.Obj object, String name) {
        return object.get(name) instanceof Json.Bool value && value.value();
    }

    static long number(Json.Obj object, String name) {
        return optionalNumber(object, name).orElse(0L);
    }

    static Optional<Long> optionalNumber(Json.Obj object, String name) {
        if (object.get(name) instanceof Json.Num number) {
            try {
                return Optional.of(number.value().longValueExact());
            } catch (ArithmeticException e) {
                // Not an integer this API can carry.
            }
        }
        return Optional.empty();
    }

    /** An identifier Rift sends as a number in status and as a decimal string in events. */
    static long id(Json.Obj object, String name) {
        if (object.get(name) instanceof Json.Str text) {
            try {
                return Long.parseLong(text.value());
            } catch (NumberFormatException e) {
                return 0;
            }
        }
        return number(object, name);
    }

    static Json.Arr array(Json.Obj object, String name) {
        return object.get(name) instanceof Json.Arr array ? array : new Json.Arr(List.of());
    }

    static <T> List<T> objects(Json.Obj object, String name, Function<Json.Obj, T> item) {
        List<T> out = new ArrayList<>();
        for (Json element : array(object, name).items()) {
            if (element instanceof Json.Obj member) {
                out.add(item.apply(member));
            }
        }
        return List.copyOf(out);
    }

    static List<String> strings(Json.Obj object, String name) {
        List<String> out = new ArrayList<>();
        for (Json element : array(object, name).items()) {
            if (element instanceof Json.Str text) {
                out.add(text.value());
            }
        }
        return List.copyOf(out);
    }

    static Map<String, String> properties(Json.Obj object, String name) {
        Map<String, String> out = new LinkedHashMap<>();
        if (object.get(name) instanceof Json.Obj members) {
            members.members().forEach((key, value) -> {
                if (value instanceof Json.Str text) {
                    out.put(key, text.value());
                }
            });
        }
        return out;
    }
}
