package io.github.jotrorox.rift.permissions.client;

import io.github.jotrorox.rift.messaging.ConnectionListener;
import io.github.jotrorox.rift.messaging.Message;
import io.github.jotrorox.rift.messaging.MessagingClient;
import io.github.jotrorox.rift.messaging.MessagingException;
import io.github.jotrorox.rift.permissions.Actor;
import io.github.jotrorox.rift.permissions.Applied;
import io.github.jotrorox.rift.permissions.Change;
import io.github.jotrorox.rift.permissions.CompiledSubject;
import io.github.jotrorox.rift.permissions.ContextCalculator;
import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.Contexts;
import io.github.jotrorox.rift.permissions.ErrorCode;
import io.github.jotrorox.rift.permissions.EventBus;
import io.github.jotrorox.rift.permissions.GroupInfo;
import io.github.jotrorox.rift.permissions.MetaView;
import io.github.jotrorox.rift.permissions.PermissionChangeEvent;
import io.github.jotrorox.rift.permissions.PermissionException;
import io.github.jotrorox.rift.permissions.PermissionTimeoutException;
import io.github.jotrorox.rift.permissions.RiftPermissions;
import io.github.jotrorox.rift.permissions.SubjectUpdatedEvent;
import io.github.jotrorox.rift.permissions.Subscription;
import io.github.jotrorox.rift.permissions.Trace;
import io.github.jotrorox.rift.permissions.Tristate;
import io.github.jotrorox.rift.permissions.client.Protocol.CommandReply;
import io.github.jotrorox.rift.permissions.client.Protocol.Err;
import io.github.jotrorox.rift.permissions.client.Protocol.Ok;
import io.github.jotrorox.rift.permissions.client.Protocol.RegisteredPermission;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.time.Clock;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.Optional;
import java.util.Set;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicLong;
import java.util.function.Consumer;
import java.util.function.Function;
import java.util.logging.Level;
import java.util.logging.Logger;

/**
 * {@link RiftPermissions} for a backend: a {@link Replica} kept current over the messaging bus, the
 * {@link Resolver} for reads, and requests to the proxy for writes ({@code docs/permissions-protocol.md}).
 *
 * <p>On every (re)connection it sends {@code hello}, adopts the proxy's revision and refetches
 * stale snapshots with {@code subject.batch}; it follows {@code rift.perms.update} and the pushed
 * {@code rift.perms.update.<uuid>} snapshots. Players without a snapshot are answered by the
 * {@link OfflinePolicy}. The client does not own the {@link MessagingClient}; close both.
 */
public final class PermissionsClient implements RiftPermissions, AutoCloseable {
    /** The name and version announced in {@code hello}. */
    public static final String CLIENT_VERSION = "rift-permissions-client/" + Objects.requireNonNullElse(
            PermissionsClient.class.getPackage().getImplementationVersion(), "dev");
    /** How long a pushed snapshot may take after its change event before it is fetched instead. */
    private static final Duration PUSH_GRACE = Duration.ofMillis(250);
    /** How often players whose snapshot is missing are retried, and hello after a failure. */
    private static final Duration RETRY = Duration.ofSeconds(5);
    /** How often a stale replica is logged and departed players are evicted. */
    private static final Duration HOUSEKEEPING = Duration.ofSeconds(60);
    /** Most entries per {@code rift.perms.register} payload. */
    private static final int REGISTER_CHUNK = 4096;
    /** Clock difference to the proxy worth a warning, since it shifts every expiry. */
    private static final long SKEW_WARNING_SECONDS = 30;

    private final MessagingClient messaging;
    private final String server;
    private final Duration requestTimeout;
    private final Duration joinTimeout;
    private final OfflineStore offline;
    private final Logger logger;
    private final Clock clock;
    private final Platform platform;
    private final Replica replica = new Replica();
    private final ScheduledExecutorService scheduler =
            Executors.newSingleThreadScheduledExecutor(daemon("rift-permissions-scheduler"));
    private final ExecutorService events = Executors.newSingleThreadExecutor(daemon("rift-permissions-events"));
    private final Map<Class<?>, List<Consumer<Object>>> listeners = Map.of(
            PermissionChangeEvent.class, new CopyOnWriteArrayList<>(),
            SubjectUpdatedEvent.class, new CopyOnWriteArrayList<>());
    private final List<ContextCalculator<?>> calculators = new CopyOnWriteArrayList<>();
    private final Set<UUID> online = ConcurrentHashMap.newKeySet();
    private final Set<UUID> missing = ConcurrentHashMap.newKeySet();
    private final Map<UUID, CompletableFuture<CompiledSubject>> inflight = new ConcurrentHashMap<>();
    private final Map<UUID, ContextSet> reported = new ConcurrentHashMap<>();
    private final List<io.github.jotrorox.rift.messaging.Subscription> subscriptions = new CopyOnWriteArrayList<>();
    private final AtomicLong epoch = new AtomicLong();
    private final ConnectionListener connectionListener = new ConnectionListener() {
        @Override
        public void connected() {
            hello(epoch.incrementAndGet(), 0);
        }

        @Override
        public void disconnected(Throwable cause) {
            epoch.incrementAndGet();
            available = false;
            logger.warning("Lost the connection to the Rift proxy (" + cause.getMessage()
                    + "); serving the replica at revision " + replica.revision() + " until it is back");
        }

        @Override
        public void connectFailed(Throwable cause) {
            logger.fine(() -> "Cannot reach the Rift proxy: " + cause.getMessage());
        }
    };
    private volatile List<RegisteredPermission> registrations = List.of();
    private volatile Protocol.Limits limits = Protocol.Limits.DEFAULT;
    private volatile Set<String> externalContexts = Set.of();
    private volatile boolean available;

    private PermissionsClient(Builder builder) throws IOException {
        this.messaging = builder.messaging;
        this.server = builder.server;
        this.requestTimeout = builder.requestTimeout;
        this.joinTimeout = builder.joinTimeout;
        this.offline = OfflineStore.load(builder.offlinePolicy);
        this.logger = builder.logger;
        this.clock = builder.clock;
        this.platform = builder.platform == null ? new Standalone() : builder.platform.apply(this);
    }

    /** @return a builder */
    public static Builder builder() {
        return new Builder();
    }

    private void start() {
        messaging.addListener(connectionListener);
        CompletableFuture<?> updates = messaging.subscribe(Protocol.UPDATE, null, this::onUpdate)
                .thenAccept(subscriptions::add);
        CompletableFuture<?> snapshots = messaging.subscribe(Protocol.UPDATE_ALL, null, this::onSnapshot)
                .thenAccept(subscriptions::add);
        CompletableFuture.allOf(updates, snapshots).whenComplete((ignored, error) -> {
            if (error != null) {
                logger.severe("Cannot subscribe to permission updates: " + unwrap(error).getMessage()
                        + "; grant subscribe on rift.perms.update and rift.perms.update.>");
            } else if (messaging.isConnected() && epoch.compareAndSet(0, 1)) {
                // Connected before this client listened; otherwise connected() sends hello.
                hello(1, 0);
            }
        });
        scheduler.scheduleWithFixedDelay(this::retryMissing, RETRY.toMillis(), RETRY.toMillis(), TimeUnit.MILLISECONDS);
        scheduler.scheduleWithFixedDelay(this::housekeeping, HOUSEKEEPING.toMillis(), HOUSEKEEPING.toMillis(),
                TimeUnit.MILLISECONDS);
    }

    // ---- reads ----

    private long now() {
        return clock.instant().getEpochSecond();
    }

    private CompiledSubject subject(UUID uuid) {
        return replica.get(uuid).orElseGet(() -> offline.subject(uuid));
    }

    @Override
    public long revision() {
        return replica.revision();
    }

    @Override
    public Tristate check(UUID uuid, String node, ContextSet contexts) {
        CompiledSubject subject = subject(uuid);
        return subject == null ? Tristate.UNDEFINED : Resolver.check(subject, node, contexts, now());
    }

    @Override
    public Trace trace(UUID uuid, String node, ContextSet contexts) {
        CompiledSubject subject = subject(uuid);
        return subject == null
                ? new Trace(node, Tristate.UNDEFINED, null, List.of())
                : Resolver.trace(subject, node, contexts, now());
    }

    @Override
    public MetaView meta(UUID uuid, ContextSet contexts) {
        CompiledSubject subject = subject(uuid);
        return subject == null
                ? new MetaView(null, null, Map.of(), DEFAULT_GROUP)
                : Resolver.meta(subject, contexts, now());
    }

    @Override
    public List<String> groupsOf(UUID uuid, ContextSet contexts) {
        CompiledSubject subject = subject(uuid);
        return subject == null ? List.of() : Resolver.groups(subject, contexts, now());
    }

    @Override
    public boolean inGroup(UUID uuid, String group, ContextSet contexts) {
        CompiledSubject subject = subject(uuid);
        return subject != null && Resolver.inGroup(subject, group, contexts, now());
    }

    @Override
    public Optional<CompiledSubject> compiled(UUID uuid) {
        return replica.get(uuid);
    }

    @Override
    public CompletableFuture<CompiledSubject> load(UUID uuid) {
        return fetch(uuid, requestTimeout);
    }

    @Override
    public List<GroupInfo> groups() {
        return replica.groups().groups();
    }

    /**
     * Fetch one group with its permission nodes, which the replica's group table leaves out.
     *
     * @param name the group
     * @return the group, or empty when it does not exist
     */
    public CompletableFuture<Optional<GroupInfo>> fetchGroup(String name) {
        Protocol.GroupsGetRequest request = new Protocol.GroupsGetRequest(List.of(name), true);
        return request(Protocol.GROUPS_GET, Codec.toJson(request), Codec::readGroupsReply, requestTimeout)
                .thenApply(reply -> reply.groups().stream().filter(group -> group.name().equals(name)).findFirst());
    }

    /** @return the host adapter in use */
    public Platform platform() {
        return platform;
    }

    /** @return whether the proxy answered {@code hello} on the current connection */
    public boolean isAvailable() {
        return available;
    }

    // ---- writes ----

    @Override
    public CompletableFuture<Applied> apply(Change change) {
        return apply(null, change, null);
    }

    @Override
    public CompletableFuture<Applied> apply(Actor.Player actor, Change change, String reason) {
        Protocol.ApplyRequest request = new Protocol.ApplyRequest(change, actor == null ? null : actor.uuid(), reason);
        return request(Protocol.APPLY, Codec.toJson(request), Codec::readApplied, requestTimeout);
    }

    /**
     * Forward a {@code /perms} command to the proxy, which parses, authorises, applies and audits it.
     *
     * @param actor the player who typed it, or null for this server's console
     * @param args the arguments without the command name
     * @return the component to show and the proxy's revision
     */
    public CompletableFuture<CommandReply> command(UUID actor, List<String> args) {
        Protocol.CommandRequest request = new Protocol.CommandRequest(actor, args);
        return request(Protocol.COMMAND, Codec.toJson(request), Codec::readCommandReply, requestTimeout);
    }

    /**
     * Declare the permissions this server's plugins know, for tab completion and the dashboard. Sent
     * now when the proxy is available and again after every reconnect.
     *
     * @param permissions the registered permissions
     */
    public void register(List<RegisteredPermission> permissions) {
        registrations = List.copyOf(permissions);
        if (available) {
            publishRegistrations();
        }
    }

    /**
     * Report a player's custom contexts; keys outside the proxy's {@code external_contexts} are
     * dropped, and unchanged reports are not sent again.
     *
     * @param uuid a player on this server
     * @param custom the player's non-reserved contexts
     */
    public void reportContexts(UUID uuid, ContextSet custom) {
        ContextSet.Builder allowed = ContextSet.builder();
        Set<String> keys = externalContexts;
        custom.forEach(pair -> {
            if (keys.contains(pair.key())) {
                allowed.add(pair.key(), pair.value());
            }
        });
        ContextSet filtered = allowed.build();
        ContextSet previous = reported.put(uuid, filtered);
        if (available && !filtered.equals(previous)) {
            publishContexts(uuid, filtered);
        }
    }

    // ---- players ----

    /**
     * Wait for a joining player's snapshot: immediate when held, otherwise {@code subject.get} with
     * the join timeout. After a failure the player is retried every 5 seconds while online and the
     * offline policy answers meanwhile.
     *
     * @param uuid the joining player
     * @return the snapshot, or a failure after the join timeout
     */
    public CompletableFuture<CompiledSubject> awaitSubject(UUID uuid) {
        online.add(uuid);
        replica.markJoined(uuid);
        Optional<CompiledSubject> held = replica.get(uuid);
        if (held.isPresent()) {
            return CompletableFuture.completedFuture(held.get());
        }
        return fetch(uuid, joinTimeout).whenComplete((subject, error) -> {
            if (error != null) {
                missing.add(uuid);
            }
        });
    }

    /**
     * @param uuid a player now on this server
     */
    public void playerJoined(UUID uuid) {
        online.add(uuid);
        replica.markJoined(uuid);
        if (!replica.contains(uuid)) {
            missing.add(uuid);
        }
    }

    /**
     * @param uuid a player who left this server; the snapshot is evicted ten minutes later
     */
    public void playerLeft(UUID uuid) {
        online.remove(uuid);
        missing.remove(uuid);
        reported.remove(uuid);
        replica.markLeft(uuid, now());
    }

    // ---- contexts and events ----

    @Override
    public EventBus events() {
        return new EventBus() {
            @Override
            @SuppressWarnings("unchecked")
            public <E> Subscription subscribe(Class<E> type, Consumer<? super E> listener) {
                List<Consumer<Object>> list = listeners.get(type);
                if (list == null) {
                    throw new IllegalArgumentException("unknown permission event " + type.getName());
                }
                Consumer<Object> wrapped = event -> listener.accept((E) event);
                list.add(wrapped);
                return () -> list.remove(wrapped);
            }
        };
    }

    @Override
    public Contexts contexts() {
        return new Contexts() {
            @Override
            public ContextSet of(Object player) {
                return platform.contexts(player);
            }

            @Override
            public UUID uuid(Object player) {
                return platform.uuid(player);
            }

            @Override
            public Subscription register(ContextCalculator<?> calculator) {
                calculators.add(Objects.requireNonNull(calculator, "calculator"));
                return () -> calculators.remove(calculator);
            }
        };
    }

    /**
     * Run every registered calculator for a player; a failing or mistyped calculator is logged and
     * skipped.
     *
     * @param player a host player object
     * @param contexts the builder to add to
     * @return {@code contexts}
     */
    @SuppressWarnings({"unchecked", "rawtypes"})
    public ContextSet.Builder applyCalculators(Object player, ContextSet.Builder contexts) {
        for (ContextCalculator calculator : calculators) {
            try {
                calculator.calculate(player, contexts);
            } catch (RuntimeException e) {
                logger.log(Level.WARNING, "Context calculator " + calculator.getClass().getName() + " failed", e);
            }
        }
        return contexts;
    }

    private <E> void dispatch(Class<E> type, E event) {
        List<Consumer<Object>> list = listeners.get(type);
        if (list.isEmpty()) {
            return;
        }
        events.execute(() -> {
            for (Consumer<Object> listener : list) {
                try {
                    listener.accept(event);
                } catch (RuntimeException e) {
                    logger.log(Level.WARNING, "A " + type.getSimpleName() + " listener failed", e);
                }
            }
        });
    }

    // ---- bus ----

    private void hello(long connection, int attempt) {
        Protocol.HelloRequest request = new Protocol.HelloRequest(server, replica.revision(), CLIENT_VERSION);
        request(Protocol.HELLO, Codec.toJson(request), Codec::readHelloReply, requestTimeout)
                .whenComplete((reply, error) -> {
                    if (connection != epoch.get()) {
                        return;
                    }
                    if (error != null) {
                        String problem = "Rift permissions unavailable (" + unwrap(error).getMessage()
                                + "); retrying every " + RETRY.toSeconds() + " s";
                        logger.log(attempt == 0 ? Level.WARNING : Level.FINE, problem);
                        scheduler.schedule(() -> {
                            if (connection == epoch.get()) {
                                hello(connection, attempt + 1);
                            }
                        }, RETRY.toMillis(), TimeUnit.MILLISECONDS);
                        return;
                    }
                    if (reply.protocol() != Protocol.VERSION) {
                        logger.severe("The proxy speaks permissions protocol " + reply.protocol() + ", this client "
                                + Protocol.VERSION + "; applying the offline policy");
                        return;
                    }
                    welcome(reply);
                });
    }

    private void welcome(Protocol.HelloReply reply) {
        limits = reply.limits();
        externalContexts = Set.copyOf(reply.externalContexts());
        long skew = Math.abs(reply.now() - now());
        if (skew > SKEW_WARNING_SECONDS) {
            logger.warning("This server's clock is " + skew + " s away from the proxy's, which shifts every expiry");
        }
        boolean resync = replica.syncRevision(reply.revision());
        available = true;
        logger.info("Connected to Rift permissions at revision " + reply.revision());
        List<UUID> stale = new ArrayList<>(resync ? replica.uuids() : List.of());
        for (UUID uuid : online) {
            if (!replica.contains(uuid) && !stale.contains(uuid)) {
                stale.add(uuid);
            }
        }
        refetch(stale);
        refreshGroups();
        publishRegistrations();
        reported.forEach(this::publishContexts);
    }

    private void onUpdate(Message message) {
        PermissionChangeEvent event;
        try {
            event = Codec.readChangeEvent(Json.parse(message.text()));
        } catch (IllegalArgumentException e) {
            logger.warning("Ignoring a malformed permission update: " + e.getMessage());
            return;
        }
        switch (replica.applyUpdate(event)) {
            case UpdateOutcome.Stale stale -> {
                return;
            }
            case UpdateOutcome.Applied applied -> {
                for (UUID uuid : applied.refetch()) {
                    scheduler.schedule(() -> refetchIfOlder(uuid, event.revision()), PUSH_GRACE.toMillis(),
                            TimeUnit.MILLISECONDS);
                }
                if (applied.groups()) {
                    refreshGroups();
                }
            }
            case UpdateOutcome.Gap gap -> {
                logger.info("Missed a permission update before revision " + event.revision() + "; resynchronising");
                refetch(gap.refetch());
                refreshGroups();
            }
        }
        dispatch(PermissionChangeEvent.class, event);
    }

    private void onSnapshot(Message message) {
        Optional<UUID> uuid = Protocol.parseUpdateSubject(message.subject());
        if (uuid.isEmpty()) {
            return;
        }
        try {
            CompiledSubject subject = Codec.readCompiledSubject(Json.parse(message.text()));
            if (!subject.uuid().equals(uuid.get())) {
                logger.warning("Ignoring a snapshot for " + subject.uuid() + " pushed on " + message.subject());
                return;
            }
            store(subject);
        } catch (IllegalArgumentException e) {
            logger.warning("Ignoring a malformed snapshot on " + message.subject() + ": " + e.getMessage());
        }
    }

    private void store(CompiledSubject subject) {
        UUID uuid = subject.uuid();
        if (!replica.insert(subject)) {
            return;
        }
        if (online.contains(uuid)) {
            missing.remove(uuid);
        } else {
            // Pushed before the player arrives, or for a player elsewhere: evicted unless they join.
            replica.markLeft(uuid, now());
        }
        dispatch(SubjectUpdatedEvent.class, new SubjectUpdatedEvent(uuid, subject.revision()));
    }

    private void refetchIfOlder(UUID uuid, long revision) {
        if (replica.get(uuid).map(CompiledSubject::revision).orElse(-1L) < revision) {
            fetch(uuid, requestTimeout);
        }
    }

    private CompletableFuture<CompiledSubject> fetch(UUID uuid, Duration timeout) {
        return inflight.computeIfAbsent(uuid, key -> {
            Protocol.SubjectGetRequest request = new Protocol.SubjectGetRequest(uuid, server);
            CompletableFuture<CompiledSubject> result =
                    request(Protocol.SUBJECT_GET, Codec.toJson(request), Codec::readCompiledSubject, timeout)
                            .thenApply(subject -> {
                                if (!subject.uuid().equals(uuid)) {
                                    throw new PermissionException(ErrorCode.INTERNAL,
                                            "asked for " + uuid + ", the proxy answered " + subject.uuid());
                                }
                                store(subject);
                                return subject;
                            });
            result.whenComplete((subject, error) -> inflight.remove(uuid));
            return result;
        });
    }

    private void refetch(List<UUID> uuids) {
        int batch = Math.max(1, limits.batch());
        for (int start = 0; start < uuids.size(); start += batch) {
            List<UUID> chunk = uuids.subList(start, Math.min(uuids.size(), start + batch));
            request(Protocol.SUBJECT_BATCH, Codec.toJson(new Protocol.SubjectBatchRequest(chunk)),
                    Codec::readSubjectBatchReply, requestTimeout)
                    .whenComplete((reply, error) -> {
                        if (error != null) {
                            logger.warning("Cannot refetch " + chunk.size() + " snapshots: " + unwrap(error).getMessage());
                            missing.addAll(chunk.stream().filter(online::contains).toList());
                        } else {
                            reply.subjects().forEach(this::store);
                        }
                    });
        }
    }

    private void refreshGroups() {
        request(Protocol.GROUPS_GET, Codec.toJson(new Protocol.GroupsGetRequest(null, false)), Codec::readGroupsReply,
                requestTimeout)
                .whenComplete((reply, error) -> {
                    if (error != null) {
                        logger.warning("Cannot fetch the group table: " + unwrap(error).getMessage());
                    } else {
                        replica.setGroups(reply.revision(), reply.groups());
                    }
                });
    }

    private void publishRegistrations() {
        List<RegisteredPermission> all = registrations;
        for (int start = 0; start < all.size(); start += REGISTER_CHUNK) {
            List<RegisteredPermission> chunk = all.subList(start, Math.min(all.size(), start + REGISTER_CHUNK));
            messaging.publish(Protocol.REGISTER, bytes(Codec.toRegisterJson(chunk))).whenComplete((ignored, error) -> {
                if (error != null) {
                    logger.warning("Cannot register plugin permissions: " + unwrap(error).getMessage());
                }
            });
        }
    }

    private void publishContexts(UUID uuid, ContextSet contexts) {
        messaging.publish(Protocol.CONTEXT, bytes(Codec.toJson(new Protocol.ContextReport(uuid, contexts))))
                .whenComplete((ignored, error) -> {
                    if (error != null) {
                        logger.fine(() -> "Cannot report contexts of " + uuid + ": " + unwrap(error).getMessage());
                    }
                });
    }

    private void retryMissing() {
        if (!available) {
            return;
        }
        for (UUID uuid : missing) {
            if (!online.contains(uuid) || replica.contains(uuid)) {
                missing.remove(uuid);
            } else {
                fetch(uuid, requestTimeout);
            }
        }
    }

    private void housekeeping() {
        if (!available) {
            logger.warning("Rift permissions replica is stale at revision " + replica.revision()
                    + ": the proxy is unreachable");
        }
        replica.evict(now(), Replica.EVICT_AFTER);
    }

    /**
     * One request with the reply envelope; fails with {@link PermissionTimeoutException} after
     * {@code timeout} and {@link PermissionException} for refusals and transport errors.
     */
    private <T> CompletableFuture<T> request(String subject, Json payload, Function<Json, T> data, Duration timeout) {
        CompletableFuture<Message> sent;
        try {
            sent = messaging.request(subject, bytes(payload), timeout)
                    .orTimeout(timeout.toMillis(), TimeUnit.MILLISECONDS);
        } catch (IllegalArgumentException e) {
            return CompletableFuture.failedFuture(new PermissionException(ErrorCode.INVALID_REQUEST, e.getMessage(), e));
        }
        return sent.handle((message, error) -> {
            if (error != null) {
                throw translate(subject, timeout, unwrap(error));
            }
            Protocol.Reply<T> reply;
            try {
                reply = Codec.readReply(Json.parse(message.text()), data);
            } catch (IllegalArgumentException e) {
                throw new PermissionException(ErrorCode.INTERNAL, "malformed reply on " + subject + ": " + e.getMessage(), e);
            }
            return switch (reply) {
                case Ok<T> ok -> ok.data();
                case Err<T> err -> throw new PermissionException(err.code(), err.error());
            };
        });
    }

    private static PermissionException translate(String subject, Duration timeout, Throwable error) {
        return switch (error) {
            case PermissionException e -> e;
            case TimeoutException e ->
                    new PermissionTimeoutException("no reply on " + subject + " within " + timeout.toMillis() + " ms");
            case MessagingException e -> new PermissionException(ErrorCode.UNAVAILABLE, e.getMessage(), e);
            default -> new PermissionException(ErrorCode.INTERNAL, String.valueOf(error.getMessage()), error);
        };
    }

    private static Throwable unwrap(Throwable error) {
        Throwable current = error;
        while (current instanceof CompletionException && current.getCause() != null) {
            current = current.getCause();
        }
        return current;
    }

    private static byte[] bytes(Json json) {
        return json.toString().getBytes(StandardCharsets.UTF_8);
    }

    private static java.util.concurrent.ThreadFactory daemon(String name) {
        return runnable -> {
            Thread thread = new Thread(runnable, name);
            thread.setDaemon(true);
            return thread;
        };
    }

    /**
     * Stop following the bus and, with {@link OfflinePolicy#lastSnapshot}, persist the replica. The
     * {@link MessagingClient} stays open.
     */
    @Override
    public void close() {
        messaging.removeListener(connectionListener);
        subscriptions.forEach(io.github.jotrorox.rift.messaging.Subscription::close);
        scheduler.shutdownNow();
        events.shutdown();
        try {
            offline.save(replica.revision(), replica.subjects());
        } catch (IOException e) {
            logger.log(Level.WARNING, "Cannot persist the permission snapshot", e);
        }
    }

    /** Players are UUIDs; contexts are the server name plus the calculators' pairs. */
    private final class Standalone implements Platform {
        @Override
        public UUID uuid(Object player) {
            if (player instanceof UUID uuid) {
                return uuid;
            }
            throw new IllegalArgumentException("players are UUIDs without a platform adapter");
        }

        @Override
        public ContextSet contexts(Object player) {
            return applyCalculators(uuid(player), ContextSet.builder().add(ContextSet.SERVER, server)).build();
        }
    }

    /** Configures a {@link PermissionsClient}. */
    public static final class Builder {
        private MessagingClient messaging;
        private String server;
        private Duration requestTimeout = Duration.ofSeconds(5);
        private Duration joinTimeout = Duration.ofSeconds(2);
        private OfflinePolicy offlinePolicy = OfflinePolicy.deny();
        private Logger logger = Logger.getLogger("RiftPermissions");
        private Clock clock = Clock.systemUTC();
        private Function<PermissionsClient, Platform> platform;

        private Builder() {}

        /**
         * @param messaging the bus connection, authenticated as this server's principal
         * @return this builder
         */
        public Builder messaging(MessagingClient messaging) {
            this.messaging = Objects.requireNonNull(messaging, "messaging");
            return this;
        }

        /**
         * @param server this backend's name: the {@code server} context it fills
         * @return this builder
         */
        public Builder server(String server) {
            this.server = Objects.requireNonNull(server, "server");
            return this;
        }

        /**
         * @param timeout how long writes, commands and fetches wait for the proxy (default 5 s)
         * @return this builder
         */
        public Builder requestTimeout(Duration timeout) {
            this.requestTimeout = Objects.requireNonNull(timeout, "timeout");
            return this;
        }

        /**
         * @param timeout how long a joining player waits for a snapshot (default 2 s)
         * @return this builder
         */
        public Builder joinTimeout(Duration timeout) {
            this.joinTimeout = Objects.requireNonNull(timeout, "timeout");
            return this;
        }

        /**
         * @param policy what players without a snapshot get (default {@link OfflinePolicy#deny()})
         * @return this builder
         */
        public Builder offlinePolicy(OfflinePolicy policy) {
            this.offlinePolicy = Objects.requireNonNull(policy, "policy");
            return this;
        }

        /**
         * @param logger where the client reports state changes
         * @return this builder
         */
        public Builder logger(Logger logger) {
            this.logger = Objects.requireNonNull(logger, "logger");
            return this;
        }

        /**
         * @param clock the clock expiries are evaluated against
         * @return this builder
         */
        public Builder clock(Clock clock) {
            this.clock = Objects.requireNonNull(clock, "clock");
            return this;
        }

        /**
         * @param platform creates the host adapter, given the client for its calculators
         * @return this builder
         */
        public Builder platform(Function<PermissionsClient, Platform> platform) {
            this.platform = Objects.requireNonNull(platform, "platform");
            return this;
        }

        /**
         * Create the client and start following the bus.
         *
         * @return the client
         * @throws IOException when the offline policy's file cannot be read
         * @throws IllegalStateException when the messaging client or server name is missing
         */
        public PermissionsClient build() throws IOException {
            if (messaging == null || server == null) {
                throw new IllegalStateException("messaging and server are required");
            }
            ContextSet.of(ContextSet.SERVER, server);
            PermissionsClient client = new PermissionsClient(this);
            client.start();
            return client;
        }
    }
}
