package io.github.jotrorox.rift.permissions;

import java.nio.charset.StandardCharsets;

/** Validation shared by the value classes; the rules are those of the Rust crate. */
final class Names {
    static final int MAX_NODE = 256;
    static final int MAX_NAME = 64;
    static final int MAX_META_KEY = 64;
    static final int MAX_TEXT = 256;
    static final int MAX_META_VALUE = 1024;
    static final int MAX_LABEL = 96;

    private Names() {}

    /** Lowercase ASCII letters only, as Rust's {@code to_ascii_lowercase} does. */
    static String asciiLower(String text) {
        for (int i = 0; i < text.length(); i++) {
            char c = text.charAt(i);
            if (c >= 'A' && c <= 'Z') {
                char[] chars = text.toCharArray();
                for (int j = i; j < chars.length; j++) {
                    if (chars[j] >= 'A' && chars[j] <= 'Z') {
                        chars[j] = (char) (chars[j] + ('a' - 'A'));
                    }
                }
                return new String(chars);
            }
        }
        return text;
    }

    static int utf8Length(String text) {
        return text.getBytes(StandardCharsets.UTF_8).length;
    }

    static boolean hasControl(String text) {
        return text.codePoints().anyMatch(Character::isISOControl);
    }

    private static boolean nodeChar(char c) {
        return (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '_' || c == ':' || c == '-';
    }

    /** Whether {@code node} is a normalised permission pattern. */
    static boolean validNode(String node) {
        if (node.equals("*")) {
            return true;
        }
        if (node.length() > MAX_NODE) {
            return false;
        }
        String body = node.endsWith(".*") ? node.substring(0, node.length() - 2) : node;
        int start = 0;
        while (true) {
            int end = body.indexOf('.', start);
            int stop = end < 0 ? body.length() : end;
            if (stop == start) {
                return false;
            }
            for (int i = start; i < stop; i++) {
                if (!nodeChar(body.charAt(i))) {
                    return false;
                }
            }
            if (end < 0) {
                return true;
            }
            start = end + 1;
        }
    }

    static String node(String node) {
        String lower = asciiLower(node);
        if (!validNode(lower)) {
            throw invalid("permission node", node,
                    "use dot-separated segments of [a-z0-9_:-], at most 256 bytes, with * only as a whole trailing segment");
        }
        return lower;
    }

    /** Group and track names: 1 to 64 bytes of {@code [a-z0-9_.-]}, lowercased. */
    static String name(String field, String name) {
        String lower = asciiLower(name);
        boolean valid = !lower.isEmpty() && lower.length() <= MAX_NAME
                && lower.chars().allMatch(c -> (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9')
                        || c == '_' || c == '.' || c == '-');
        if (!valid) {
            throw invalid(field, name, "use 1 to 64 bytes of [a-z0-9_.-]");
        }
        return lower;
    }

    static String group(String name) {
        return name("group name", name);
    }

    static String track(String name) {
        return name("track name", name);
    }

    /** Meta and context keys: 1 to 64 bytes of {@code [a-z0-9_.:-]}, lowercased. */
    static String key(String field, String key) {
        String lower = asciiLower(key);
        boolean valid = !lower.isEmpty() && lower.length() <= MAX_META_KEY
                && lower.chars().allMatch(c -> nodeChar((char) c) || c == '.');
        if (!valid) {
            throw invalid(field, key, "use 1 to 64 bytes of [a-z0-9_.:-]");
        }
        return lower;
    }

    static String text(String field, String text, int max) {
        if (utf8Length(text) > max || hasControl(text)) {
            throw invalid(field, text, "too long or contains control characters");
        }
        return text;
    }

    /** Names after a {@code prefix:} in actor and source strings. */
    static boolean validLabel(String name) {
        return !name.isEmpty() && name.length() <= MAX_LABEL
                && name.chars().allMatch(c -> (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z')
                        || (c >= '0' && c <= '9') || c == '_' || c == '.' || c == '-' || c == ':');
    }

    static String source(String source) {
        boolean valid = switch (source) {
            case Node.SOURCE_STORE, Node.SOURCE_CONFIG, Node.SOURCE_IMPORT -> true;
            default -> source.startsWith("plugin:") && validLabel(source.substring("plugin:".length()));
        };
        if (!valid) {
            throw invalid("node source", source, "expected store, config, import or plugin:<name>");
        }
        return source;
    }

    static IllegalArgumentException invalid(String field, String value, String reason) {
        String quoted = value.length() > MAX_LABEL ? value.substring(0, MAX_LABEL) : value;
        return new IllegalArgumentException("invalid " + field + " \"" + quoted + "\": " + reason);
    }

    /** Order by code point, which is the byte order of UTF-8 that Rust sorts by. */
    static int compare(String a, String b) {
        int i = 0;
        int j = 0;
        while (i < a.length() && j < b.length()) {
            int x = a.codePointAt(i);
            int y = b.codePointAt(j);
            if (x != y) {
                return Integer.compare(x, y);
            }
            i += Character.charCount(x);
            j += Character.charCount(y);
        }
        return Integer.compare(a.length() - i, b.length() - j);
    }
}
