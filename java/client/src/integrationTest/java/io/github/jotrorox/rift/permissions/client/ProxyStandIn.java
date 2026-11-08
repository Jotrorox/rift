package io.github.jotrorox.rift.permissions.client;

import io.github.jotrorox.rift.messaging.Message;
import io.github.jotrorox.rift.messaging.MessagingClient;
import io.github.jotrorox.rift.permissions.Actor;
import io.github.jotrorox.rift.permissions.Applied;
import io.github.jotrorox.rift.permissions.CompiledSubject;
import io.github.jotrorox.rift.permissions.CompiledSubject.Entry;
import io.github.jotrorox.rift.permissions.CompiledSubject.EntryKind;
import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.ErrorCode;
import io.github.jotrorox.rift.permissions.GroupInfo;
import io.github.jotrorox.rift.permissions.Node;
import io.github.jotrorox.rift.permissions.PermissionChangeEvent;
import io.github.jotrorox.rift.permissions.SubjectRef;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.Map;
import java.util.OptionalLong;
import java.util.UUID;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.function.Function;

/**
 * Plays the proxy's {@code rift.perms.*} handlers on the real bus: a second client that subscribes
 * to the request subjects and answers with the documented envelopes, and publishes updates.
 */
final class ProxyStandIn implements AutoCloseable {
    final MessagingClient bus;
    final BlockingQueue<Protocol.HelloRequest> hellos = new LinkedBlockingQueue<>();
    final BlockingQueue<Protocol.SubjectGetRequest> gets = new LinkedBlockingQueue<>();
    final BlockingQueue<Protocol.SubjectBatchRequest> batches = new LinkedBlockingQueue<>();
    final BlockingQueue<Protocol.ApplyRequest> applies = new LinkedBlockingQueue<>();
    final BlockingQueue<Protocol.CommandRequest> commands = new LinkedBlockingQueue<>();
    final BlockingQueue<List<Protocol.RegisteredPermission>> registrations = new LinkedBlockingQueue<>();
    final BlockingQueue<Protocol.ContextReport> contexts = new LinkedBlockingQueue<>();
    /** The permission each player holds; changed by applies. */
    final Map<UUID, Boolean> fly = new ConcurrentHashMap<>();
    volatile long revision = 1;

    ProxyStandIn(MessagingClient bus) {
        this.bus = bus;
    }

    /** Subscribe every handler and wait until the server accepted them. */
    void start() {
        CompletableFuture.allOf(
                answer(Protocol.HELLO, json -> {
                    hellos.add(Codec.readHelloRequest(json));
                    return Codec.toJson(new Protocol.HelloReply(revision, Protocol.VERSION,
                            System.currentTimeMillis() / 1000, Protocol.Limits.DEFAULT, List.of("region")));
                }),
                answer(Protocol.SUBJECT_GET, json -> {
                    Protocol.SubjectGetRequest request = Codec.readSubjectGetRequest(json);
                    gets.add(request);
                    return Codec.toJson(subject(request.uuid()));
                }),
                answer(Protocol.SUBJECT_BATCH, json -> {
                    Protocol.SubjectBatchRequest request = Codec.readSubjectBatchRequest(json);
                    batches.add(request);
                    return Codec.toJson(new Protocol.SubjectBatchReply(revision,
                            request.uuids().stream().map(this::subject).toList()));
                }),
                answer(Protocol.GROUPS_GET, json -> Codec.toJson(new Protocol.GroupsReply(revision,
                        List.of(new GroupInfo("default", 0, null, List.of()),
                                new GroupInfo("vip", 10, "VIP", List.of(Node.prefix(100, "[VIP] "))))))),
                bus.subscribe(Protocol.APPLY, null, this::apply),
                bus.subscribe(Protocol.COMMAND, null, this::command),
                bus.subscribe(Protocol.REGISTER, null,
                        message -> registrations.add(Codec.readRegisterRequest(Json.parse(message.text())))),
                bus.subscribe(Protocol.CONTEXT, null,
                        message -> contexts.add(Codec.readContextReport(Json.parse(message.text())))))
                .join();
    }

    CompiledSubject subject(UUID uuid) {
        Entry entry = new Entry(EntryKind.PERMISSION, "essentials.fly", fly.getOrDefault(uuid, true),
                ContextSet.empty(), 0, OptionalLong.empty());
        Entry member = new Entry(EntryKind.GROUP, "default", true, ContextSet.empty(), 1, OptionalLong.empty());
        return new CompiledSubject(revision, uuid, "default", List.of(entry, member), List.of(), List.of(), List.of());
    }

    private CompletableFuture<?> answer(String subject, Function<Json, Json> handler) {
        return bus.subscribe(subject, null, message -> reply(message,
                Codec.toJson(new Protocol.Ok<>(handler.apply(Json.parse(message.text()))), Function.identity())));
    }

    private void reply(Message request, Json envelope) {
        bus.publish(request.reply(), envelope.toString().getBytes(StandardCharsets.UTF_8)).join();
    }

    /** Commit like the proxy: reply, publish the change event, then push the fresh snapshot. */
    private void apply(Message message) {
        Protocol.ApplyRequest request = Codec.readApplyRequest(Json.parse(message.text()));
        applies.add(request);
        if (request.change() instanceof io.github.jotrorox.rift.permissions.Change.SetNode set
                && set.subject() instanceof SubjectRef.User user
                && set.node().payload() instanceof Node.Permission permission) {
            revision++;
            fly.put(user.uuid(), permission.value());
            Applied applied = new Applied(revision, 1, List.of(user.uuid()), List.of());
            reply(message, Codec.toJson(new Protocol.Ok<>(Codec.toJson(applied)), Function.identity()));
            PermissionChangeEvent event = new PermissionChangeEvent(revision, System.currentTimeMillis() / 1000,
                    request.actorUuid() == null ? new Actor.Backend("server.survival-1")
                            : new Actor.Player(request.actorUuid()),
                    request.reason(), List.of(new PermissionChangeEvent.NodeChange(user, null, set.node())), false,
                    List.of(user.uuid()), List.of());
            bus.publish(Protocol.UPDATE, Codec.toJson(event).toString().getBytes(StandardCharsets.UTF_8)).join();
            push(user.uuid());
        } else {
            reply(message, Codec.toJson(new Protocol.Err<Json>(ErrorCode.FORBIDDEN, "only permission nodes on users"),
                    Function.identity()));
        }
    }

    /** Publish {@code rift.perms.update.<uuid>}. */
    void push(UUID uuid) {
        bus.publish(Protocol.updateSubject(uuid), Codec.toJson(subject(uuid)).toString().getBytes(StandardCharsets.UTF_8))
                .join();
    }

    /** Answer every command except {@code ignore}, which tests the client's timeout. */
    private void command(Message message) {
        Protocol.CommandRequest request = Codec.readCommandRequest(Json.parse(message.text()));
        commands.add(request);
        if (!request.args().equals(List.of("ignore"))) {
            String component = "{\"text\":\"ran " + String.join(" ", request.args()) + "\",\"color\":\"green\"}";
            Json data = Codec.toJson(new Protocol.CommandReply(revision, component));
            reply(message, Codec.toJson(new Protocol.Ok<>(data), Function.identity()));
        }
    }

    @Override
    public void close() {
        bus.close();
    }
}
