package io.github.jotrorox.rift.control;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.github.jotrorox.rift.messaging.Message;
import io.github.jotrorox.rift.messaging.MessagingException;
import io.github.jotrorox.rift.permissions.client.Json;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.OptionalInt;
import org.junit.jupiter.api.Test;

class ControlParsingTest {
    private static byte[] bytes(String json) {
        return json.getBytes(StandardCharsets.UTF_8);
    }

    private static Json.Obj object(String json) {
        return (Json.Obj) Json.parse(json);
    }

    private static Optional<RiftEvents.Event> event(String subject, String json) {
        return RiftEvents.parse(new Message(subject, null, bytes(json)));
    }

    @Test
    void unwrapsSuccessfulReplies() throws Exception {
        Json data = RiftControl.unwrap("maintenance", bytes("{\"ok\":true,\"data\":{\"maintenance\":true}}"));
        assertEquals(new Json.Bool(true), ((Json.Obj) data).get("maintenance"));
        assertEquals(Json.Null.INSTANCE, RiftControl.unwrap("reload", bytes("{\"ok\":true}")));
    }

    @Test
    void reportsRefusedCommands() {
        ControlException error = assertThrows(ControlException.class,
                () -> RiftControl.unwrap("drain", bytes("{\"ok\":false,\"error\":\"unknown backend\"}")));
        assertEquals("drain", error.command());
        assertEquals("drain: unknown backend", error.getMessage());
        assertEquals(MessagingException.OPERATION, error.code());
        for (String malformed : new String[] {"not json", "[1]"}) {
            MessagingException invalid =
                    assertThrows(MessagingException.class, () -> RiftControl.unwrap("status", bytes(malformed)));
            assertFalse(invalid instanceof ControlException, malformed);
            assertEquals(MessagingException.CLIENT, invalid.code());
        }
    }

    @Test
    void commandsAreOneSubjectToken() {
        assertEquals("rift.control.servers", RiftControl.subject("servers"));
        for (String command : new String[] {"", "a.b", "*", ">", "a b"}) {
            assertThrows(IllegalArgumentException.class, () -> RiftControl.subject(command), command);
        }
        assertThrows(IllegalArgumentException.class, () -> RiftControl.subject(null));
    }

    @Test
    void parsesStatus() {
        ProxyStatus status = ProxyStatus.parse(object("""
                {"maintenance":false,"connections":3,"players_online":2,
                 "players":[{"connection_id":7,"name":"Steve","backend":"lobby","protocol":774},
                            {"connection_id":9,"name":"Alex","backend":"survival","protocol":772}],
                 "backends":[{"backend":"lobby","up":true,"draining":false,"service_state":null,"properties":{}},
                             {"backend":"survival","up":false,"draining":true,"service_state":"INGAME",
                              "properties":{"map":"Desert"}}],
                 "managed_servers":[{"name":"survival","state":"running"}],
                 "future_field":1}
                """));
        assertFalse(status.maintenance());
        assertEquals(3, status.connections());
        assertEquals(2, status.playersOnline());
        assertEquals(new ProxyStatus.Player(7, "Steve", "lobby", 774), status.player("steve").orElseThrow());
        assertEquals(1, status.playersOn("survival").size());
        assertEquals(
                new ProxyStatus.Backend("lobby", true, false, Optional.empty(), Map.of()),
                status.backend("lobby").orElseThrow());
        assertEquals(
                new ProxyStatus.Backend("survival", false, true, Optional.of("INGAME"), Map.of("map", "Desert")),
                status.backend("survival").orElseThrow());
        assertEquals(
                new Json.Str("running"), ((Json.Obj) status.managedServers().items().get(0)).get("state"));
        assertEquals(Json.Num.of(1), status.raw().get("future_field"));
    }

    @Test
    void parsesSparseStatus() {
        ProxyStatus status = ProxyStatus.parse(object("{}"));
        assertTrue(status.players().isEmpty());
        assertTrue(status.backends().isEmpty());
        assertTrue(status.managedServers().items().isEmpty());
    }

    @Test
    void parsesNodes() {
        List<NodeInfo> nodes = NodeInfo.parse(Json.parse("""
                {"nodes":[
                 {"name":"edge-1","address":"10.0.0.5","labels":["eu","ssd"],"state":"online","memory_mb":8192,
                  "allocated_memory_mb":2048,"max_instances":8,"instances":2,"running":2,"version":"0.4.0",
                  "last_seen_ms":150,"connected_ms":60000,"future_field":1},
                 {"name":"edge-2","address":"10.0.0.6","labels":[],"state":"never_connected","memory_mb":null,
                  "allocated_memory_mb":0,"max_instances":4,"instances":0,"running":null,"version":null,
                  "last_seen_ms":null,"connected_ms":null},
                 {"name":"edge-3","address":"10.0.0.7","labels":[],"state":"rebooting","memory_mb":null,
                  "allocated_memory_mb":0,"max_instances":4,"instances":0,"running":0,"version":"0.4.0",
                  "last_seen_ms":30000,"connected_ms":null}]}
                """));
        assertEquals(3, nodes.size());
        NodeInfo online = nodes.get(0);
        assertEquals("edge-1", online.name());
        assertEquals("10.0.0.5", online.address());
        assertEquals(List.of("eu", "ssd"), online.labels());
        assertTrue(online.isOnline());
        assertEquals(OptionalInt.of(8192), online.memoryMb());
        assertEquals(2048, online.allocatedMemoryMb());
        assertEquals(8, online.maxInstances());
        assertEquals(2, online.instances());
        assertEquals(OptionalInt.of(2), online.running());
        assertEquals(Optional.of("0.4.0"), online.version());
        assertEquals(Optional.of(Duration.ofMillis(150)), online.lastSeen());
        assertEquals(Optional.of(Duration.ofMinutes(1)), online.connected());
        assertEquals(Json.Num.of(1), online.raw().get("future_field"));

        NodeInfo never = nodes.get(1);
        assertEquals(NodeInfo.State.NEVER_CONNECTED, never.state());
        assertFalse(never.isOnline());
        assertEquals(OptionalInt.empty(), never.memoryMb());
        assertEquals(OptionalInt.empty(), never.running());
        assertEquals(Optional.empty(), never.version());
        assertEquals(Optional.empty(), never.lastSeen());
        assertEquals(NodeInfo.State.UNKNOWN, nodes.get(2).state(), "a state added by a newer Rift");

        assertTrue(NodeInfo.parse(Json.parse("{\"nodes\":[]}")).isEmpty());
        assertThrows(IllegalArgumentException.class, () -> NodeInfo.parse(Json.parse("[]")));
    }

    @Test
    void parsesEvents() {
        RiftEvents.Lifecycle lifecycle = assertInstanceOf(
                RiftEvents.Lifecycle.class, event(RiftEvents.LIFECYCLE, "{\"state\":\"draining\"}").orElseThrow());
        assertTrue(lifecycle.isDraining());
        assertFalse(lifecycle.isReady());
        assertEquals(
                new RiftEvents.Reload("abc123"), event(RiftEvents.RELOAD, "{\"revision\":\"abc123\"}").orElseThrow());
        assertEquals(
                new RiftEvents.Control("drain", false),
                event(RiftEvents.CONTROL, "{\"command\":\"drain\",\"ok\":false}").orElseThrow());
        assertTrue(event("rift.events.other", "{}").isEmpty());
        assertTrue(event(RiftEvents.RELOAD, "{").isEmpty());
        assertTrue(event(RiftEvents.RELOAD, "[]").isEmpty());
    }

    @Test
    void parsesPlayerServerAndInstanceEvents() {
        RiftEvents.Player switched = assertInstanceOf(RiftEvents.Player.class, event(RiftEvents.PLAYERS, """
                {"event":"switch","connection_id":"7","uuid":"0123456789abcdef0123456789abcdef","name":"Alice",
                 "server":"survival","previous":"lobby","visible_to":"staff.see"}
                """).orElseThrow());
        assertEquals(RiftEvents.Player.Kind.SWITCH, switched.kind());
        assertEquals(7, switched.connectionId());
        assertEquals("0123456789abcdef0123456789abcdef", switched.uuid());
        assertEquals("Alice", switched.name());
        assertEquals("survival", switched.server());
        assertEquals(Optional.of("lobby"), switched.previous());
        assertEquals(Optional.of("staff.see"), switched.visibleTo());

        RiftEvents.Player joined = assertInstanceOf(RiftEvents.Player.class, event(RiftEvents.PLAYERS, """
                {"event":"join","connection_id":"8","uuid":"u","name":"Bob","server":"lobby","visible_to":null}
                """).orElseThrow());
        assertEquals(RiftEvents.Player.Kind.JOIN, joined.kind());
        assertEquals(Optional.empty(), joined.previous());
        assertEquals(Optional.empty(), joined.visibleTo());
        assertTrue(event(RiftEvents.PLAYERS, "{\"event\":\"teleport\"}").isEmpty(), "a kind added by a newer Rift");

        assertEquals(
                new RiftEvents.ServerState("game-1", Optional.of("INGAME"), Map.of("map", "Desert"), true),
                event(RiftEvents.SERVERS, """
                        {"event":"state","server":"game-1","state":"INGAME","properties":{"map":"Desert"},
                         "draining":true}
                        """).orElseThrow());
        assertEquals(
                new RiftEvents.ServerState("game-1", Optional.empty(), Map.of(), false),
                event(RiftEvents.SERVERS, """
                        {"event":"state","server":"game-1","state":null,"properties":{},"draining":false}
                        """).orElseThrow());

        RiftEvents.Instance created = assertInstanceOf(RiftEvents.Instance.class, event(RiftEvents.INSTANCES, """
                {"name":"lobby-2","group":"lobby","port":25601,"address":"127.0.0.1:25601","template":"paper",
                 "storage":"disposable","created":true,"event":"created"}
                """).orElseThrow());
        assertTrue(created.created());
        assertEquals("lobby-2", created.name());
        assertEquals(Optional.of("lobby"), created.group());
        assertEquals(Json.Num.of(25601), created.raw().get("port"));
        RiftEvents.Instance removed = assertInstanceOf(RiftEvents.Instance.class, event(RiftEvents.INSTANCES, """
                {"name":"lobby-2","removed":true,"storage":"disposable","files_removed":true,"cleanup_error":null,
                 "event":"removed"}
                """).orElseThrow());
        assertFalse(removed.created());
        assertEquals("lobby-2", removed.name());
        assertEquals(Optional.empty(), removed.group());
    }
}
