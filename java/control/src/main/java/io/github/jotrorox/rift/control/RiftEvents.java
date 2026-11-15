package io.github.jotrorox.rift.control;

import io.github.jotrorox.rift.messaging.Message;
import io.github.jotrorox.rift.messaging.MessagingClient;
import io.github.jotrorox.rift.messaging.Subscription;
import io.github.jotrorox.rift.permissions.client.Json;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Objects;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.function.Consumer;

/**
 * The events Rift publishes on {@code rift.events.*}, typed.
 *
 * <pre>{@code
 * RiftEvents.subscribe(bus, RiftEvents.PLAYERS, event -> {
 *     if (event instanceof RiftEvents.Player player && player.kind() == RiftEvents.Player.Kind.JOIN) {
 *         log.info(player.name() + " joined " + player.server());
 *     }
 * });
 * }</pre>
 *
 * <p>Events are ephemeral: nothing is replayed after a reconnect, so query {@link
 * RiftControl#status()} for the current state then. Subscribing needs a matching subscribe grant,
 * such as {@code rift.events.>}.
 */
public final class RiftEvents {
    /** Every Rift event, including subjects this class has no type for. */
    public static final String ALL = "rift.events.>";
    /** {@link Lifecycle}: {@code ready} after startup, {@code draining} on shutdown. */
    public static final String LIFECYCLE = "rift.events.lifecycle";
    /** {@link Reload}: a configuration reload succeeded. */
    public static final String RELOAD = "rift.events.reload";
    /** {@link Control}: a control command other than status finished. */
    public static final String CONTROL = "rift.events.control";
    /** {@link Player}: players joining, switching servers and quitting. */
    public static final String PLAYERS = "rift.events.players";
    /** {@link ServerState}: changes to what backends report about themselves. */
    public static final String SERVERS = "rift.events.servers";
    /** {@link Instance}: service group instances created and removed. */
    public static final String INSTANCES = "rift.events.instances";

    private RiftEvents() {}

    /**
     * Subscribe to events; the subscription survives reconnects. Events arrive in order on one Rift
     * thread, never on a server thread. Messages {@link #parse} has no type for are skipped.
     *
     * @param messaging the bus connection
     * @param pattern one of this class's subjects, or any pattern under {@code rift.events.}
     * @param handler called for every event
     * @return completes once the proxy accepted the subscription
     */
    public static CompletableFuture<Subscription> subscribe(
            MessagingClient messaging, String pattern, Consumer<Event> handler) {
        Objects.requireNonNull(handler, "handler");
        return messaging.subscribe(pattern, null, message -> parse(message).ifPresent(handler));
    }

    /**
     * @param message a message from a {@code rift.events.*} subject
     * @return the event, or empty for a subject without a type here or a malformed payload
     */
    public static Optional<Event> parse(Message message) {
        Json.Obj body;
        try {
            if (!(Json.parse(message.text()) instanceof Json.Obj object)) {
                return Optional.empty();
            }
            body = object;
        } catch (IllegalArgumentException e) {
            return Optional.empty();
        }
        return Optional.ofNullable(switch (message.subject()) {
            case LIFECYCLE -> new Lifecycle(Wire.string(body, "state"));
            case RELOAD -> new Reload(Wire.string(body, "revision"));
            case CONTROL -> new Control(Wire.string(body, "command"), Wire.bool(body, "ok"));
            case PLAYERS -> player(body);
            case SERVERS -> new ServerState(
                    Wire.string(body, "server"),
                    Wire.optionalString(body, "state"),
                    Wire.properties(body, "properties"),
                    Wire.bool(body, "draining"));
            case INSTANCES -> instance(body);
            default -> null;
        });
    }

    private static Player player(Json.Obj body) {
        Player.Kind kind = switch (Wire.string(body, "event")) {
            case "join" -> Player.Kind.JOIN;
            case "switch" -> Player.Kind.SWITCH;
            case "quit" -> Player.Kind.QUIT;
            case "visibility" -> Player.Kind.VISIBILITY;
            default -> null;
        };
        return kind == null
                ? null
                : new Player(
                        kind,
                        Wire.id(body, "connection_id"),
                        Wire.string(body, "uuid"),
                        Wire.string(body, "name"),
                        Wire.string(body, "server"),
                        Wire.optionalString(body, "previous"),
                        Wire.optionalString(body, "visible_to"),
                        body);
    }

    private static Instance instance(Json.Obj body) {
        String event = Wire.string(body, "event");
        return event.equals("created") || event.equals("removed")
                ? new Instance(
                        event.equals("created"), Wire.string(body, "name"), Wire.optionalString(body, "group"), body)
                : null;
    }

    /** An event published by Rift. */
    public sealed interface Event permits Lifecycle, Reload, Control, Player, ServerState, Instance {}

    /**
     * The proxy changed lifecycle state.
     *
     * @param state {@code ready} or {@code draining}
     */
    public record Lifecycle(String state) implements Event {
        /** @return whether the proxy finished starting */
        public boolean isReady() {
            return state.equals("ready");
        }

        /** @return whether the proxy is shutting down */
        public boolean isDraining() {
            return state.equals("draining");
        }
    }

    /**
     * A configuration reload was applied.
     *
     * @param revision the active configuration revision
     */
    public record Reload(String revision) implements Event {}

    /**
     * A control command finished.
     *
     * @param command the command name, such as {@code drain}
     * @param ok whether it succeeded
     */
    public record Control(String command, boolean ok) implements Event {}

    /**
     * A player joined, switched servers, quit, or changed visibility. Events for one player arrive
     * in order.
     *
     * @param kind what happened
     * @param connectionId the player's connection, as {@link ProxyStatus.Player#connectionId()}
     * @param uuid the player's UUID as 32 lowercase hex digits without hyphens
     * @param name the player name
     * @param server the server the player is on; for a quit, the last one
     * @param previous for a switch, the server the player left
     * @param visibleTo the permission node a viewer needs to see the player, or empty when visible
     *     to everyone
     * @param raw the complete event, including fields added by newer Rift versions
     */
    public record Player(
            Kind kind,
            long connectionId,
            String uuid,
            String name,
            String server,
            Optional<String> previous,
            Optional<String> visibleTo,
            Json.Obj raw)
            implements Event {
        /** What happened to the player. */
        public enum Kind {
            /** The player entered the network. */
            JOIN,
            /** The player moved to another server. */
            SWITCH,
            /** The player left the network. */
            QUIT,
            /** The node a viewer needs to see the player changed. */
            VISIBILITY
        }
    }

    /**
     * A backend's report about itself changed, or was forgotten on restart. The event carries the
     * whole report, so a subscriber can replace what it holds.
     *
     * @param server the server
     * @param state the server's label, such as {@code INGAME}, or empty without one
     * @param properties the server's properties
     * @param draining whether the server itself asked for no new players; an operator's drain is not
     *     included
     */
    public record ServerState(String server, Optional<String> state, Map<String, String> properties, boolean draining)
            implements Event {
        public ServerState {
            Objects.requireNonNull(state, "state");
            properties = Collections.unmodifiableMap(new LinkedHashMap<>(properties));
        }
    }

    /**
     * A service group instance was created or removed, by an operator or by automatic scaling.
     *
     * @param created true for a creation, false for a removal
     * @param name the instance, such as {@code lobby-1}
     * @param group its service group; only a creation names it
     * @param raw the complete event: port, address, node, template and storage for a creation;
     *     storage and whether its files were deleted for a removal
     */
    public record Instance(boolean created, String name, Optional<String> group, Json.Obj raw) implements Event {}
}
