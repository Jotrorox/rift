package io.github.jotrorox.rift.messaging;

import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.net.ConnectException;
import org.junit.jupiter.api.Test;

class SessionTest {
    @Test
    void retriesOnlyTheServerHelloRace() {
        assertTrue(Session.isHandshakeRace(new ConnectException("Handshake error: ")));
        assertTrue(Session.isHandshakeRace(new ConnectException(
                "Handshake error: tech.kwik.agent15.alert.UnexpectedMessageAlert: unexpected server hello message")));

        assertFalse(Session.isHandshakeRace(new ConnectException(
                "Handshake error: tech.kwik.agent15.alert.BadCertificateAlert: signature check failed")));
        assertFalse(Session.isHandshakeRace(
                new ConnectException("Handshake error: Server closed connection: CONNECTION_REFUSED")));
        assertFalse(Session.isHandshakeRace(new ConnectException("Connection timed out after 10000 ms")));
        assertFalse(Session.isHandshakeRace(new ConnectException()));
    }
}
