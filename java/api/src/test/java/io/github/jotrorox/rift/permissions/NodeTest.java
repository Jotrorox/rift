package io.github.jotrorox.rift.permissions;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.List;
import java.util.OptionalLong;
import java.util.UUID;
import org.junit.jupiter.api.Test;

/** Validation parity with the Rust crate's {@code node}, {@code subject} and {@code change} tests. */
class NodeTest {
    @Test
    void nodePatterns() {
        for (String good : List.of("a", "a.b", "a.*", "*", "minecraft:command.gamemode", "minecraft:command.*",
                "x-y_z.0")) {
            assertTrue(Node.isValidPattern(good), good);
        }
        for (String bad : List.of("", "a.", ".a", "a..b", "a*", "a.*.b", "*.a", "a.**", "minecraft:*", "A.b", "a b",
                "a/b")) {
            assertFalse(Node.isValidPattern(bad), bad);
        }
        assertTrue(Node.isValidPattern("a".repeat(256)));
        assertFalse(Node.isValidPattern("a".repeat(257)));
        assertEquals("essentials.fly", Node.normalizePattern("Essentials.FLY"));
        assertThrows(IllegalArgumentException.class, () -> Node.normalizePattern("é.fly"));
    }

    @Test
    void kindsHaveStableCodes() {
        NodeKind[] kinds = NodeKind.values();
        for (int code = 0; code < kinds.length; code++) {
            assertEquals(code, kinds[code].code());
            assertEquals(kinds[code], NodeKind.fromWireName(kinds[code].wireName()));
        }
        assertEquals(6, NodeKind.DISPLAY_NAME.code());
        assertEquals("display_name", NodeKind.DISPLAY_NAME.wireName());
    }

    @Test
    void constructorsNormaliseAndValidate() {
        Node node = Node.permission("Essentials.Fly", true).context("Server", "Survival").expiresAt(5);
        assertEquals(new Node.Permission("essentials.fly", true), node.payload());
        assertEquals("server=survival", node.contexts().toString());
        assertEquals(OptionalLong.of(5), node.expiresAt());
        assertEquals("homes", ((Node.Meta) Node.meta("Homes", "5").payload()).key());
        assertEquals("plugin:essentials", Node.meta("homes", "5").source("plugin:essentials").source());
        assertThrows(IllegalArgumentException.class, () -> Node.permission("a..b", true));
        assertThrows(IllegalArgumentException.class, () -> Node.inherit("a:b", true));
        assertThrows(IllegalArgumentException.class, () -> Node.prefix(1, "a\nb"));
        assertThrows(IllegalArgumentException.class, () -> Node.displayName(""));
        assertThrows(IllegalArgumentException.class, () -> Node.permission("a", true).source("plugin:"));
        assertThrows(IllegalArgumentException.class,
                () -> Node.permission("a", true).context("server", "a").context("server", "b"));
        assertTrue(Node.permission("a", true).context("region", "a").context("region", "b").contexts().size() == 2);
    }

    @Test
    void storageKeys() {
        assertEquals("100", Node.prefix(100, "x").payload().storageKey());
        assertEquals("weight", Node.weight(3).payload().storageKey());
        assertEquals("false", Node.permission("a.*", false).payload().storageValue());
        assertTrue(Node.permission("a", true).expiresAt(10).isExpired(10));
        assertFalse(Node.permission("a", true).expiresAt(10).isExpired(9));
    }

    @Test
    void changesNormaliseAndCheckStructure() {
        SubjectRef user = SubjectRef.user(new UUID(0, 1));
        assertThrows(IllegalArgumentException.class, () -> Change.setNode(user, Node.weight(1)));
        assertThrows(IllegalArgumentException.class, () -> new Change.DeleteGroup("Default"));
        assertEquals(new Change.CreateGroup("vip", 1, null), new Change.CreateGroup("VIP", 1, null));
        assertThrows(IllegalArgumentException.class, () -> new Change.Batch(List.of()));
        assertThrows(IllegalArgumentException.class,
                () -> Change.batch(Change.batch(new Change.DeleteTrack("x"))));
        assertThrows(IllegalArgumentException.class, () -> new Change.SetTrack("R", List.of("a", "A")));
        assertThrows(IllegalArgumentException.class, () -> Change.bulk(Filter.all(), new Patch(false, null, null, null,
                false, null)));
        assertThrows(IllegalArgumentException.class,
                () -> new Change.UnsetNode(user, NodeKind.PREFIX, "x", ContextSet.empty()));
        assertEquals("100", ((Change.UnsetNode) Change.unsetNode(SubjectRef.group("VIP"), Node.prefix(100, "x"))).key());
        assertEquals(new Change.UnsetNode(user, NodeKind.PERMISSION, "a.b", ContextSet.empty()),
                new Change.UnsetNode(user, NodeKind.PERMISSION, "A.B", ContextSet.empty()));
    }

    @Test
    void patchesCombine() {
        Patch patch = Patch.value(false).and(Patch.makePermanent()).and(Patch.source("import"));
        assertEquals(new Patch(false, false, null, null, true, "import"), patch);
        assertFalse(patch.isEmpty());
    }

    @Test
    void actorsAndUuids() {
        String hex = "0123456789abcdef0123456789abcdef";
        UUID uuid = Uuids.parse(hex);
        assertEquals(hex, Uuids.toHex(uuid));
        assertEquals(uuid, Uuids.parse("01234567-89AB-CDEF-0123-456789ABCDEF"));
        assertEquals(UUID.fromString("01234567-89ab-cdef-0123-456789abcdef"), uuid);
        for (String bad : List.of("", "0123", "0123456789abcdef0123456789abcdeg", "01234567-89ab-cdef-0123_456789abcdef")) {
            assertThrows(IllegalArgumentException.class, () -> Uuids.parse(bad), bad);
        }
        for (String text : List.of("uuid:" + hex, "operator:alice", "lua:shop", "backend:server.lobby", "cli", "import",
                "system")) {
            assertEquals(text, Actor.parse(text).toString());
        }
        for (String bad : List.of("", "uuid:x", "operator:", "lua:a b", "root", "cli:x")) {
            assertThrows(IllegalArgumentException.class, () -> Actor.parse(bad), bad);
        }
        assertEquals("group:vip", SubjectRef.group("VIP").toString());
    }
}
