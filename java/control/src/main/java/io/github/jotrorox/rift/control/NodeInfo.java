package io.github.jotrorox.rift.control;

import io.github.jotrorox.rift.permissions.client.Json;
import java.time.Duration;
import java.util.List;
import java.util.Objects;
import java.util.Optional;
import java.util.OptionalInt;

/**
 * One node of {@code rift.control.nodes}: a host whose agent runs servers for the controller.
 *
 * @param name the node's configured name
 * @param address the address the proxy reaches the node's servers on
 * @param labels the node's labels, which service groups select nodes by
 * @param state whether the node's agent is reporting
 * @param memoryMb the memory the node offers to servers, or empty without a limit
 * @param allocatedMemoryMb the memory of the instances placed on the node
 * @param maxInstances the most instances the node takes
 * @param instances the servers placed on the node
 * @param running the processes the agent last reported, or empty when it never connected
 * @param version the agent's Rift version, or empty when it never connected
 * @param lastSeen how long ago the agent last reported, or empty when it never connected
 * @param connected how long the agent has been reporting without a gap, or empty unless online
 * @param raw the complete node object, including fields added by newer Rift versions
 */
public record NodeInfo(
        String name,
        String address,
        List<String> labels,
        State state,
        OptionalInt memoryMb,
        int allocatedMemoryMb,
        int maxInstances,
        int instances,
        OptionalInt running,
        Optional<String> version,
        Optional<Duration> lastSeen,
        Optional<Duration> connected,
        Json.Obj raw) {
    public NodeInfo {
        labels = List.copyOf(labels);
        Objects.requireNonNull(state, "state");
        Objects.requireNonNull(raw, "raw");
    }

    /** Whether a node's agent is reporting to the controller. */
    public enum State {
        /** The agent reported within the last 10 seconds. */
        ONLINE,
        /** The agent connected once but has stopped reporting. */
        UNREACHABLE,
        /** No agent has connected for this node since the controller started. */
        NEVER_CONNECTED,
        /** A state this client does not know; see {@link NodeInfo#raw()}. */
        UNKNOWN
    }

    /** @return whether the node's agent is reporting, so servers can be started on it */
    public boolean isOnline() {
        return state == State.ONLINE;
    }

    static List<NodeInfo> parse(Json data) {
        if (!(data instanceof Json.Obj reply)) {
            throw new IllegalArgumentException("expected an object");
        }
        return Wire.objects(reply, "nodes", node -> new NodeInfo(
                Wire.string(node, "name"),
                Wire.string(node, "address"),
                Wire.strings(node, "labels"),
                switch (Wire.string(node, "state")) {
                    case "online" -> State.ONLINE;
                    case "unreachable" -> State.UNREACHABLE;
                    case "never_connected" -> State.NEVER_CONNECTED;
                    default -> State.UNKNOWN;
                },
                optionalInt(node, "memory_mb"),
                (int) Wire.number(node, "allocated_memory_mb"),
                (int) Wire.number(node, "max_instances"),
                (int) Wire.number(node, "instances"),
                optionalInt(node, "running"),
                Wire.optionalString(node, "version"),
                Wire.optionalNumber(node, "last_seen_ms").map(Duration::ofMillis),
                Wire.optionalNumber(node, "connected_ms").map(Duration::ofMillis),
                node));
    }

    private static OptionalInt optionalInt(Json.Obj object, String name) {
        return Wire.optionalNumber(object, name)
                .map(value -> OptionalInt.of(value.intValue()))
                .orElseGet(OptionalInt::empty);
    }
}
