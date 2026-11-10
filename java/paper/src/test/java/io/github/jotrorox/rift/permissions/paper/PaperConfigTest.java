package io.github.jotrorox.rift.permissions.paper;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.nio.file.Path;
import java.util.Map;
import org.bukkit.configuration.file.YamlConfiguration;
import org.junit.jupiter.api.Test;

/** One template configuration for every instance: what is unset comes from Rift's variables. */
class PaperConfigTest {
    private static final Path FOLDER = Path.of("/srv/lobby-3/plugins/RiftPerms");
    private static final Map<String, String> RIFT = Map.of(
            "RIFT_SERVER_NAME", "lobby-3",
            "RIFT_SERVER_GROUP", "lobby",
            "RIFT_BUS_ADDRESS", "10.0.0.2:7443",
            "RIFT_BUS_SERVER_NAME", "rift.internal",
            "RIFT_BUS_CERT", "/srv/lobby-3/rift-bus.pem",
            "RIFT_BUS_TOKEN", "ri1.lobby-3.1.x");

    @Test
    void aTemplateWithoutConnectionSettingsUsesRiftsVariables() throws Exception {
        YamlConfiguration yaml = new YamlConfiguration();
        yaml.loadFromString("offline_policy: deny\n");
        PaperConfig config = PaperConfig.load(yaml, FOLDER, RIFT::get);
        assertEquals("lobby-3", config.server());
        assertEquals("lobby", config.group());
        assertEquals("10.0.0.2", config.host());
        assertEquals(7443, config.port());
        assertEquals("rift.internal", config.serverName());
        assertEquals(Path.of("/srv/lobby-3/rift-bus.pem"), config.certificate());
        assertEquals("RIFT_BUS_TOKEN", config.tokenEnv());
    }

    @Test
    void configuredSettingsWinAndMissingOnesAreNamed() throws Exception {
        YamlConfiguration yaml = new YamlConfiguration();
        yaml.loadFromString("server: survival-1\nbus:\n  address: 127.0.0.1:4222\n"
                + "  certificate: messaging-cert.pem\n  token_env: RIFT_PERMS_TOKEN\n");
        PaperConfig config = PaperConfig.load(yaml, FOLDER, RIFT::get);
        assertEquals("survival-1", config.server());
        assertEquals(4222, config.port());
        assertEquals(FOLDER.resolve("messaging-cert.pem"), config.certificate());
        assertEquals("RIFT_PERMS_TOKEN", config.tokenEnv());
        // The certificate name falls back to Rift's, then to the host.
        assertEquals("rift.internal", config.serverName());
        assertEquals("127.0.0.1", PaperConfig.load(yaml, FOLDER, name -> null).serverName());
        assertNull(PaperConfig.load(yaml, FOLDER, name -> null).group());
        IllegalArgumentException missing = assertThrows(IllegalArgumentException.class,
                () -> PaperConfig.load(new YamlConfiguration(), FOLDER, name -> null));
        assertTrue(missing.getMessage().contains("RIFT_BUS_ADDRESS"), missing.getMessage());
    }
}
