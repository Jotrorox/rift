package io.github.jotrorox.rift.permissions.client;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.github.jotrorox.rift.permissions.Actor;
import io.github.jotrorox.rift.permissions.CompiledSubject;
import io.github.jotrorox.rift.permissions.CompiledSubject.Entry;
import io.github.jotrorox.rift.permissions.CompiledSubject.EntryKind;
import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.GroupInfo;
import io.github.jotrorox.rift.permissions.PermissionChangeEvent;
import io.github.jotrorox.rift.permissions.Tristate;
import java.util.Arrays;
import java.util.List;
import java.util.OptionalLong;
import java.util.UUID;
import org.junit.jupiter.api.Test;

class ReplicaTest {
    static UUID uuid(long n) {
        return new UUID(0, n);
    }

    static CompiledSubject subject(long uuid, long revision, boolean value) {
        Entry fly = new Entry(EntryKind.PERMISSION, "fly", value, ContextSet.empty(), 0, OptionalLong.empty());
        return new CompiledSubject(revision, uuid(uuid), "default", List.of(fly), List.of(), List.of(), List.of());
    }

    static PermissionChangeEvent event(long revision, long... changed) {
        return new PermissionChangeEvent(revision, 0, Actor.SYSTEM, null, List.of(), false,
                Arrays.stream(changed).mapToObj(ReplicaTest::uuid).toList(), List.of());
    }

    private static Tristate fly(Replica replica, long uuid) {
        return replica.get(uuid(uuid))
                .map(s -> Resolver.check(s, "fly", ContextSet.empty(), 0))
                .orElse(Tristate.UNDEFINED);
    }

    @Test
    void newerSnapshotsWin() {
        Replica replica = new Replica();
        assertEquals(Tristate.UNDEFINED, fly(replica, 1));
        assertTrue(replica.insert(subject(1, 5, true)));
        assertFalse(replica.insert(subject(1, 4, false)), "older snapshot");
        assertFalse(replica.insert(subject(1, 5, false)), "duplicate revision");
        assertEquals(Tristate.TRUE, fly(replica, 1));
        assertTrue(replica.insert(subject(1, 6, false)));
        assertEquals(Tristate.FALSE, fly(replica, 1));
        assertEquals(1, replica.size());
        assertTrue(replica.remove(uuid(1)).isPresent());
        assertTrue(replica.isEmpty());
    }

    @Test
    void updatesTrackRevisions() {
        Replica replica = new Replica();
        assertFalse(replica.syncRevision(10), "an empty replica needs no resync");
        replica.insert(subject(1, 8, true));
        replica.insert(subject(2, 11, true));
        assertEquals(new UpdateOutcome.Applied(List.of(uuid(1)), false), replica.applyUpdate(event(11, 1, 2, 3)));
        assertEquals(11, replica.revision());
        assertInstanceOf(UpdateOutcome.Stale.class, replica.applyUpdate(event(11, 1)), "duplicate");
        assertInstanceOf(UpdateOutcome.Stale.class, replica.applyUpdate(event(9, 1)), "reordered");
        UpdateOutcome gap = replica.applyUpdate(event(13));
        assertEquals(List.of(uuid(1), uuid(2)),
                ((UpdateOutcome.Gap) gap).refetch().stream().sorted().toList());
        assertEquals(13, replica.revision());
        assertTrue(replica.syncRevision(20));
        assertFalse(replica.syncRevision(20));
    }

    @Test
    void groupChangesAskForTheTable() {
        Replica replica = new Replica();
        replica.syncRevision(1);
        PermissionChangeEvent withGroups = new PermissionChangeEvent(2, 0, Actor.SYSTEM, null, List.of(), false,
                List.of(), List.of("vip"));
        assertEquals(new UpdateOutcome.Applied(List.of(), true), replica.applyUpdate(withGroups));
    }

    @Test
    void evictionAfterLeaving() {
        Replica replica = new Replica();
        replica.insert(subject(1, 1, true));
        replica.insert(subject(2, 1, true));
        replica.markLeft(uuid(1), 100);
        replica.markLeft(uuid(2), 100);
        replica.markJoined(uuid(2));
        assertTrue(replica.evict(699, Replica.EVICT_AFTER).isEmpty());
        assertEquals(List.of(uuid(1)), replica.evict(700, Replica.EVICT_AFTER));
        assertEquals(List.of(uuid(2)), replica.uuids());
        replica.markLeft(uuid(2), 100);
        assertTrue(replica.insert(subject(2, 2, false)));
        assertEquals(List.of(uuid(2)), replica.evict(700, Replica.EVICT_AFTER), "a newer snapshot keeps the departure");
    }

    @Test
    void groupTable() {
        Replica replica = new Replica();
        GroupInfo vip = new GroupInfo("vip", 1, null, List.of());
        assertTrue(replica.setGroups(5, List.of(vip)));
        assertFalse(replica.setGroups(4, List.of()));
        assertEquals(new Replica.Groups(5, List.of(vip)), replica.groups());
    }
}
