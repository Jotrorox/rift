package io.github.jotrorox.rift.permissions.client;

import java.math.BigDecimal;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * A small JSON value model (RFC 8259) for the protocol's documents, so the client needs no JSON
 * library. Objects keep member order; numbers compare by value.
 */
public sealed interface Json {
    /**
     * @param text a JSON document
     * @return its value
     * @throws IllegalArgumentException when it is not valid JSON
     */
    static Json parse(String text) {
        return new JsonParser(text).document();
    }

    /** @return the compact serialisation */
    @Override
    String toString();

    /**
     * An object.
     *
     * @param members the members in order
     */
    record Obj(Map<String, Json> members) implements Json {
        public Obj {
            members = Collections.unmodifiableMap(new LinkedHashMap<>(members));
        }

        /**
         * @param name a member name
         * @return the member, or null when absent
         */
        public Json get(String name) {
            return members.get(name);
        }

        @Override
        public String toString() {
            StringBuilder out = new StringBuilder("{");
            for (Map.Entry<String, Json> member : members.entrySet()) {
                if (out.length() > 1) {
                    out.append(',');
                }
                Str.quote(member.getKey(), out);
                out.append(':').append(member.getValue());
            }
            return out.append('}').toString();
        }
    }

    /**
     * An array.
     *
     * @param items the items
     */
    record Arr(List<Json> items) implements Json {
        public Arr {
            items = List.copyOf(items);
        }

        @Override
        public String toString() {
            StringBuilder out = new StringBuilder("[");
            for (Json item : items) {
                if (out.length() > 1) {
                    out.append(',');
                }
                out.append(item);
            }
            return out.append(']').toString();
        }
    }

    /**
     * A string.
     *
     * @param value the text
     */
    record Str(String value) implements Json {
        @Override
        public String toString() {
            StringBuilder out = new StringBuilder();
            quote(value, out);
            return out.toString();
        }

        static void quote(String text, StringBuilder out) {
            out.append('"');
            for (int i = 0; i < text.length(); i++) {
                char c = text.charAt(i);
                switch (c) {
                    case '"' -> out.append("\\\"");
                    case '\\' -> out.append("\\\\");
                    case '\n' -> out.append("\\n");
                    case '\r' -> out.append("\\r");
                    case '\t' -> out.append("\\t");
                    case '\b' -> out.append("\\b");
                    case '\f' -> out.append("\\f");
                    default -> {
                        if (c < 0x20) {
                            out.append(String.format("\\u%04x", (int) c));
                        } else {
                            out.append(c);
                        }
                    }
                }
            }
            out.append('"');
        }
    }

    /**
     * A number, normalised so that equal values are equal records.
     *
     * @param value the number
     */
    record Num(BigDecimal value) implements Json {
        public Num {
            value = value.signum() == 0 ? BigDecimal.ZERO : value.stripTrailingZeros();
        }

        /**
         * @param value an integer
         * @return the number
         */
        public static Num of(long value) {
            return new Num(BigDecimal.valueOf(value));
        }

        @Override
        public String toString() {
            return value.scale() <= 0 ? value.toBigInteger().toString() : value.toPlainString();
        }
    }

    /**
     * {@code true} or {@code false}.
     *
     * @param value the value
     */
    record Bool(boolean value) implements Json {
        @Override
        public String toString() {
            return Boolean.toString(value);
        }
    }

    /** {@code null}. */
    record Null() implements Json {
        /** The only instance needed. */
        public static final Null INSTANCE = new Null();

        @Override
        public String toString() {
            return "null";
        }
    }
}
