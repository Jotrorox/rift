package io.github.jotrorox.rift.network.client;

import io.github.jotrorox.rift.messaging.Message;
import io.github.jotrorox.rift.messaging.MessagingClient;
import io.github.jotrorox.rift.messaging.MessagingException;
import io.github.jotrorox.rift.network.BusMessage;
import io.github.jotrorox.rift.network.Messenger;
import io.github.jotrorox.rift.network.NetworkException;
import io.github.jotrorox.rift.network.NetworkException.Reason;
import io.github.jotrorox.rift.network.NetworkPlayer;
import io.github.jotrorox.rift.network.NetworkTimeoutException;
import io.github.jotrorox.rift.network.RiftNetwork;
import io.github.jotrorox.rift.network.ServerInfo;
import io.github.jotrorox.rift.network.Subscription;
import io.github.jotrorox.rift.permissions.client.Json;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.Optional;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.function.Consumer;
import java.util.function.Function;

/**
 * {@link RiftNetwork} over the messaging bus: the proxy's {@code rift.network.*} requests, and the
 * bus itself as a {@link Messenger}. The login needs publish access to {@code rift.network.*},
 * which the {@code backend} preset includes.
 *
 * <pre>{@code
 * MessagingClient bus = MessagingClient.builder().fromEnvironment().build();
 * RiftNetwork network = NetworkClient.builder().messaging(bus).fromEnvironment().build();
 * }</pre>
 *
 * <p>The client does not own the {@link MessagingClient}: close that when done.
 */
public final class NetworkClient implements RiftNetwork {
    private static final String SERVERS = "rift.network.servers";
    private static final String PLAYERS = "rift.network.players";
    private static final String PLAYER = "rift.network.player";
    private static final String CONNECT = "rift.network.connect";

    private final MessagingClient messaging;
    private final String server;
    private final String group;
    private final Duration requestTimeout;
    private final Messenger messenger = new BusMessenger();

    private NetworkClient(Builder builder) {
        this.messaging = Objects.requireNonNull(builder.messaging, "messaging");
        this.server = Objects.requireNonNull(builder.server, "server");
        this.group = builder.group;
        this.requestTimeout = builder.requestTimeout;
    }

    /** @return a builder */
    public static Builder builder() {
        return new Builder();
    }

    @Override
    public String localServer() {
        return server;
    }

    @Override
    public Optional<String> localGroup() {
        return Optional.ofNullable(group);
    }

    @Override
    public CompletableFuture<List<ServerInfo>> servers() {
        return ask(SERVERS, Map.of(), Wire::servers);
    }

    @Override
    public CompletableFuture<List<ServerInfo>> servers(String serverOrGroup) {
        return ask(SERVERS, Map.of("server", name(serverOrGroup, "serverOrGroup")), Wire::servers);
    }

    @Override
    public CompletableFuture<List<NetworkPlayer>> players() {
        return ask(PLAYERS, Map.of(), Wire::players);
    }

    @Override
    public CompletableFuture<List<NetworkPlayer>> players(String serverOrGroup) {
        return ask(PLAYERS, Map.of("server", name(serverOrGroup, "serverOrGroup")), Wire::players);
    }

    @Override
    public CompletableFuture<Optional<NetworkPlayer>> player(UUID uuid) {
        return ask(PLAYER, Map.of("player", Wire.hex(uuid)), Wire::optionalPlayer);
    }

    @Override
    public CompletableFuture<Optional<NetworkPlayer>> player(String name) {
        return ask(PLAYER, Map.of("player", name(name, "name")), Wire::optionalPlayer);
    }

    @Override
    public CompletableFuture<Boolean> connect(UUID player, String server) {
        return ask(CONNECT, Map.of("player", Wire.hex(player), "server", name(server, "server")), Wire::bool);
    }

    @Override
    public CompletableFuture<Boolean> connectToGroup(UUID player, String group) {
        return ask(CONNECT, Map.of("player", Wire.hex(player), "group", name(group, "group")), Wire::bool);
    }

    @Override
    public Messenger messages() {
        return messenger;
    }

    private static String name(String value, String what) {
        Objects.requireNonNull(value, what);
        if (value.isEmpty()) {
            throw new IllegalArgumentException(what + " must not be empty");
        }
        return value;
    }

    /** One {@code rift.network} request with the reply envelope. */
    private <T> CompletableFuture<T> ask(String subject, Map<String, String> fields, Function<Json, T> data) {
        Map<String, Json> members = new LinkedHashMap<>();
        fields.forEach((key, value) -> members.put(key, new Json.Str(value)));
        byte[] payload = new Json.Obj(members).toString().getBytes(StandardCharsets.UTF_8);
        return send(subject, payload, requestTimeout).thenApply(message -> {
            try {
                Json.Obj reply = Wire.object(Json.parse(message.text()));
                if (reply.get("ok") instanceof Json.Bool ok && ok.value()) {
                    return data.apply(reply.get("data"));
                }
                String error = reply.get("error") instanceof Json.Str text ? text.value() : "the proxy refused";
                throw new NetworkException(Reason.REFUSED, error);
            } catch (IllegalArgumentException e) {
                throw new NetworkException(
                        Reason.INVALID, "malformed reply on " + subject + ": " + e.getMessage(), e);
            }
        });
    }

    /** A bus request whose failures are this API's exceptions, never the bus client's. */
    private CompletableFuture<Message> send(String subject, byte[] payload, Duration timeout) {
        CompletableFuture<Message> sent;
        try {
            sent = messaging.request(subject, payload, timeout).orTimeout(timeout.toMillis(), TimeUnit.MILLISECONDS);
        } catch (IllegalArgumentException e) {
            return CompletableFuture.failedFuture(new NetworkException(Reason.INVALID, e.getMessage(), e));
        }
        return translated(sent, "no reply on " + subject + " within " + timeout.toMillis() + " ms", subject);
    }

    private static <T> CompletableFuture<T> translated(CompletableFuture<T> future, String timedOut, String subject) {
        return future.handle((value, error) -> {
            if (error == null) {
                return value;
            }
            Throwable cause = error;
            while (cause instanceof CompletionException && cause.getCause() != null) {
                cause = cause.getCause();
            }
            throw switch (cause) {
                case NetworkException e -> e;
                case TimeoutException e -> new NetworkTimeoutException(timedOut);
                case MessagingException e when e.code() == MessagingException.PERMISSION -> new NetworkException(
                        Reason.DENIED,
                        "this server's bus login has no grant for " + subject + ": " + e.getMessage(),
                        e);
                case MessagingException e -> new NetworkException(Reason.UNAVAILABLE, e.getMessage(), e);
                case IllegalArgumentException e -> new NetworkException(Reason.INVALID, e.getMessage(), e);
                default -> new NetworkException(Reason.UNAVAILABLE, String.valueOf(cause.getMessage()), cause);
            };
        });
    }

    private final class BusMessenger implements Messenger {
        @Override
        public CompletableFuture<Void> publish(String subject, byte[] payload) {
            Objects.requireNonNull(payload, "payload");
            CompletableFuture<Void> sent;
            try {
                sent = messaging.publish(name(subject, "subject"), payload);
            } catch (IllegalArgumentException e) {
                return CompletableFuture.failedFuture(new NetworkException(Reason.INVALID, e.getMessage(), e));
            }
            return translated(sent, "publishing to " + subject + " timed out", subject);
        }

        @Override
        public CompletableFuture<BusMessage> request(String subject, byte[] payload, Duration timeout) {
            Objects.requireNonNull(payload, "payload");
            Objects.requireNonNull(timeout, "timeout");
            return send(name(subject, "subject"), payload, timeout).thenApply(NetworkClient::message);
        }

        @Override
        public CompletableFuture<Subscription> subscribe(String pattern, Consumer<BusMessage> handler) {
            return subscribe(pattern, null, handler);
        }

        @Override
        public CompletableFuture<Subscription> subscribe(String pattern, String queue, Consumer<BusMessage> handler) {
            Objects.requireNonNull(handler, "handler");
            CompletableFuture<Subscription> subscribed;
            try {
                subscribed = messaging
                        .subscribe(name(pattern, "pattern"), queue, message -> handler.accept(message(message)))
                        .thenApply(subscription -> subscription::close);
            } catch (IllegalArgumentException e) {
                return CompletableFuture.failedFuture(new NetworkException(Reason.INVALID, e.getMessage(), e));
            }
            return translated(subscribed, "subscribing to " + pattern + " timed out", pattern);
        }
    }

    private static BusMessage message(Message message) {
        return new BusMessage(message.subject(), Optional.ofNullable(message.reply()), message.payload());
    }

    /** Builds a {@link NetworkClient}. */
    public static final class Builder {
        private MessagingClient messaging;
        private String server;
        private String group;
        private Duration requestTimeout = Duration.ofSeconds(5);

        private Builder() {}

        /**
         * @param messaging the bus connection, which stays the caller's to close
         * @return this
         */
        public Builder messaging(MessagingClient messaging) {
            this.messaging = Objects.requireNonNull(messaging, "messaging");
            return this;
        }

        /**
         * @param server this server's name on the proxy
         * @return this
         */
        public Builder server(String server) {
            this.server = name(server, "server");
            return this;
        }

        /**
         * @param group the service group this server is an instance of, or null
         * @return this
         */
        public Builder group(String group) {
            this.group = group == null || group.isBlank() ? null : group;
            return this;
        }

        /**
         * Take the server and group from {@code RIFT_SERVER_NAME} and {@code RIFT_SERVER_GROUP},
         * which Rift sets for the servers it manages.
         *
         * @return this
         * @throws IllegalStateException when {@code RIFT_SERVER_NAME} is unset
         */
        public Builder fromEnvironment() {
            return fromEnvironment(System::getenv);
        }

        /**
         * @param variables an environment variable's value, or null
         * @return this
         * @throws IllegalStateException when {@code RIFT_SERVER_NAME} is unset
         */
        public Builder fromEnvironment(Function<String, String> variables) {
            String name = variables.apply("RIFT_SERVER_NAME");
            if (name == null || name.isBlank()) {
                throw new IllegalStateException(
                        "RIFT_SERVER_NAME is not set; Rift sets it for managed servers, otherwise call server(...)");
            }
            return server(name).group(variables.apply("RIFT_SERVER_GROUP"));
        }

        /**
         * @param timeout how long {@link RiftNetwork} calls wait for the proxy, 1 ms to 120 s; 5 s
         *     by default
         * @return this
         */
        public Builder requestTimeout(Duration timeout) {
            this.requestTimeout = Objects.requireNonNull(timeout, "timeout");
            return this;
        }

        /** @return the client */
        public NetworkClient build() {
            return new NetworkClient(this);
        }
    }
}
