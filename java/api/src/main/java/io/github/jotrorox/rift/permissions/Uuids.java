package io.github.jotrorox.rift.permissions;

import java.util.UUID;

/** The protocol's UUID form: 32 lowercase hex digits without dashes. */
public final class Uuids {
    private static final char[] HEX = "0123456789abcdef".toCharArray();

    private Uuids() {}

    /**
     * @param uuid a UUID
     * @return its 32-digit lowercase hex form
     */
    public static String toHex(UUID uuid) {
        char[] out = new char[32];
        write(uuid.getMostSignificantBits(), out, 0);
        write(uuid.getLeastSignificantBits(), out, 16);
        return new String(out);
    }

    private static void write(long bits, char[] out, int offset) {
        for (int i = 15; i >= 0; i--) {
            out[offset + i] = HEX[(int) (bits & 0xf)];
            bits >>>= 4;
        }
    }

    /**
     * @param text 32 hex digits, or the dashed 8-4-4-4-12 form, in either case
     * @return the UUID
     * @throws IllegalArgumentException for anything else
     */
    public static UUID parse(String text) {
        String digits;
        if (text.length() == 32) {
            digits = text;
        } else if (text.length() == 36 && text.charAt(8) == '-' && text.charAt(13) == '-'
                && text.charAt(18) == '-' && text.charAt(23) == '-') {
            digits = text.replace("-", "");
        } else {
            digits = "";
        }
        if (digits.length() != 32) {
            throw Names.invalid("uuid", text, "expected 32 hex digits or the dashed 8-4-4-4-12 form");
        }
        return new UUID(read(digits, 0, text), read(digits, 16, text));
    }

    private static long read(String digits, int offset, String text) {
        long bits = 0;
        for (int i = offset; i < offset + 16; i++) {
            int nibble = Character.digit(digits.charAt(i), 16);
            if (nibble < 0 || digits.charAt(i) > 'f') {
                throw Names.invalid("uuid", text, "expected 32 hex digits or the dashed 8-4-4-4-12 form");
            }
            bits = bits << 4 | nibble;
        }
        return bits;
    }
}
