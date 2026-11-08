package io.github.jotrorox.rift.permissions.test;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.github.jotrorox.rift.permissions.Actor;
import io.github.jotrorox.rift.permissions.Applied;
import io.github.jotrorox.rift.permissions.Change;
import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.ErrorCode;
import io.github.jotrorox.rift.permissions.Filter;
import io.github.jotrorox.rift.permissions.Node;
import io.github.jotrorox.rift.permissions.NodeKind;
import io.github.jotrorox.rift.permissions.Patch;
import io.github.jotrorox.rift.permissions.PermissionChangeEvent;
import io.github.jotrorox.rift.permissions.PermissionException;
import io.github.jotrorox.rift.permissions.SubjectUpdatedEvent;
import io.github.jotrorox.rift.permissions.Tristate;
import java.time.Clock;
import java.time.Duration;
import java.time.Instant;
import java.time.ZoneOffset;
import java.util.ArrayList;
import java.util.List;
import java.util.UUID;
import java.util.concurrent.CompletionException;
import org.junit.jupiter.api.Test;

class InMemoryRiftPermissionsTest {
    private static final UUID ALICE = new UUID(0, 1);
    private static final ContextSet GLOBAL = ContextSet.empty();

    private static InMemoryRiftPermissions ranks() {
        return InMemoryRiftPermissions.builder()
                .group("default", group -> group.permission("chat.use", true))
                .group("vip", group -> group.weight(10).permission("essentials.fly", true).prefix(100, "[VIP] "))
                .group("mod", group -> group.weight(20).parent("vip").permission("essentials.kick", true))
                .track("ranks", "default", "vip", "mod")
                .user(ALICE, user -> user.parent("vip").meta("homes", "3"))
                .build();
    }

    @Test
    void readsFollowTheProxyRules() {
        InMemoryRiftPermissions perms = ranks();
        assertEquals(Tristate.TRUE, perms.check(ALICE, "Essentials.Fly", GLOBAL));
        assertEquals(Tristate.TRUE, perms.check(ALICE, "chat.use", GLOBAL));
        assertEquals(Tristate.UNDEFINED, perms.check(ALICE, "essentials.kick", GLOBAL));
        assertEquals(Tristate.TRUE, perms.check(ALICE, "group.vip", GLOBAL));
        assertEquals("[VIP] ", perms.meta(ALICE, GLOBAL).prefix());
        assertEquals("3", perms.meta(ALICE, GLOBAL).value("homes").orElseThrow());
        assertEquals("vip", perms.meta(ALICE, GLOBAL).primaryGroup());
        assertEquals(List.of("vip", "default"), perms.groupsOf(ALICE, GLOBAL));
        assertTrue(perms.compiled(new UUID(0, 99)).isPresent(), "every UUID compiles");
        assertEquals(Tristate.TRUE, perms.check(ALICE, "ESSENTIALS.FLY", perms.contexts().of(ALICE)));
        assertEquals(Tristate.TRUE, perms.check((Object) ALICE, "essentials.fly"));
    }

    @Test
    void writesBumpTheRevisionAndFireEvents() {
        InMemoryRiftPermissions perms = ranks();
        List<PermissionChangeEvent> changes = new ArrayList<>();
        List<SubjectUpdatedEvent> updates = new ArrayList<>();
        perms.events().subscribe(PermissionChangeEvent.class, changes::add);
        perms.events().subscribe(SubjectUpdatedEvent.class, updates::add);

        Applied applied = perms.user(ALICE).set(Node.permission("essentials.fly", false).context("server", "test"))
                .join();
        assertEquals(2, applied.revision());
        assertEquals(List.of(ALICE), applied.changedUsers());
        assertEquals(Tristate.FALSE, perms.check(ALICE, "essentials.fly", perms.contexts().of(ALICE)));
        assertEquals(Tristate.TRUE, perms.check(ALICE, "essentials.fly", GLOBAL));
        assertEquals(1, changes.size());
        assertInstanceOf(Actor.Backend.class, changes.getFirst().actor());
        assertEquals(List.of(new SubjectUpdatedEvent(ALICE, 2)), updates);

        Applied unchanged = perms.user(ALICE).set(Node.permission("essentials.fly", false).context("server", "test"))
                .join();
        assertEquals(new Applied(2, 0, List.of(), List.of()), unchanged, "no-op writes create no revision");

        perms.group("vip").setPrefix(100, "[V] ").join();
        assertEquals("[V] ", perms.meta(ALICE, GLOBAL).prefix());
        assertEquals(List.of("vip"), changes.getLast().groups());
    }

    @Test
    void groupLifecycleAndErrors() {
        InMemoryRiftPermissions perms = ranks();
        perms.group("admin").create(50, "Admin").join();
        assertEquals("Admin", perms.group("admin").info().orElseThrow().displayName());
        CompletionException duplicate = assertThrows(CompletionException.class, () -> perms.group("admin").create().join());
        assertEquals(ErrorCode.CONFLICT, ((PermissionException) duplicate.getCause()).code());
        CompletionException missing = assertThrows(CompletionException.class, () -> perms.group("ghost").setWeight(1).join());
        assertEquals(ErrorCode.NOT_FOUND, ((PermissionException) missing.getCause()).code());
        perms.group("vip").delete().join();
        assertEquals(Tristate.UNDEFINED, perms.check(ALICE, "essentials.fly", GLOBAL), "dangling inherit grants nothing");
        perms.group("vip").create().join();
        assertEquals(Tristate.TRUE, perms.check(ALICE, "group.vip", GLOBAL), "recreating the group resolves again");
    }

    @Test
    void promoteAndDemoteAlongATrack() {
        InMemoryRiftPermissions perms = ranks();
        perms.track("ranks").promote(ALICE).join();
        assertEquals(List.of("mod", "vip", "default"), perms.groupsOf(ALICE, GLOBAL));
        CompletionException top = assertThrows(CompletionException.class, () -> perms.track("ranks").promote(ALICE).join());
        assertEquals(ErrorCode.CONFLICT, ((PermissionException) top.getCause()).code());
        perms.track("ranks").demote(ALICE).join();
        perms.track("ranks").demote(ALICE).join();
        assertEquals(List.of("default"), perms.groupsOf(ALICE, GLOBAL), "demoting to default adds no node");
        perms.track("ranks").promote(ALICE).join();
        assertEquals(List.of("vip", "default"), perms.groupsOf(ALICE, GLOBAL), "promoting from default");
    }

    @Test
    void bulkAndAtomicBatches() {
        InMemoryRiftPermissions perms = ranks();
        perms.apply(Change.bulk(Filter.kind(NodeKind.PERMISSION).keyLike("ESSENTIALS.%").groups(), Patch.value(false)))
                .join();
        assertEquals(Tristate.FALSE, perms.check(ALICE, "essentials.fly", GLOBAL));
        assertEquals(Tristate.TRUE, perms.check(ALICE, "chat.use", GLOBAL));

        long revision = perms.revision();
        Change batch = Change.batch(Change.setNode(io.github.jotrorox.rift.permissions.SubjectRef.user(ALICE),
                Node.permission("x", true)), new Change.SetWeight("ghost", 1));
        assertThrows(CompletionException.class, () -> perms.apply(batch).join());
        assertEquals(revision, perms.revision());
        assertEquals(Tristate.UNDEFINED, perms.check(ALICE, "x", GLOBAL), "a failed batch changes nothing");
    }

    @Test
    void expiryIsEvaluatedAgainstTheClock() {
        Clock clock = Clock.fixed(Instant.ofEpochSecond(1_000), ZoneOffset.UTC);
        InMemoryRiftPermissions perms = InMemoryRiftPermissions.builder()
                .clock(clock)
                .user(ALICE, user -> user.node(Node.permission("temp", true).expiresAt(1_001)))
                .build();
        assertEquals(Tristate.TRUE, perms.check(ALICE, "temp", GLOBAL));
        InMemoryRiftPermissions later = InMemoryRiftPermissions.builder()
                .clock(Clock.offset(clock, Duration.ofSeconds(1)))
                .user(ALICE, user -> user.node(Node.permission("temp", true).expiresAt(1_001)))
                .build();
        assertEquals(Tristate.UNDEFINED, later.check(ALICE, "temp", GLOBAL));
    }

    @Test
    void calculatorsAddContexts() {
        InMemoryRiftPermissions perms = InMemoryRiftPermissions.builder()
                .user(ALICE, user -> user.node(Node.permission("build", true).context("region", "spawn")))
                .build();
        assertEquals(Tristate.UNDEFINED, perms.check((Object) ALICE, "build"));
        var registration = perms.contexts().register((UUID player, ContextSet.Builder contexts) ->
                contexts.add("region", "spawn"));
        assertEquals(Tristate.TRUE, perms.check((Object) ALICE, "build"));
        registration.close();
        assertFalse(perms.check((Object) ALICE, "build").isDefined());
    }

    @Test
    void likeMatchesSqlite() {
        assertTrue(Store.like("essentials.%", "Essentials.fly"));
        assertTrue(Store.like("a_c", "abc"));
        assertFalse(Store.like("a_c", "ac"));
        assertTrue(Store.like("100\\%", "100%"));
        assertFalse(Store.like("100\\%", "1000"));
        assertTrue(Store.like("%", ""));
    }
}
