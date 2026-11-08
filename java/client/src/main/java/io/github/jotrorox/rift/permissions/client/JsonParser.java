package io.github.jotrorox.rift.permissions.client;

import io.github.jotrorox.rift.permissions.client.Json.Arr;
import io.github.jotrorox.rift.permissions.client.Json.Bool;
import io.github.jotrorox.rift.permissions.client.Json.Null;
import io.github.jotrorox.rift.permissions.client.Json.Num;
import io.github.jotrorox.rift.permissions.client.Json.Obj;
import io.github.jotrorox.rift.permissions.client.Json.Str;
import java.math.BigDecimal;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/** A strict recursive-descent parser for {@link Json#parse}. */
final class JsonParser {
    /** Deepest nesting accepted. */
    private static final int MAX_DEPTH = 128;
    /** Largest decimal exponent accepted, which bounds the cost of printing a number. */
    private static final int MAX_SCALE = 1000;

    private final String text;
    private int position;

    JsonParser(String text) {
        this.text = text;
    }

    Json document() {
        Json value = value(0);
        skipWhitespace();
        if (position != text.length()) {
            throw error("trailing characters");
        }
        return value;
    }

    private IllegalArgumentException error(String reason) {
        return new IllegalArgumentException("invalid JSON at offset " + position + ": " + reason);
    }

    private void skipWhitespace() {
        while (position < text.length()) {
            char c = text.charAt(position);
            if (c != ' ' && c != '\t' && c != '\n' && c != '\r') {
                return;
            }
            position++;
        }
    }

    private Json value(int depth) {
        if (depth > MAX_DEPTH) {
            throw error("nested too deeply");
        }
        skipWhitespace();
        if (position >= text.length()) {
            throw error("unexpected end");
        }
        char c = text.charAt(position);
        return switch (c) {
            case '{' -> object(depth);
            case '[' -> array(depth);
            case '"' -> new Str(string());
            case 't' -> literal("true", new Bool(true));
            case 'f' -> literal("false", new Bool(false));
            case 'n' -> literal("null", Null.INSTANCE);
            default -> {
                if (c == '-' || (c >= '0' && c <= '9')) {
                    yield number();
                }
                throw error("unexpected character '" + c + "'");
            }
        };
    }

    private Json literal(String word, Json value) {
        if (!text.startsWith(word, position)) {
            throw error("expected " + word);
        }
        position += word.length();
        return value;
    }

    private Json object(int depth) {
        position++;
        Map<String, Json> members = new LinkedHashMap<>();
        skipWhitespace();
        if (peek() == '}') {
            position++;
            return new Obj(members);
        }
        while (true) {
            skipWhitespace();
            if (peek() != '"') {
                throw error("expected a member name");
            }
            String name = string();
            skipWhitespace();
            expect(':');
            members.put(name, value(depth + 1));
            skipWhitespace();
            char c = next();
            if (c == '}') {
                return new Obj(members);
            }
            if (c != ',') {
                throw error("expected ',' or '}'");
            }
        }
    }

    private Json array(int depth) {
        position++;
        List<Json> items = new ArrayList<>();
        skipWhitespace();
        if (peek() == ']') {
            position++;
            return new Arr(items);
        }
        while (true) {
            items.add(value(depth + 1));
            skipWhitespace();
            char c = next();
            if (c == ']') {
                return new Arr(items);
            }
            if (c != ',') {
                throw error("expected ',' or ']'");
            }
        }
    }

    private String string() {
        expect('"');
        StringBuilder out = new StringBuilder();
        while (true) {
            char c = next();
            if (c == '"') {
                return out.toString();
            }
            if (c < 0x20) {
                throw error("control character in string");
            }
            if (c != '\\') {
                out.append(c);
                continue;
            }
            char escape = next();
            switch (escape) {
                case '"', '\\', '/' -> out.append(escape);
                case 'b' -> out.append('\b');
                case 'f' -> out.append('\f');
                case 'n' -> out.append('\n');
                case 'r' -> out.append('\r');
                case 't' -> out.append('\t');
                case 'u' -> out.append(unicode());
                default -> throw error("invalid escape");
            }
        }
    }

    private char unicode() {
        if (position + 4 > text.length()) {
            throw error("truncated \\u escape");
        }
        int value = 0;
        for (int i = 0; i < 4; i++) {
            int digit = Character.digit(text.charAt(position + i), 16);
            if (digit < 0) {
                throw error("invalid \\u escape");
            }
            value = value << 4 | digit;
        }
        position += 4;
        return (char) value;
    }

    private Json number() {
        int start = position;
        if (peek() == '-') {
            position++;
        }
        if (peek() == '0') {
            position++;
        } else if (digits() == 0) {
            throw error("expected a digit");
        }
        if (peek() == '.') {
            position++;
            if (digits() == 0) {
                throw error("expected a fraction digit");
            }
        }
        if (peek() == 'e' || peek() == 'E') {
            position++;
            if (peek() == '+' || peek() == '-') {
                position++;
            }
            if (digits() == 0) {
                throw error("expected an exponent digit");
            }
        }
        return new Num(new BigDecimal(text.substring(start, position)));
    }

    private int digits() {
        int start = position;
        while (position < text.length() && text.charAt(position) >= '0' && text.charAt(position) <= '9') {
            position++;
        }
        return position - start;
    }

    private char peek() {
        return position < text.length() ? text.charAt(position) : '\0';
    }

    private char next() {
        if (position >= text.length()) {
            throw error("unexpected end");
        }
        return text.charAt(position++);
    }

    private void expect(char c) {
        if (next() != c) {
            position--;
            throw error("expected '" + c + "'");
        }
    }
}
