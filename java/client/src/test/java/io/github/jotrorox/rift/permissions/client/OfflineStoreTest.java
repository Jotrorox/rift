package io.github.jotrorox.rift.permissions.client;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.github.jotrorox.rift.permissions.CompiledSubject;
import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.Tristate;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.UUID;
import java.util.logging.Logger;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/** The offline policy files, in the Rust client's formats ({@code docs/permissions-clients.md}). */
class OfflineStoreTest {
    private static final Logger LOGGER = Logger.getLogger(OfflineStoreTest.class.getName());
    private static final UUID STAFF = new UUID(0, 1);
    private static final UUID GUEST = new UUID(0, 2);

    /** The Rust client's {@code fallback_file_compiles_like_the_proxy}, line for line. */
    @Test
    void fallbackFileCompilesLikeTheProxy() {
        Fallback fallback = Fallback.parse(Json.parse("""
                {"groups": [
                   {"name": "default", "nodes": [{"kind": "permission", "key": "spawn", "value": true}]},
                   {"name": "Staff", "weight": 100, "nodes": [
                      {"kind": "permission", "key": "essentials.*", "value": true},
                      {"kind": "prefix", "priority": 100, "text": "[Staff] "}]}],
                 "users": [{"uuid": "00000000000000000000000000000001",
                            "nodes": [{"kind": "inherit", "group": "staff", "value": true}]}]}
                """));
        ContextSet none = ContextSet.empty();
        CompiledSubject staff = fallback.subject(STAFF, 0);
        assertEquals(0, staff.revision());
        assertEquals(Tristate.TRUE, Resolver.check(staff, "essentials.fly", none, 0));
        assertEquals(Tristate.TRUE, Resolver.check(staff, "spawn", none, 0));
        assertEquals("[Staff] ", Resolver.meta(staff, none, 0).prefix());
        assertEquals("staff", staff.primaryGroup());
        CompiledSubject guest = fallback.subject(GUEST, 0);
        assertEquals(Tristate.UNDEFINED, Resolver.check(guest, "essentials.fly", none, 0));
        assertEquals(Tristate.TRUE, Resolver.check(guest, "spawn", none, 0));
        assertEquals("default", guest.primaryGroup());

        assertThrows(IllegalArgumentException.class, () -> Fallback.parse(Json.parse("{\"groups\": [{\"name\": \"a b\"}]}")));
        assertThrows(IllegalArgumentException.class, () -> Fallback.parse(Json.parse("{\"users\": [{\"uuid\": \"x\"}]}")));
        assertThrows(IllegalArgumentException.class, () -> Fallback.parse(Json.parse("{\"extra\": 1}")));
        assertThrows(IllegalArgumentException.class,
                () -> Fallback.parse(Json.parse("{\"groups\": [{\"name\": \"vip\"}, {\"name\": \"VIP\"}]}")));
        // The pre-release format, a bare node list, is refused rather than misread.
        assertThrows(IllegalArgumentException.class,
                () -> Fallback.parse(Json.parse("{\"primary_group\": \"default\", \"nodes\": []}")));
    }

    @Test
    void invalidFallbackFilesFailOnLoad(@TempDir Path directory) throws IOException {
        Path file = directory.resolve("fallback.json");
        assertThrows(IOException.class, () -> OfflineStore.load(OfflinePolicy.fallbackFile(file), LOGGER));
        Files.writeString(file, "{\"groups\": [{\"name\": \"a b\"}]}");
        assertThrows(IOException.class, () -> OfflineStore.load(OfflinePolicy.fallbackFile(file), LOGGER));
        Files.writeString(file, "{}");
        OfflineStore store = OfflineStore.load(OfflinePolicy.fallbackFile(file), LOGGER);
        assertTrue(store.hasFallback());
        assertEquals(List.of(new CompiledSubject.Entry(CompiledSubject.EntryKind.GROUP, "default", true,
                ContextSet.empty(), 1, java.util.OptionalLong.empty())), store.fallback(GUEST, 0).entries());
    }

    @Test
    void lastSnapshotIsTheBatchReplyShape(@TempDir Path directory) throws IOException {
        Path file = directory.resolve("nested/replica.json");
        OfflineStore missing = OfflineStore.load(OfflinePolicy.lastSnapshot(file), LOGGER);
        assertNull(missing.snapshot(), "a missing file starts empty");
        CompiledSubject subject = Codec.readCompiledSubject(Json.parse("""
                {"revision": 7, "uuid": "00000000000000000000000000000001", "primary_group": "default",
                 "entries": [{"k": "perm", "key": "a.b", "v": true, "ctx": {}, "rank": 0}]}
                """));
        missing.save(9, List.of(subject));
        Json written = Json.parse(Files.readString(file, StandardCharsets.UTF_8));
        assertEquals(Json.parse("""
                {"revision": 9, "subjects": [{"revision": 7, "uuid": "00000000000000000000000000000001",
                  "primary_group": "default", "entries": [{"k": "perm", "key": "a.b", "v": true, "ctx": {}, "rank": 0}],
                  "prefix": [], "suffix": [], "meta": []}]}
                """), written);
        Protocol.SubjectBatchReply loaded = OfflineStore.load(OfflinePolicy.lastSnapshot(file), LOGGER).snapshot();
        assertEquals(new Protocol.SubjectBatchReply(9, List.of(subject)), loaded);

        Files.writeString(file, "not json");
        assertNull(OfflineStore.load(OfflinePolicy.lastSnapshot(file), LOGGER).snapshot(),
                "an unreadable file starts empty");
    }
}
