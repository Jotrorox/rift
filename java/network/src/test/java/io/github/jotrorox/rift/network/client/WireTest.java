package io.github.jotrorox.rift.network.client;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.github.jotrorox.rift.network.NetworkPlayer;
import io.github.jotrorox.rift.network.ServerInfo;
import io.github.jotrorox.rift.network.ServerState;
import io.github.jotrorox.rift.permissions.client.Json;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.OptionalInt;
import java.util.UUID;
import org.junit.jupiter.api.Test;

class WireTest {
    @Test
    void serversCarryGroupStateAndLimits() {
        List<ServerInfo> servers = Wire.servers(Json.parse("""
                [{"name":"lobby-1","group":"lobby","state":"running","service_state":"INGAME",
                  "properties":{"map":"Desert"},"players":50,"max_players":50},
                 {"name":"survival","group":null,"state":"draining","players":3,"max_players":0},
                 {"name":"later","group":null,"state":"hibernating","players":0,"max_players":0,"extra":true}]"""));
        assertEquals(
                new ServerInfo(
                        "lobby-1",
                        Optional.of("lobby"),
                        ServerState.RUNNING,
                        Optional.of("INGAME"),
                        Map.of("map", "Desert"),
                        50,
                        OptionalInt.of(50)),
                servers.get(0));
        assertTrue(servers.get(0).acceptsPlayers());
        assertTrue(servers.get(0).isFull());
        assertEquals(
                new ServerInfo(
                        "survival",
                        Optional.empty(),
                        ServerState.DRAINING,
                        Optional.empty(),
                        Map.of(),
                        3,
                        OptionalInt.empty()),
                servers.get(1));
        assertFalse(servers.get(1).acceptsPlayers());
        assertFalse(servers.get(1).isFull());
        // A state from a newer proxy does not break the list.
        assertEquals(ServerState.UNKNOWN, servers.get(2).state());
    }

    @Test
    void playersUseTheProtocolsUuidForm() {
        UUID uuid = UUID.fromString("f47ac10b-58cc-4372-a567-0e02b2c3d479");
        assertEquals("f47ac10b58cc4372a5670e02b2c3d479", Wire.hex(uuid));
        Optional<NetworkPlayer> player = Wire.optionalPlayer(Json.parse("""
                {"connection_id":7,"uuid":"f47ac10b58cc4372a5670e02b2c3d479","name":"Alice","server":"lobby-1",
                 "protocol":774,"visible_to":"vanish.see","locale":null}"""));
        assertEquals(Optional.of(new NetworkPlayer(uuid, "Alice", "lobby-1", Optional.of("vanish.see"))), player);
        assertTrue(player.orElseThrow().hidden());
        assertEquals(Optional.empty(), Wire.optionalPlayer(Json.parse("null")));
    }

    @Test
    void malformedRepliesAreRefused() {
        assertThrows(IllegalArgumentException.class, () -> Wire.servers(Json.parse("{}")));
        assertThrows(IllegalArgumentException.class, () -> Wire.servers(Json.parse("[{\"name\":\"x\"}]")));
        assertThrows(IllegalArgumentException.class, () -> Wire.bool(Json.parse("1")));
        assertThrows(
                IllegalArgumentException.class,
                () -> Wire.players(Json.parse("[{\"uuid\":\"nope\",\"name\":\"A\",\"server\":\"s\"}]")));
    }
}
