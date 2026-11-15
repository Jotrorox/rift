package io.github.jotrorox.rift.control;

import io.github.jotrorox.rift.messaging.MessagingClient;
import io.github.jotrorox.rift.messaging.MessagingException;
import io.github.jotrorox.rift.permissions.client.Json;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;

/**
 * Operates the proxy through Rift's {@code rift.control.*} subjects.
 *
 * <pre>{@code
 * MessagingClient bus = MessagingClient.builder().fromEnvironment().build();
 * RiftControl control = new RiftControl(bus);
 * control.status().thenAccept(status -> log.info(status.playersOnline() + " players online"));
 * control.drain("survival", true);
 * }</pre>
 *
 * <p>The login needs {@code control = true} in Rift's configuration and a publish grant for the
 * subjects it uses, for example {@code rift.control.*}. Every call returns a future that completes
 * on a Rift thread. It fails with {@link ControlException} when Rift refused the command, with
 * {@link java.util.concurrent.TimeoutException} when no reply arrived in time, and with another
 * {@link MessagingException} when the bus did (no connection, a missing grant).
 *
 * <p>Rift runs control commands one at a time and never retries them. A command that timed out may
 * still have run: check {@link #status()} before repeating one that changes something.
 *
 * <p>This class does not own the {@link MessagingClient}: close that when done.
 */
public final class RiftControl {
    /** The default wait for a reply; Rift itself gives up on a command after 40 seconds. */
    public static final Duration DEFAULT_TIMEOUT = Duration.ofSeconds(45);

    private final MessagingClient messaging;
    private final Duration timeout;

    /** @param messaging the bus connection, which stays the caller's to close */
    public RiftControl(MessagingClient messaging) {
        this(messaging, DEFAULT_TIMEOUT);
    }

    private RiftControl(MessagingClient messaging, Duration timeout) {
        this.messaging = Objects.requireNonNull(messaging, "messaging");
        this.timeout = timeout;
    }

    /**
     * @param timeout how long each call waits for Rift's reply, 1 ms to 120 s. {@code create},
     *     {@code remove} and {@code download} through {@link #execute} can take longer than that
     *     when they download server software; follow {@link RiftEvents#INSTANCES} instead of waiting
     * @return a view of this API with that timeout
     */
    public RiftControl withTimeout(Duration timeout) {
        long millis = Objects.requireNonNull(timeout, "timeout").toMillis();
        if (millis < 1 || millis > 120_000) {
            throw new IllegalArgumentException("timeout must be 1 to 120000 ms");
        }
        return new RiftControl(messaging, timeout);
    }

    /** @return connections, players, backends and the maintenance state */
    public CompletableFuture<ProxyStatus> status() {
        return execute("status", Map.of()).thenApply(data -> read("status", data, json -> {
            if (json instanceof Json.Obj object) {
                return ProxyStatus.parse(object);
            }
            throw new IllegalArgumentException("expected an object");
        }));
    }

    /**
     * Enable or disable maintenance, which refuses new logins.
     *
     * @param enabled the wanted state
     * @return the maintenance state Rift reports afterwards
     */
    public CompletableFuture<Boolean> maintenance(boolean enabled) {
        return execute("maintenance", Map.of("enabled", new Json.Bool(enabled)))
                .thenApply(data -> flag(data, "maintenance", enabled));
    }

    /**
     * Enable or disable draining, which keeps new players away from a backend. Turning it off also
     * withdraws a drain the server asked for itself.
     *
     * @param backend a backend name
     * @param enabled the wanted state
     * @return the draining state Rift reports afterwards
     */
    public CompletableFuture<Boolean> drain(String backend, boolean enabled) {
        return execute("drain", Map.of("backend", backend(backend), "enabled", new Json.Bool(enabled)))
                .thenApply(data -> flag(data, "draining", enabled));
    }

    /**
     * Move an online player to a backend, with the same access, health and protocol checks as other
     * transfers.
     *
     * @param connectionId the player's {@link ProxyStatus.Player#connectionId()}
     * @param backend a backend name
     * @return Rift's description of the completed transfer
     */
    public CompletableFuture<Json> transfer(long connectionId, String backend) {
        if (connectionId <= 0) {
            throw new IllegalArgumentException("connectionId must be positive");
        }
        return execute("transfer", Map.of("connection_id", Json.Num.of(connectionId), "backend", backend(backend)));
    }

    /**
     * Move the online player of that name to a backend: a {@link #status()} call to find the
     * connection, then {@link #transfer(long, String)}.
     *
     * @param playerName a player name; ASCII case is ignored
     * @param backend a backend name
     * @return Rift's description of the completed transfer; fails with {@link ControlException} when
     *     no such player is online
     */
    public CompletableFuture<Json> transfer(String playerName, String backend) {
        Objects.requireNonNull(playerName, "playerName");
        Json.Str target = backend(backend);
        return status().thenCompose(status -> status.player(playerName)
                .map(player -> execute(
                        "transfer",
                        Map.of("connection_id", Json.Num.of(player.connectionId()), "backend", target)))
                .orElseGet(() -> CompletableFuture.failedFuture(
                        new ControlException("transfer", "player " + playerName + " is not online"))));
    }

    /**
     * Reload Rift's configuration file. An invalid configuration is refused and the running one
     * stays active.
     *
     * @return Rift's description of the applied configuration
     */
    public CompletableFuture<Json> reload() {
        return execute("reload", Map.of());
    }

    /**
     * The configured nodes with their reachability and load.
     *
     * @return every node, sorted by name; empty on a proxy without nodes
     */
    public CompletableFuture<List<NodeInfo>> nodes() {
        return execute("nodes", Map.of()).thenApply(data -> read("nodes", data, NodeInfo::parse));
    }

    /**
     * Send a request to {@code rift.control.<command>} and return the {@code data} of a successful
     * reply. This reaches every control subject, including those without a method here ({@code
     * servers}, {@code groups}, {@code create}, {@code plugins}, …) and those newer Rift versions
     * add.
     *
     * @param command the command name, the last token of its subject
     * @param arguments the members of the request object; empty for a command without arguments
     * @return the reply's {@code data}, or {@link Json.Null} when it has none; fails with {@link
     *     ControlException} when Rift refused the command
     */
    public CompletableFuture<Json> execute(String command, Map<String, Json> arguments) {
        String subject = subject(command);
        byte[] payload = new Json.Obj(new LinkedHashMap<>(arguments)).toString().getBytes(StandardCharsets.UTF_8);
        return messaging.request(subject, payload, timeout).thenApply(reply -> {
            try {
                return unwrap(command, reply.payload());
            } catch (MessagingException e) {
                throw new CompletionException(e);
            }
        });
    }

    /**
     * @return the {@code data} of a successful reply
     * @throws ControlException when Rift refused the command
     * @throws MessagingException when the reply is not a control reply
     */
    static Json unwrap(String command, byte[] payload) throws MessagingException {
        Json.Obj response;
        try {
            if (!(Json.parse(new String(payload, StandardCharsets.UTF_8)) instanceof Json.Obj object)) {
                throw new IllegalArgumentException("expected an object");
            }
            response = object;
        } catch (IllegalArgumentException e) {
            throw invalid(command, e);
        }
        if (response.get("ok") instanceof Json.Bool ok && ok.value()) {
            Json data = response.get("data");
            return data == null ? Json.Null.INSTANCE : data;
        }
        throw new ControlException(
                command, response.get("error") instanceof Json.Str error ? error.value() : "command failed");
    }

    private interface Reader<T> {
        T read(Json data);
    }

    private static <T> T read(String command, Json data, Reader<T> reader) {
        try {
            return reader.read(data);
        } catch (IllegalArgumentException e) {
            throw new CompletionException(invalid(command, e));
        }
    }

    private static MessagingException invalid(String command, IllegalArgumentException cause) {
        MessagingException error = new MessagingException(
                "invalid control reply for " + command + ": " + cause.getMessage(), MessagingException.CLIENT);
        error.initCause(cause);
        return error;
    }

    /** The state Rift reports, or the requested one from a Rift that reports none. */
    private static boolean flag(Json data, String name, boolean requested) {
        return data instanceof Json.Obj object && object.get(name) instanceof Json.Bool value
                ? value.value()
                : requested;
    }

    static String subject(String command) {
        if (command == null
                || command.isEmpty()
                || command.contains(".")
                || command.contains("*")
                || command.contains(">")
                || command.chars().anyMatch(Character::isWhitespace)) {
            throw new IllegalArgumentException("invalid control command: " + command);
        }
        return "rift.control." + command;
    }

    private static Json.Str backend(String backend) {
        if (backend == null || backend.isEmpty() || backend.getBytes(StandardCharsets.UTF_8).length > 256) {
            throw new IllegalArgumentException("backend must be a nonempty name of at most 256 bytes");
        }
        return new Json.Str(backend);
    }
}
