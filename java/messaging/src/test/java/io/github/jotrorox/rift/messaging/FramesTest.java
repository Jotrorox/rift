package io.github.jotrorox.rift.messaging;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.io.ByteArrayInputStream;
import java.io.EOFException;
import java.nio.charset.StandardCharsets;
import org.junit.jupiter.api.Test;

/** The framing of docs/messaging-protocol.md, byte for byte. */
class FramesTest {
    @Test
    void publishFrameLayout() throws Exception {
        byte[] frame = new Frames.Writer(Frames.PUBLISH_ACK).subject("a.b").string("").payload(new byte[] {9}).frame();
        // u32 length, opcode 5, u16 3 "a.b", u16 0, payload 0x09
        assertArrayEquals(new byte[] {0, 0, 0, 9, 5, 0, 3, 'a', '.', 'b', 0, 0, 9}, frame);
        byte[] body = Frames.read(new ByteArrayInputStream(frame), Frames.MAX_FRAME);
        Frames.Reader reader = new Frames.Reader(body);
        assertEquals(Frames.PUBLISH_ACK, reader.u8());
        Message message = Frames.message(reader);
        assertEquals("a.b", message.subject());
        assertNull(message.reply(), "an empty reply subject is absent");
        assertArrayEquals(new byte[] {9}, message.payload());
    }

    @Test
    void requestCarriesTheTimeout() throws Exception {
        byte[] frame = new Frames.Writer(Frames.REQUEST).u32(2000).subject("x").payload(new byte[0]).frame();
        assertArrayEquals(new byte[] {0, 0, 0, 8, 9, 0, 0, 7, (byte) 0xd0, 0, 1, 'x'}, frame);
    }

    @Test
    void rejectsBadLengthsAndTruncation() {
        assertThrows(MessagingException.class,
                () -> Frames.read(new ByteArrayInputStream(new byte[] {0, 0, 0, 0}), Frames.MAX_FRAME));
        assertThrows(MessagingException.class,
                () -> Frames.read(new ByteArrayInputStream(new byte[] {0, 0x11, 0, 1}), Frames.MAX_FRAME));
        assertThrows(EOFException.class, () -> Frames.read(new ByteArrayInputStream(new byte[0]), 10));
        assertThrows(EOFException.class,
                () -> Frames.read(new ByteArrayInputStream(new byte[] {0, 0, 0, 5, 1}), 10));
        assertThrows(MessagingException.class, () -> new Frames.Reader(new byte[] {0, 5, 'a'}).string());
        assertThrows(MessagingException.class,
                () -> new Frames.Reader(new byte[] {0, 2, (byte) 0xc3, 0x28}).string(), "invalid UTF-8");
        assertThrows(IllegalArgumentException.class, () -> new Frames.Writer(Frames.PUBLISH).subject(""));
        assertThrows(IllegalArgumentException.class,
                () -> new Frames.Writer(Frames.PUBLISH).payload(new byte[Frames.MAX_PAYLOAD + 1]));
    }

    @Test
    void errorFramesCarryTheCode() {
        byte[] message = "publication not permitted".getBytes(StandardCharsets.UTF_8);
        byte[] body = new byte[5 + message.length];
        body[0] = Frames.ERROR;
        body[2] = 1;
        body[4] = (byte) message.length;
        System.arraycopy(message, 0, body, 5, message.length);
        MessagingException error = assertThrows(MessagingException.class, () -> Frames.expectOk(body));
        assertEquals(MessagingException.PERMISSION, error.code());
        assertEquals("publication not permitted", error.getMessage());
        assertThrows(MessagingException.class, () -> Frames.expectOk(new byte[] {Frames.OK, 0}), "trailing bytes");
    }
}
