package io.github.jotrorox.rift.network.test;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.github.jotrorox.rift.network.NetworkException;
import io.github.jotrorox.rift.network.NetworkTimeoutException;
import io.github.jotrorox.rift.network.RiftNetwork;
import io.github.jotrorox.rift.network.ServerInfo;
import io.github.jotrorox.rift.network.ServerState;
import io.github.jotrorox.rift.network.Subscription;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;
import org.junit.jupiter.api.Test;

class InMemoryRiftNetworkTest {
    private static final UUID ALICE = new UUID(0, 1);
    private static final UUID BOB = new UUID(0, 2);

    private static InMemoryRiftNetwork network() {
        return InMemoryRiftNetwork.builder("lobby-1", "lobby")
                .server("lobby-1", "lobby", 50)
                .server("bedwars-1", "bedwars", 1)
                .server("bedwars-2", "bedwars", 1)
                .server("bedwars-3", "bedwars", 1, ServerState.DRAINING)
                .server("survival")
                .player(ALICE, "Alice", "lobby-1")
                .player(BOB, "Bob", "survival")
                .build();
    }

    private static NetworkException failure(CompletableFuture<?> future) {
        return assertInstanceOf(
                NetworkException.class, assertThrows(CompletionException.class, future::join).getCause());
    }

    @Test
    void listsServersAndPlayersByServerOrGroup() {
        InMemoryRiftNetwork network = network();
        assertEquals(Optional.of("lobby"), network.localGroup());
        assertEquals(
                List.of("bedwars-1", "bedwars-2", "bedwars-3", "lobby-1", "survival"),
                network.servers().join().stream().map(ServerInfo::name).toList());
        assertEquals(3, network.servers("Bedwars").join().size());
        ServerInfo lobby = network.servers("lobby-1").join().get(0);
        assertEquals(1, lobby.players());
        assertEquals(50, lobby.maxPlayers().orElseThrow());
        assertEquals(List.of("Alice"), network.players("lobby").join().stream().map(p -> p.name()).toList());
        assertEquals("survival", network.player("bob").join().orElseThrow().server());
        assertEquals(NetworkException.Reason.REFUSED, failure(network.servers("missing")).reason());

        network.setHiddenPlayer(new UUID(0, 3), "Carol", "lobby-1", "vanish.see");
        assertEquals(1, network.servers("lobby-1").join().get(0).players());
        assertEquals(2, network.players("lobby-1").join().size());
    }

    @Test
    void connectsToAnInstanceOfAGroupThatHasRoom() {
        InMemoryRiftNetwork network = network();
        network.setPlayer(new UUID(0, 9), "Dave", "bedwars-1");
        assertTrue(network.connectToGroup(ALICE, "bedwars").join());
        assertEquals("bedwars-2", network.player(ALICE).join().orElseThrow().server());

        // Only its own players, like a backend's bus login.
        assertEquals(NetworkException.Reason.REFUSED, failure(network.connect(BOB, "lobby-1")).reason());
        assertFalse(network.connect(new UUID(0, 4), "survival").join());
        // A group is not a server name here, and the other way round is refused.
        assertEquals(NetworkException.Reason.REFUSED, failure(network.connectToGroup(ALICE, "survival")).reason());

        InMemoryRiftNetwork friends = InMemoryRiftNetwork.builder("hub")
                .server("hub")
                .server("survival")
                .player(BOB, "Bob", "survival")
                .connectAnyone()
                .build();
        assertTrue(friends.connect(BOB, "hub").join());
        assertEquals("hub", friends.player(BOB).join().orElseThrow().server());
    }

    @Test
    void messagesFollowSubjectsQueuesAndReplies() {
        InMemoryRiftNetwork network = network();
        List<String> seen = new ArrayList<>();
        Subscription all = network.messages().subscribe("party.>", m -> seen.add("all:" + m.subject())).join();
        network.messages().subscribe("party.*", m -> seen.add("one:" + m.text())).join();
        network.messages().subscribe("jobs", "workers", m -> seen.add("a")).join();
        network.messages().subscribe("jobs", "workers", m -> seen.add("b")).join();
        network.messages().publish("party.invite", "hi".getBytes(StandardCharsets.UTF_8)).join();
        network.messages().publish("party.invite.alice", new byte[0]).join();
        network.messages().publish("jobs", new byte[0]).join();
        network.messages().publish("jobs", new byte[0]).join();
        assertEquals(5, seen.size(), seen.toString());
        assertTrue(seen.containsAll(List.of("all:party.invite", "one:hi", "all:party.invite.alice", "a", "b")));
        all.close();
        all.close();

        network.messages().subscribe("echo", request -> network.messages()
                .publish(request.replyTo().orElseThrow(), request.payload()));
        assertEquals(
                "ping",
                network.messages()
                        .request("echo", "ping".getBytes(StandardCharsets.UTF_8), Duration.ofSeconds(1))
                        .join()
                        .text());
        assertEquals(
                NetworkException.Reason.UNAVAILABLE,
                failure(network.messages().request("nobody", new byte[0], Duration.ofSeconds(1))).reason());
        network.messages().subscribe("silent", request -> {});
        assertInstanceOf(
                NetworkTimeoutException.class,
                failure(network.messages().request("silent", new byte[0], Duration.ofMillis(20))));
    }

    @Test
    void installsBehindTheStaticAccessor() {
        InMemoryRiftNetwork network = network();
        network.install();
        assertSame(network, RiftNetwork.get());
        network.uninstall();
        assertThrows(IllegalStateException.class, RiftNetwork::get);
    }
}
