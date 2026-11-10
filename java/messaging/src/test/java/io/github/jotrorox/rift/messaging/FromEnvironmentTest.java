package io.github.jotrorox.rift.messaging;

import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.Map;
import org.junit.jupiter.api.Test;

class FromEnvironmentTest {
    @Test
    void missingVariablesAreNamed() {
        IllegalStateException error = assertThrows(IllegalStateException.class,
                () -> MessagingClient.builder().fromEnvironment(Map.<String, String>of()::get));
        assertTrue(error.getMessage().contains("RIFT_BUS_ADDRESS"), error.getMessage());
        IllegalStateException token = assertThrows(IllegalStateException.class,
                () -> MessagingClient.builder().fromEnvironment(Map.of(
                        "RIFT_BUS_ADDRESS", "10.0.0.2:7443",
                        "RIFT_BUS_CERT", "/srv/lobby-3/rift-bus.pem")::get));
        assertTrue(token.getMessage().contains("RIFT_BUS_TOKEN"), token.getMessage());
        IllegalStateException address = assertThrows(IllegalStateException.class,
                () -> MessagingClient.builder().fromEnvironment(Map.of("RIFT_BUS_ADDRESS", "nohost")::get));
        assertTrue(address.getMessage().contains("host:port"), address.getMessage());
    }
}
