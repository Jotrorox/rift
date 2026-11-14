package io.github.jotrorox.rift.network.test;

import io.github.jotrorox.rift.network.BusMessage;
import io.github.jotrorox.rift.network.Messenger;
import io.github.jotrorox.rift.network.NetworkException;
import io.github.jotrorox.rift.network.NetworkException.Reason;
import io.github.jotrorox.rift.network.NetworkPlayer;
import io.github.jotrorox.rift.network.NetworkTimeoutException;
import io.github.jotrorox.rift.network.RiftNetwork;
import io.github.jotrorox.rift.network.RiftNetworkProvider;
import io.github.jotrorox.rift.network.ServerInfo;
import io.github.jotrorox.rift.network.ServerState;
import io.github.jotrorox.rift.network.Subscription;
import java.time.Duration;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.Optional;
import java.util.OptionalInt;
import java.util.TreeMap;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;
import java.util.function.Consumer;

/**
 * {@link RiftNetwork} in memory, for plugin tests: no proxy and no network. Futures are already
 * complete and handlers run on the calling thread.
 *
 * <pre>{@code
 * InMemoryRiftNetwork network = InMemoryRiftNetwork.builder("lobby-1", "lobby")
 *         .server("lobby-1", "lobby", 50)
 *         .server("bedwars-1", "bedwars", 16)
 *         .player(alice, "Alice", "lobby-1")
 *         .build();
 * network.connectToGroup(alice, "bedwars").join();
 * assertEquals("bedwars-1", network.player(alice).join().orElseThrow().server());
 * }</pre>
 *
 * <p>A transfer takes effect at once, to the named server or to the running instance of the group
 * with the fewest players; like the proxy, it answers {@code true} and leaves the player in place
 * when no server can take them. Player counts follow the players, hidden ones excluded. As for a
 * backend's bus login, only players on {@link #localServer()} can be moved unless the builder
 * allowed {@link Builder#connectAnyone() anyone}.
 */
public final class InMemoryRiftNetwork implements RiftNetwork {
    private record Server(
            String name, String group, int maxPlayers, ServerState state, String serviceState, Map<String, String> properties) {
        Server(String name, String group, int maxPlayers, ServerState state) {
            this(name, group, maxPlayers, state, null, Map.of());
        }
    }

    private record Subscriber(String[] pattern, String queue, Consumer<BusMessage> handler) {}

    private final String localServer;
    private final String localGroup;
    private final boolean connectAnyone;
    private final Map<String, Server> servers = new TreeMap<>();
    private final Map<UUID, NetworkPlayer> players = new LinkedHashMap<>();
    private final List<Subscriber> subscribers = new CopyOnWriteArrayList<>();
    private final AtomicLong inboxes = new AtomicLong();
    private final AtomicLong rotation = new AtomicLong();
    private final Messenger messenger = new Bus();

    private InMemoryRiftNetwork(Builder builder) {
        this.localServer = builder.localServer;
        this.localGroup = builder.localGroup;
        this.connectAnyone = builder.connectAnyone;
        this.servers.putAll(builder.servers);
        this.players.putAll(builder.players);
    }

    /**
     * @param localServer the name of the server under test
     * @return a builder
     */
    public static Builder builder(String localServer) {
        return new Builder(localServer, null);
    }

    /**
     * @param localServer the name of the server under test
     * @param localGroup the service group it is an instance of
     * @return a builder
     */
    public static Builder builder(String localServer, String localGroup) {
        return new Builder(localServer, Objects.requireNonNull(localGroup, "localGroup"));
    }

    /** Make this the network {@link RiftNetwork#get()} returns. */
    public void install() {
        RiftNetworkProvider.register(this);
    }

    /** Undo {@link #install()}. */
    public void uninstall() {
        RiftNetworkProvider.unregister(this);
    }

    /**
     * Add or replace a server that is up.
     *
     * @param name the server
     * @param group its service group, or null
     * @param maxPlayers its player limit, or 0 for none
     * @param state its state
     */
    public synchronized void setServer(String name, String group, int maxPlayers, ServerState state) {
        servers.put(name, new Server(name, group, maxPlayers, Objects.requireNonNull(state, "state")));
    }

    /**
     * Set what a server reports about itself, as it would on {@code rift.server.state}.
     *
     * @param name a server added before
     * @param serviceState its label, such as {@code INGAME}, or null for none
     * @param properties its properties
     */
    public synchronized void setServiceState(String name, String serviceState, Map<String, String> properties) {
        Server server = servers.get(name);
        if (server == null) {
            throw new IllegalArgumentException("unknown server " + name);
        }
        servers.put(
                name,
                new Server(name, server.group(), server.maxPlayers(), server.state(), serviceState, Map.copyOf(properties)));
    }

    /**
     * Take a server down. Its players stay listed until moved or removed.
     *
     * @param name the server
     */
    public synchronized void removeServer(String name) {
        servers.remove(name);
    }

    /**
     * Add a player, or move one.
     *
     * @param uuid the player
     * @param name their name
     * @param server the server they are on
     */
    public synchronized void setPlayer(UUID uuid, String name, String server) {
        players.put(uuid, new NetworkPlayer(uuid, name, server, Optional.empty()));
    }

    /**
     * Add a player that a visibility rule hides.
     *
     * @param uuid the player
     * @param name their name
     * @param server the server they are on
     * @param visibleTo the node a viewer needs to see them
     */
    public synchronized void setHiddenPlayer(UUID uuid, String name, String server, String visibleTo) {
        players.put(uuid, new NetworkPlayer(uuid, name, server, Optional.of(visibleTo)));
    }

    /**
     * Take a player offline.
     *
     * @param uuid the player
     */
    public synchronized void removePlayer(UUID uuid) {
        players.remove(uuid);
    }

    @Override
    public String localServer() {
        return localServer;
    }

    @Override
    public Optional<String> localGroup() {
        return Optional.ofNullable(localGroup);
    }

    @Override
    public synchronized CompletableFuture<List<ServerInfo>> servers() {
        return CompletableFuture.completedFuture(servers.values().stream().map(this::info).toList());
    }

    @Override
    public synchronized CompletableFuture<List<ServerInfo>> servers(String serverOrGroup) {
        return selection(serverOrGroup)
                .map(names -> CompletableFuture.completedFuture(
                        servers.values().stream().filter(names).map(this::info).toList()))
                .orElseGet(InMemoryRiftNetwork::unknown);
    }

    @Override
    public synchronized CompletableFuture<List<NetworkPlayer>> players() {
        return CompletableFuture.completedFuture(List.copyOf(players.values()));
    }

    @Override
    public synchronized CompletableFuture<List<NetworkPlayer>> players(String serverOrGroup) {
        return selection(serverOrGroup)
                .map(names -> CompletableFuture.completedFuture(players.values().stream()
                        .filter(player -> servers.containsKey(player.server()) && names.test(servers.get(player.server())))
                        .toList()))
                .orElseGet(InMemoryRiftNetwork::unknown);
    }

    @Override
    public synchronized CompletableFuture<Optional<NetworkPlayer>> player(UUID uuid) {
        return CompletableFuture.completedFuture(Optional.ofNullable(players.get(uuid)));
    }

    @Override
    public synchronized CompletableFuture<Optional<NetworkPlayer>> player(String name) {
        return CompletableFuture.completedFuture(players.values().stream()
                .filter(player -> player.name().equalsIgnoreCase(name))
                .findFirst());
    }

    @Override
    public synchronized CompletableFuture<Boolean> connect(UUID player, String server) {
        if (selection(server).isEmpty()) {
            return unknown();
        }
        return move(player, server);
    }

    @Override
    public synchronized CompletableFuture<Boolean> connectToGroup(UUID player, String group) {
        if (servers.values().stream().noneMatch(server -> group.equals(server.group()))) {
            return CompletableFuture.failedFuture(new NetworkException(Reason.REFUSED, "unknown service group"));
        }
        return move(player, group);
    }

    @Override
    public Messenger messages() {
        return messenger;
    }

    private CompletableFuture<Boolean> move(UUID uuid, String target) {
        NetworkPlayer player = players.get(uuid);
        if (player == null) {
            return CompletableFuture.completedFuture(false);
        }
        if (!connectAnyone && !player.server().equals(localServer)) {
            return CompletableFuture.failedFuture(new NetworkException(
                    Reason.REFUSED,
                    "this login may only connect players on its own server; others need players = true"));
        }
        servers.values().stream()
                .filter(server -> server.name().equals(target) || target.equals(server.group()))
                .filter(server -> server.state() == ServerState.RUNNING)
                .filter(server -> server.maxPlayers() == 0 || count(server.name()) < server.maxPlayers())
                .min(Comparator.comparingInt((Server server) -> count(server.name())))
                .ifPresent(server -> players.put(
                        uuid, new NetworkPlayer(uuid, player.name(), server.name(), player.visibleTo())));
        return CompletableFuture.completedFuture(true);
    }

    /** A test for the servers a server or group name selects, or empty when it names neither. */
    private Optional<java.util.function.Predicate<Server>> selection(String serverOrGroup) {
        Objects.requireNonNull(serverOrGroup, "serverOrGroup");
        boolean group = servers.values().stream().anyMatch(server -> serverOrGroup.equalsIgnoreCase(server.group()));
        boolean single = servers.keySet().stream().anyMatch(serverOrGroup::equalsIgnoreCase);
        if (!group && !single) {
            return Optional.empty();
        }
        return Optional.of(server -> group
                ? serverOrGroup.equalsIgnoreCase(server.group())
                : serverOrGroup.equalsIgnoreCase(server.name()));
    }

    private static <T> CompletableFuture<T> unknown() {
        return CompletableFuture.failedFuture(new NetworkException(Reason.REFUSED, "unknown server or service group"));
    }

    private int count(String server) {
        return (int) players.values().stream()
                .filter(player -> player.server().equals(server) && !player.hidden())
                .count();
    }

    private ServerInfo info(Server server) {
        return new ServerInfo(
                server.name(),
                Optional.ofNullable(server.group()),
                server.state(),
                Optional.ofNullable(server.serviceState()),
                server.properties(),
                count(server.name()),
                server.maxPlayers() == 0 ? OptionalInt.empty() : OptionalInt.of(server.maxPlayers()));
    }

    /** A subject broker with the bus's matching rules, queue groups and request/reply. */
    private final class Bus implements Messenger {
        @Override
        public CompletableFuture<Void> publish(String subject, byte[] payload) {
            deliver(new BusMessage(subject, Optional.empty(), payload.clone()));
            return CompletableFuture.completedFuture(null);
        }

        @Override
        public CompletableFuture<BusMessage> request(String subject, byte[] payload, Duration timeout) {
            String inbox = "_INBOX.test." + inboxes.incrementAndGet();
            CompletableFuture<BusMessage> reply = new CompletableFuture<>();
            Subscriber waiting = new Subscriber(inbox.split("\\."), null, reply::complete);
            subscribers.add(waiting);
            reply.whenComplete((message, error) -> subscribers.remove(waiting));
            if (deliver(new BusMessage(subject, Optional.of(inbox), payload.clone())) == 0) {
                reply.completeExceptionally(new NetworkException(Reason.UNAVAILABLE, "no responders available"));
            }
            return reply.orTimeout(timeout.toMillis(), TimeUnit.MILLISECONDS).exceptionallyCompose(error ->
                    CompletableFuture.failedFuture(error instanceof java.util.concurrent.TimeoutException
                            ? new NetworkTimeoutException(
                                    "no reply on " + subject + " within " + timeout.toMillis() + " ms")
                            : error));
        }

        @Override
        public CompletableFuture<Subscription> subscribe(String pattern, Consumer<BusMessage> handler) {
            return subscribe(pattern, null, handler);
        }

        @Override
        public CompletableFuture<Subscription> subscribe(String pattern, String queue, Consumer<BusMessage> handler) {
            Subscriber subscriber =
                    new Subscriber(pattern.split("\\."), queue, Objects.requireNonNull(handler, "handler"));
            subscribers.add(subscriber);
            return CompletableFuture.completedFuture(() -> subscribers.remove(subscriber));
        }

        /** Every plain subscriber gets a copy; each queue group gets one, its members taking turns. */
        private int deliver(BusMessage message) {
            String[] subject = message.subject().split("\\.");
            Map<String, List<Subscriber>> queues = new LinkedHashMap<>();
            List<Subscriber> receivers = new ArrayList<>();
            for (Subscriber subscriber : subscribers) {
                if (!matches(subscriber.pattern(), subject)) {
                    continue;
                }
                if (subscriber.queue() == null) {
                    receivers.add(subscriber);
                } else {
                    queues.computeIfAbsent(subscriber.queue(), queue -> new ArrayList<>()).add(subscriber);
                }
            }
            long turn = rotation.getAndIncrement();
            queues.values().forEach(members -> receivers.add(members.get((int) (turn % members.size()))));
            receivers.forEach(receiver -> receiver.handler().accept(message));
            return receivers.size();
        }

        private static boolean matches(String[] pattern, String[] subject) {
            for (int i = 0; i < pattern.length; i++) {
                if (pattern[i].equals(">")) {
                    return i == pattern.length - 1 && subject.length > i;
                }
                if (i >= subject.length || !(pattern[i].equals("*") || pattern[i].equals(subject[i]))) {
                    return false;
                }
            }
            return pattern.length == subject.length;
        }
    }

    /** Builds an {@link InMemoryRiftNetwork}. */
    public static final class Builder {
        private final String localServer;
        private final String localGroup;
        private final Map<String, Server> servers = new TreeMap<>();
        private final Map<UUID, NetworkPlayer> players = new LinkedHashMap<>();
        private boolean connectAnyone;

        private Builder(String localServer, String localGroup) {
            this.localServer = Objects.requireNonNull(localServer, "localServer");
            this.localGroup = localGroup;
        }

        /**
         * @param name a running server outside any group, without a player limit
         * @return this
         */
        public Builder server(String name) {
            return server(name, null, 0);
        }

        /**
         * @param name a running server
         * @param group its service group, or null
         * @param maxPlayers its player limit, or 0 for none
         * @return this
         */
        public Builder server(String name, String group, int maxPlayers) {
            return server(name, group, maxPlayers, ServerState.RUNNING);
        }

        /**
         * @param name a server that is up
         * @param group its service group, or null
         * @param maxPlayers its player limit, or 0 for none
         * @param state its state
         * @return this
         */
        public Builder server(String name, String group, int maxPlayers, ServerState state) {
            servers.put(name, new Server(name, group, maxPlayers, Objects.requireNonNull(state, "state")));
            return this;
        }

        /**
         * @param uuid an online player
         * @param name their name
         * @param server the server they are on
         * @return this
         */
        public Builder player(UUID uuid, String name, String server) {
            players.put(uuid, new NetworkPlayer(uuid, name, server, Optional.empty()));
            return this;
        }

        /**
         * Let {@code connect} move players on other servers, as a bus login with {@code players =
         * true} may.
         *
         * @return this
         */
        public Builder connectAnyone() {
            this.connectAnyone = true;
            return this;
        }

        /** @return the network */
        public InMemoryRiftNetwork build() {
            return new InMemoryRiftNetwork(this);
        }
    }
}
