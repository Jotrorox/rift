package io.github.jotrorox.rift.messaging;

import java.io.ByteArrayOutputStream;
import java.io.EOFException;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.CharacterCodingException;
import java.nio.charset.CodingErrorAction;
import java.nio.charset.StandardCharsets;
import java.nio.ByteBuffer;
import java.util.Arrays;

/**
 * The framing of {@code docs/messaging-protocol.md}: a big-endian {@code u32} body length, then the
 * body (opcode first). Strings are a {@code u16} byte length and UTF-8.
 */
final class Frames {
    static final int AUTH = 1;
    static final int OK = 2;
    static final int ERROR = 3;
    static final int PUBLISH = 4;
    static final int PUBLISH_ACK = 5;
    static final int SUBSCRIBE = 6;
    static final int MESSAGE = 7;
    static final int PING = 8;
    static final int REQUEST = 9;

    static final int MAX_PAYLOAD = 1024 * 1024;
    static final int MAX_FRAME = MAX_PAYLOAD + 64 * 1024;
    static final int MAX_STRING = 4096;
    static final int MAX_SUBJECT = 1024;
    /** The server's error text for a REQUEST that got no reply in time. */
    static final String REQUEST_TIMED_OUT = "request timed out";

    private Frames() {}

    /** Builds one frame body and prefixes its length. */
    static final class Writer {
        private final ByteArrayOutputStream body = new ByteArrayOutputStream();

        Writer(int opcode) {
            body.write(opcode);
        }

        Writer u16(int value) {
            body.write(value >>> 8);
            body.write(value);
            return this;
        }

        Writer u32(long value) {
            for (int shift = 24; shift >= 0; shift -= 8) {
                body.write((int) (value >>> shift));
            }
            return this;
        }

        Writer string(String value) {
            byte[] bytes = value.getBytes(StandardCharsets.UTF_8);
            if (bytes.length > MAX_STRING) {
                throw new IllegalArgumentException("encoded string exceeds " + MAX_STRING + " bytes");
            }
            u16(bytes.length);
            body.writeBytes(bytes);
            return this;
        }

        Writer subject(String subject) {
            int length = subject.getBytes(StandardCharsets.UTF_8).length;
            if (length == 0 || length > MAX_SUBJECT) {
                throw new IllegalArgumentException("subjects are 1 to " + MAX_SUBJECT + " bytes");
            }
            return string(subject);
        }

        Writer payload(byte[] payload) {
            if (payload.length > MAX_PAYLOAD) {
                throw new IllegalArgumentException("payload exceeds " + MAX_PAYLOAD + " bytes");
            }
            body.writeBytes(payload);
            return this;
        }

        byte[] frame() {
            byte[] content = body.toByteArray();
            byte[] frame = new byte[4 + content.length];
            frame[0] = (byte) (content.length >>> 24);
            frame[1] = (byte) (content.length >>> 16);
            frame[2] = (byte) (content.length >>> 8);
            frame[3] = (byte) content.length;
            System.arraycopy(content, 0, frame, 4, content.length);
            return frame;
        }
    }

    /**
     * Read one frame body.
     *
     * @throws EOFException when the stream ends before a frame starts or inside one
     * @throws MessagingException when the length is zero or above {@code max}
     */
    static byte[] read(InputStream in, int max) throws IOException {
        byte[] prefix = in.readNBytes(4);
        if (prefix.length == 0) {
            throw new EOFException("stream finished");
        }
        if (prefix.length < 4) {
            throw new EOFException("stream finished inside a frame");
        }
        long length = ((prefix[0] & 0xffL) << 24) | ((prefix[1] & 0xff) << 16) | ((prefix[2] & 0xff) << 8)
                | (prefix[3] & 0xff);
        if (length == 0 || length > max) {
            throw MessagingException.protocol("frame length " + length + " is outside 1.." + max);
        }
        byte[] body = in.readNBytes((int) length);
        if (body.length < length) {
            throw new EOFException("stream finished inside a frame");
        }
        return body;
    }

    /** Reads the fields of one frame body. */
    static final class Reader {
        private final byte[] body;
        private int position;

        Reader(byte[] body) {
            this.body = body;
        }

        private void need(int count) throws MessagingException {
            if (body.length - position < count) {
                throw MessagingException.protocol("truncated frame");
            }
        }

        int u8() throws MessagingException {
            need(1);
            return body[position++] & 0xff;
        }

        int u16() throws MessagingException {
            need(2);
            int value = ((body[position] & 0xff) << 8) | (body[position + 1] & 0xff);
            position += 2;
            return value;
        }

        String string() throws MessagingException {
            int length = u16();
            need(length);
            try {
                String value = StandardCharsets.UTF_8.newDecoder()
                        .onMalformedInput(CodingErrorAction.REPORT)
                        .onUnmappableCharacter(CodingErrorAction.REPORT)
                        .decode(ByteBuffer.wrap(body, position, length))
                        .toString();
                position += length;
                return value;
            } catch (CharacterCodingException e) {
                throw MessagingException.protocol("invalid UTF-8 string");
            }
        }

        byte[] rest() {
            byte[] rest = Arrays.copyOfRange(body, position, body.length);
            position = body.length;
            return rest;
        }

        void end() throws MessagingException {
            if (position != body.length) {
                throw MessagingException.protocol("unexpected trailing bytes");
            }
        }
    }

    /** Decode a MESSAGE body after its opcode. */
    static Message message(Reader reader) throws MessagingException {
        String subject = reader.string();
        String reply = reader.string();
        return new Message(subject, reply.isEmpty() ? null : reply, reader.rest());
    }

    /** Decode an ERROR body after its opcode. */
    static MessagingException error(Reader reader) throws MessagingException {
        int code = reader.u16();
        String message = reader.string();
        return new MessagingException(message, code);
    }

    /** Accept an OK body, or throw the server's ERROR. */
    static void expectOk(byte[] body) throws MessagingException {
        Reader reader = new Reader(body);
        int opcode = reader.u8();
        if (opcode == ERROR) {
            throw error(reader);
        }
        if (opcode != OK) {
            throw MessagingException.protocol("expected OK, got opcode " + opcode);
        }
        reader.end();
    }
}
