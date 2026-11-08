package io.github.jotrorox.rift.permissions.client;

import io.github.jotrorox.rift.permissions.Change;
import io.github.jotrorox.rift.permissions.CompiledSubject;
import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.ErrorCode;
import io.github.jotrorox.rift.permissions.GroupInfo;
import io.github.jotrorox.rift.permissions.Uuids;
import java.util.List;
import java.util.Objects;
import java.util.Optional;
import java.util.UUID;
import java.util.regex.Pattern;

/**
 * Bus subjects and messages of the permission protocol, version {@link #VERSION}
 * ({@code docs/permissions-protocol.md}). {@link Codec} maps them to and from JSON.
 */
public final class Protocol {
    /** The protocol version announced in the {@code hello} reply. */
    public static final int VERSION = 1;

    /** Backend → proxy, REQUEST {@link HelloRequest} → {@link HelloReply}. */
    public static final String HELLO = "rift.perms.hello";
    /** Backend → proxy, REQUEST {@link SubjectGetRequest} → {@link CompiledSubject}. */
    public static final String SUBJECT_GET = "rift.perms.subject.get";
    /** Backend → proxy, REQUEST {@link SubjectBatchRequest} → {@link SubjectBatchReply}. */
    public static final String SUBJECT_BATCH = "rift.perms.subject.batch";
    /** Backend → proxy, REQUEST {@link GroupsGetRequest} → {@link GroupsReply}. */
    public static final String GROUPS_GET = "rift.perms.groups.get";
    /** Backend → proxy, REQUEST {@link ApplyRequest} → {@link io.github.jotrorox.rift.permissions.Applied}. */
    public static final String APPLY = "rift.perms.apply";
    /** Proxy → all, PUBLISH a change event after every committed write. */
    public static final String UPDATE = "rift.perms.update";
    /** Prefix of {@code rift.perms.update.<uuid>}: proxy → all, PUBLISH a compiled subject. */
    public static final String UPDATE_PREFIX = "rift.perms.update.";
    /** Subscription pattern for every pushed snapshot. */
    public static final String UPDATE_ALL = "rift.perms.update.>";
    /** Backend → proxy, PUBLISH_ACK a list of {@link RegisteredPermission}. */
    public static final String REGISTER = "rift.perms.register";
    /** Backend → proxy, REQUEST {@link CommandRequest} → {@link CommandReply}. */
    public static final String COMMAND = "rift.perms.command";
    /** Backend → proxy, PUBLISH {@link ContextReport}. */
    public static final String CONTEXT = "rift.perms.context";

    private Protocol() {}

    /**
     * @param uuid a player
     * @return {@code rift.perms.update.<uuid>}
     */
    public static String updateSubject(UUID uuid) {
        return UPDATE_PREFIX + Uuids.toHex(uuid);
    }

    /**
     * @param subject a delivered subject
     * @return the player of a {@code rift.perms.update.<uuid>} subject
     */
    public static Optional<UUID> parseUpdateSubject(String subject) {
        if (!subject.startsWith(UPDATE_PREFIX) || subject.length() != UPDATE_PREFIX.length() + 32) {
            return Optional.empty();
        }
        try {
            return Optional.of(Uuids.parse(subject.substring(UPDATE_PREFIX.length())));
        } catch (IllegalArgumentException e) {
            return Optional.empty();
        }
    }

    /**
     * {@code rift.perms.hello} request.
     *
     * @param server the backend's name, the {@code server} context it fills
     * @param revision the last revision the replica saw; 0 when empty
     * @param clientVersion the client's name and version
     */
    public record HelloRequest(String server, long revision, String clientVersion) {}

    /**
     * {@code rift.perms.hello} reply data.
     *
     * @param revision the proxy's revision
     * @param protocol the protocol version
     * @param now the proxy's clock in Unix seconds
     * @param limits the store limits
     * @param externalContexts the custom keys backends may report
     */
    public record HelloReply(long revision, int protocol, long now, Limits limits, List<String> externalContexts) {
        public HelloReply {
            externalContexts = List.copyOf(externalContexts);
        }
    }

    /**
     * Store limits from the {@code hello} reply.
     *
     * @param groups groups in the store
     * @param groupNodes nodes per group
     * @param userNodes nodes per user
     * @param parents direct parents per subject
     * @param contexts contexts per node
     * @param tracks tracks in the store
     * @param compiledEntries entries per compiled subject
     * @param batch UUIDs per {@code subject.batch} request
     */
    public record Limits(
            int groups, int groupNodes, int userNodes, int parents, int contexts, int tracks, int compiledEntries,
            int batch) {
        /** The proxy's defaults, used for absent fields. */
        public static final Limits DEFAULT = new Limits(1024, 256, 512, 16, 32, 64, 4096, 256);
    }

    /**
     * {@code rift.perms.subject.get} request.
     *
     * @param uuid the player
     * @param server the requesting backend, or null
     */
    public record SubjectGetRequest(UUID uuid, String server) {}

    /**
     * {@code rift.perms.subject.batch} request.
     *
     * @param uuids at most {@link Limits#batch()} players
     */
    public record SubjectBatchRequest(List<UUID> uuids) {
        public SubjectBatchRequest {
            uuids = List.copyOf(uuids);
        }
    }

    /**
     * {@code rift.perms.subject.batch} reply data, one subject per requested UUID.
     *
     * @param revision the proxy's revision when the batch was built
     * @param subjects the subjects in request order
     */
    public record SubjectBatchReply(long revision, List<CompiledSubject> subjects) {
        public SubjectBatchReply {
            subjects = List.copyOf(subjects);
        }
    }

    /**
     * {@code rift.perms.groups.get} request.
     *
     * @param names only these groups (at most 64), or null for all
     * @param permissions also list permission nodes; only together with {@code names}
     */
    public record GroupsGetRequest(List<String> names, boolean permissions) {
        public GroupsGetRequest {
            names = names == null ? null : List.copyOf(names);
        }
    }

    /**
     * {@code rift.perms.groups.get} reply data.
     *
     * @param revision the proxy's revision
     * @param groups the groups
     */
    public record GroupsReply(long revision, List<GroupInfo> groups) {
        public GroupsReply {
            groups = List.copyOf(groups);
        }
    }

    /**
     * {@code rift.perms.apply} request.
     *
     * @param change the change
     * @param actorUuid the player acting, or null for the backend itself
     * @param reason free text for the audit log, or null
     */
    public record ApplyRequest(Change change, UUID actorUuid, String reason) {
        public ApplyRequest {
            Objects.requireNonNull(change, "change");
        }
    }

    /**
     * A plugin-declared default for a node nobody has set: {@code true}, {@code false}, {@code op},
     * {@code not_op} (Bukkit) or {@code op:<0-4>} (Pumpkin op levels).
     *
     * @param value the canonical spelling
     */
    public record PermissionDefault(String value) {
        private static final Pattern VALID = Pattern.compile("true|false|op|not_op|op:[0-4]");

        /** Everyone. */
        public static final PermissionDefault TRUE = new PermissionDefault("true");
        /** Nobody. */
        public static final PermissionDefault FALSE = new PermissionDefault("false");
        /** Operators. */
        public static final PermissionDefault OP = new PermissionDefault("op");
        /** Everyone except operators. */
        public static final PermissionDefault NOT_OP = new PermissionDefault("not_op");

        public PermissionDefault {
            if (!VALID.matcher(value).matches()) {
                throw new IllegalArgumentException(
                        "invalid permission default \"" + value + "\": expected true, false, op, op:<0-4> or not_op");
            }
        }
    }

    /**
     * One entry of the {@code rift.perms.register} payload.
     *
     * @param node the permission node
     * @param description its description, or null
     * @param defaultValue the default for players without an explicit value
     * @param plugin the declaring plugin, or null
     */
    public record RegisteredPermission(String node, String description, PermissionDefault defaultValue, String plugin) {
        public RegisteredPermission {
            Objects.requireNonNull(node, "node");
            Objects.requireNonNull(defaultValue, "defaultValue");
        }
    }

    /**
     * {@code rift.perms.command} request: a forwarded {@code /perms} line split into arguments.
     *
     * @param actorUuid the player who typed it, or null for the console
     * @param args the arguments without the command name
     */
    public record CommandRequest(UUID actorUuid, List<String> args) {
        public CommandRequest {
            args = List.copyOf(args);
        }
    }

    /**
     * {@code rift.perms.command} reply data.
     *
     * @param revision the proxy's revision after the command
     * @param component Adventure component JSON to show the sender
     */
    public record CommandReply(long revision, String component) {}

    /**
     * {@code rift.perms.context} payload: custom contexts for a player on this backend.
     *
     * @param uuid the player
     * @param contexts the player's custom contexts, replacing the previous report
     */
    public record ContextReport(UUID uuid, ContextSet contexts) {}

    /**
     * The reply envelope of every request.
     *
     * @param <T> the data type
     */
    public sealed interface Reply<T> {}

    /**
     * {@code {"ok": true, "data": ...}}.
     *
     * @param data the reply data
     * @param <T> the data type
     */
    public record Ok<T>(T data) implements Reply<T> {}

    /**
     * {@code {"ok": false, "error": ..., "code": ...}}.
     *
     * @param code why
     * @param error a human-readable message
     * @param <T> the data type the request expected
     */
    public record Err<T>(ErrorCode code, String error) implements Reply<T> {}
}
