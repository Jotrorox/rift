package io.github.jotrorox.rift.permissions.client;

import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.UUID;

/**
 * Both ends of a Minecraft 1.8 (protocol 47) login, enough to bring a player through the proxy: the
 * client sends Handshake and Login Start; the backend answers Login Success with the offline UUID,
 * as an offline-mode Paper server would. Mirrors {@code connect_player} and {@code accept_player}
 * in {@code tests/permissions.rs}.
 */
final class MinecraftLogin {
    private MinecraftLogin() {}

    /** @return the offline-mode UUID of {@code name} ({@code OfflinePlayer:<name>}) */
    static UUID offlineUuid(String name) {
        return UUID.nameUUIDFromBytes(("OfflinePlayer:" + name).getBytes(StandardCharsets.UTF_8));
    }

    /** A client logging in as {@code name} through {@code front}, left open. */
    static Socket connect(InetSocketAddress front, String name) throws IOException {
        Socket socket = new Socket(front.getAddress(), front.getPort());
        socket.setSoTimeout(10_000);
        ByteArrayOutputStream handshake = new ByteArrayOutputStream();
        writeVarInt(handshake, 47);
        writeString(handshake, "localhost");
        handshake.write(25565 >> 8);
        handshake.write(25565 & 0xff);
        writeVarInt(handshake, 2);
        ByteArrayOutputStream login = new ByteArrayOutputStream();
        writeString(login, name);
        OutputStream out = socket.getOutputStream();
        writePacket(out, 0, handshake.toByteArray());
        writePacket(out, 0, login.toByteArray());
        out.flush();
        return socket;
    }

    /**
     * Accept the proxy's connection to the backend and answer Login Success. {@code beforeLogin}
     * runs once the connection is accepted, before the backend reads anything.
     */
    static Socket accept(ServerSocket backend, Runnable beforeLogin) throws IOException {
        Socket socket = backend.accept();
        beforeLogin.run();
        socket.setSoTimeout(10_000);
        DataInputStream in = new DataInputStream(socket.getInputStream());
        byte[] handshake = readPacket(in);
        if (handshake[0] != 0 || handshake[handshake.length - 1] != 2) {
            throw new IOException("expected a login handshake");
        }
        byte[] login = readPacket(in);
        DataInputStream body = new DataInputStream(new java.io.ByteArrayInputStream(login));
        if (readVarInt(body) != 0) {
            throw new IOException("expected Login Start");
        }
        String name = readString(body);
        ByteArrayOutputStream success = new ByteArrayOutputStream();
        writeString(success, offlineUuid(name).toString());
        writeString(success, name);
        writePacket(socket.getOutputStream(), 2, success.toByteArray());
        socket.getOutputStream().flush();
        return socket;
    }

    private static void writePacket(OutputStream out, int id, byte[] data) throws IOException {
        ByteArrayOutputStream body = new ByteArrayOutputStream();
        writeVarInt(body, id);
        body.write(data);
        ByteArrayOutputStream frame = new ByteArrayOutputStream();
        writeVarInt(frame, body.size());
        body.writeTo(frame);
        out.write(frame.toByteArray());
    }

    private static byte[] readPacket(DataInputStream in) throws IOException {
        byte[] body = new byte[readVarInt(in)];
        in.readFully(body);
        return body;
    }

    private static void writeVarInt(OutputStream out, int value) throws IOException {
        while ((value & ~0x7f) != 0) {
            out.write((value & 0x7f) | 0x80);
            value >>>= 7;
        }
        out.write(value);
    }

    private static int readVarInt(InputStream in) throws IOException {
        int value = 0;
        for (int shift = 0; shift < 35; shift += 7) {
            int b = in.read();
            if (b < 0) {
                throw new IOException("connection closed");
            }
            value |= (b & 0x7f) << shift;
            if ((b & 0x80) == 0) {
                return value;
            }
        }
        throw new IOException("VarInt too long");
    }

    private static void writeString(OutputStream out, String text) throws IOException {
        byte[] bytes = text.getBytes(StandardCharsets.UTF_8);
        writeVarInt(out, bytes.length);
        out.write(bytes);
    }

    private static String readString(DataInputStream in) throws IOException {
        byte[] bytes = new byte[readVarInt(in)];
        in.readFully(bytes);
        return new String(bytes, StandardCharsets.UTF_8);
    }
}
