package io.github.jotrorox.rift.permissions.test;

import io.github.jotrorox.rift.permissions.Actor;
import io.github.jotrorox.rift.permissions.Applied;
import io.github.jotrorox.rift.permissions.Change;
import io.github.jotrorox.rift.permissions.CompiledSubject;
import io.github.jotrorox.rift.permissions.ContextCalculator;
import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.Contexts;
import io.github.jotrorox.rift.permissions.EventBus;
import io.github.jotrorox.rift.permissions.GroupInfo;
import io.github.jotrorox.rift.permissions.MetaView;
import io.github.jotrorox.rift.permissions.Node;
import io.github.jotrorox.rift.permissions.PermissionChangeEvent;
import io.github.jotrorox.rift.permissions.PermissionException;
import io.github.jotrorox.rift.permissions.RiftPermissions;
import io.github.jotrorox.rift.permissions.SubjectRef;
import io.github.jotrorox.rift.permissions.SubjectUpdatedEvent;
import io.github.jotrorox.rift.permissions.Subscription;
import io.github.jotrorox.rift.permissions.Trace;
import io.github.jotrorox.rift.permissions.Tristate;
import io.github.jotrorox.rift.permissions.client.Compiler;
import io.github.jotrorox.rift.permissions.client.Resolver;
import java.time.Clock;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.Optional;
import java.util.Set;
import java.util.UUID;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.function.Consumer;
import java.util.function.Function;

/**
 * {@link RiftPermissions} in memory, for plugin unit tests: no proxy, no network. It compiles users
 * with the proxy's rules, applies {@link Change}s like the proxy (one revision per change that
 * does something) and fires events synchronously on the calling thread, so tests are
 * deterministic.
 *
 * <pre>{@code
 * InMemoryRiftPermissions perms = InMemoryRiftPermissions.builder()
 *         .group("vip", group -> group.weight(10).permission("essentials.fly", true).prefix(100, "[VIP] "))
 *         .user(alice, user -> user.parent("vip"))
 *         .build();
 * assertEquals(Tristate.TRUE, perms.check(alice, "essentials.fly", ContextSet.empty()));
 * }</pre>
 */
public final class InMemoryRiftPermissions implements RiftPermissions {
    private final Store store;
    private final Clock clock;
    private final String server;
    private final Function<Object, UUID> uuids;
    private final Function<Object, ContextSet> contexts;
    private final List<ContextCalculator<?>> calculators = new CopyOnWriteArrayList<>();
    private final Map<Class<?>, List<Consumer<Object>>> listeners = Map.of(
            PermissionChangeEvent.class, new CopyOnWriteArrayList<>(),
            SubjectUpdatedEvent.class, new CopyOnWriteArrayList<>());
    private long revision;

    private InMemoryRiftPermissions(Builder builder) {
        this.store = builder.store;
        this.clock = builder.clock;
        this.server = builder.server;
        this.uuids = builder.uuids;
        this.contexts = builder.contexts;
        this.revision = 1;
    }

    /** @return a builder for an empty store with only the {@code default} group */
    public static Builder builder() {
        return new Builder();
    }

    private long now() {
        return clock.instant().getEpochSecond();
    }

    private synchronized CompiledSubject subject(UUID uuid) {
        return Compiler.compile(store.user(uuid), store.groups(), now(), revision);
    }

    @Override
    public synchronized long revision() {
        return revision;
    }

    @Override
    public Tristate check(UUID uuid, String node, ContextSet active) {
        return Resolver.check(subject(uuid), node, active, now());
    }

    @Override
    public Trace trace(UUID uuid, String node, ContextSet active) {
        return Resolver.trace(subject(uuid), node, active, now());
    }

    @Override
    public MetaView meta(UUID uuid, ContextSet active) {
        return Resolver.meta(subject(uuid), active, now());
    }

    @Override
    public List<String> groupsOf(UUID uuid, ContextSet active) {
        return Resolver.groups(subject(uuid), active, now());
    }

    @Override
    public boolean inGroup(UUID uuid, String group, ContextSet active) {
        return Resolver.inGroup(subject(uuid), group, active, now());
    }

    /** Every UUID compiles, as on the proxy: an unknown player has just {@code default}. */
    @Override
    public Optional<CompiledSubject> compiled(UUID uuid) {
        return Optional.of(subject(uuid));
    }

    @Override
    public CompletableFuture<CompiledSubject> load(UUID uuid) {
        return CompletableFuture.completedFuture(subject(uuid));
    }

    @Override
    public synchronized List<GroupInfo> groups() {
        List<GroupInfo> result = new ArrayList<>();
        for (Compiler.Group group : store.groups().values()) {
            List<Node> nodes = group.nodes().stream()
                    .filter(node -> switch (node.payload()) {
                        case Node.Permission ignored -> false;
                        case Node.Weight ignored -> false;
                        case Node.DisplayName ignored -> false;
                        default -> true;
                    })
                    .toList();
            result.add(new GroupInfo(group.name(), group.weight(), group.displayName(), nodes));
        }
        return result;
    }

    @Override
    public CompletableFuture<Applied> apply(Change change) {
        return apply(null, change, null);
    }

    @Override
    public CompletableFuture<Applied> apply(Actor.Player actor, Change change, String reason) {
        PermissionChangeEvent event;
        Applied applied;
        List<SubjectUpdatedEvent> updates = new ArrayList<>();
        synchronized (this) {
            Map<UUID, CompiledSubject> before = compileAll();
            List<PermissionChangeEvent.Op> ops;
            try {
                ops = store.apply(change, now());
            } catch (PermissionException e) {
                return CompletableFuture.failedFuture(e);
            }
            if (ops.isEmpty()) {
                return CompletableFuture.completedFuture(new Applied(revision, 0, List.of(), List.of()));
            }
            revision++;
            List<UUID> changed = new ArrayList<>();
            compileAll().forEach((uuid, after) -> {
                CompiledSubject old = before.get(uuid);
                if (old == null || !sameContent(old, after)) {
                    changed.add(uuid);
                    updates.add(new SubjectUpdatedEvent(uuid, revision));
                }
            });
            Set<String> groups = new LinkedHashSet<>();
            for (PermissionChangeEvent.Op op : ops) {
                switch (op) {
                    case PermissionChangeEvent.NodeChange c when c.subject() instanceof SubjectRef.Group group ->
                            groups.add(group.name());
                    case PermissionChangeEvent.GroupChange c -> groups.add(c.name());
                    default -> {}
                }
            }
            Actor who = actor == null ? new Actor.Backend("server." + server) : actor;
            event = new PermissionChangeEvent(revision, now(), who, reason, ops, false, changed, List.copyOf(groups));
            applied = new Applied(revision, ops.size(), changed, List.copyOf(groups));
        }
        dispatch(PermissionChangeEvent.class, event);
        updates.forEach(update -> dispatch(SubjectUpdatedEvent.class, update));
        return CompletableFuture.completedFuture(applied);
    }

    private Map<UUID, CompiledSubject> compileAll() {
        Map<UUID, CompiledSubject> all = new HashMap<>();
        for (UUID uuid : store.users().keySet()) {
            all.put(uuid, Compiler.compile(store.user(uuid), store.groups(), now(), revision));
        }
        return all;
    }

    private static boolean sameContent(CompiledSubject a, CompiledSubject b) {
        return a.primaryGroup().equals(b.primaryGroup()) && a.entries().equals(b.entries())
                && a.prefix().equals(b.prefix()) && a.suffix().equals(b.suffix()) && a.meta().equals(b.meta());
    }

    private <E> void dispatch(Class<E> type, E event) {
        for (Consumer<Object> listener : listeners.get(type)) {
            listener.accept(event);
        }
    }

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
            @SuppressWarnings({"unchecked", "rawtypes"})
            public ContextSet of(Object player) {
                ContextSet.Builder builder = contexts.apply(player).toBuilder();
                for (ContextCalculator calculator : calculators) {
                    calculator.calculate(player, builder);
                }
                return builder.build();
            }

            @Override
            public UUID uuid(Object player) {
                return uuids.apply(player);
            }

            @Override
            public Subscription register(ContextCalculator<?> calculator) {
                calculators.add(Objects.requireNonNull(calculator, "calculator"));
                return () -> calculators.remove(calculator);
            }
        };
    }

    /** Configures an {@link InMemoryRiftPermissions}. */
    public static final class Builder {
        private final Store store = new Store();
        private Clock clock = Clock.systemUTC();
        private String server = "test";
        private Function<Object, UUID> uuids = player -> {
            if (player instanceof UUID uuid) {
                return uuid;
            }
            throw new IllegalArgumentException("players are UUIDs unless the builder maps them");
        };
        private Function<Object, ContextSet> contexts;

        private Builder() {}

        /**
         * @param server the {@code server} context of players (default {@code test})
         * @return this builder
         */
        public Builder server(String server) {
            this.server = Objects.requireNonNull(server, "server");
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
         * Map the host's player objects; without this, players are {@link UUID}s with only the
         * {@code server} context.
         *
         * @param uuid a player's UUID
         * @param active a player's active contexts, before calculators
         * @return this builder
         */
        public Builder players(Function<Object, UUID> uuid, Function<Object, ContextSet> active) {
            this.uuids = Objects.requireNonNull(uuid, "uuid");
            this.contexts = Objects.requireNonNull(active, "active");
            return this;
        }

        /**
         * @param name the group, created when missing
         * @param configure adds nodes and columns
         * @return this builder
         */
        public Builder group(String name, Consumer<GroupBuilder> configure) {
            GroupBuilder group = new GroupBuilder(new SubjectRef.Group(name));
            Compiler.Group existing = store.groups().get(group.name());
            if (existing != null) {
                group.weight = existing.weight();
                group.displayName = existing.displayName();
                group.nodes.addAll(existing.nodes());
            }
            configure.accept(group);
            store.putGroup(new Compiler.Group(group.name(), group.weight, group.displayName, group.nodes));
            return this;
        }

        /**
         * @param uuid the player
         * @param configure adds nodes and the primary group
         * @return this builder
         */
        public Builder user(UUID uuid, Consumer<UserBuilder> configure) {
            Compiler.User existing = store.user(uuid);
            UserBuilder user = new UserBuilder();
            user.primaryGroup = existing.primaryGroup();
            user.nodes.addAll(existing.nodes());
            configure.accept(user);
            store.putUser(new Compiler.User(uuid, user.primaryGroup, user.nodes));
            return this;
        }

        /**
         * @param name the track
         * @param groups its groups in order
         * @return this builder
         */
        public Builder track(String name, String... groups) {
            Change.SetTrack track = new Change.SetTrack(name, List.of(groups));
            store.putTrack(track.name(), track.groups());
            return this;
        }

        /** @return the in-memory service at revision 1 */
        public InMemoryRiftPermissions build() {
            if (contexts == null) {
                String name = server;
                contexts = player -> ContextSet.of(ContextSet.SERVER, name);
            }
            return new InMemoryRiftPermissions(this);
        }
    }

    /** Nodes shared by user and group builders. */
    public abstract static sealed class SubjectBuilder<B extends SubjectBuilder<B>> permits GroupBuilder, UserBuilder {
        final List<Node> nodes = new ArrayList<>();

        SubjectBuilder() {}

        abstract B self();

        /**
         * @param node any node, replacing one in the same slot
         * @return this builder
         */
        public B node(Node node) {
            nodes.removeIf(old -> old.kind() == node.kind()
                    && old.payload().storageKey().equals(node.payload().storageKey())
                    && old.contexts().equals(node.contexts()));
            nodes.add(node);
            return self();
        }

        /**
         * @param pattern a permission pattern
         * @param value grant or deny
         * @return this builder
         */
        public B permission(String pattern, boolean value) {
            return node(Node.permission(pattern, value));
        }

        /**
         * @param group a group to inherit globally
         * @return this builder
         */
        public B parent(String group) {
            return node(Node.inherit(group, true));
        }

        /**
         * @param priority the highest priority wins
         * @param text the prefix
         * @return this builder
         */
        public B prefix(int priority, String text) {
            return node(Node.prefix(priority, text));
        }

        /**
         * @param priority the highest priority wins
         * @param text the suffix
         * @return this builder
         */
        public B suffix(int priority, String text) {
            return node(Node.suffix(priority, text));
        }

        /**
         * @param key the meta key
         * @param value the value
         * @return this builder
         */
        public B meta(String key, String value) {
            return node(Node.meta(key, value));
        }
    }

    /** Configures one group in {@link Builder#group}. */
    public static final class GroupBuilder extends SubjectBuilder<GroupBuilder> {
        private final SubjectRef.Group ref;
        private int weight;
        private String displayName;

        private GroupBuilder(SubjectRef.Group ref) {
            this.ref = ref;
        }

        String name() {
            return ref.name();
        }

        @Override
        GroupBuilder self() {
            return this;
        }

        /**
         * @param weight higher weights rank first
         * @return this builder
         */
        public GroupBuilder weight(int weight) {
            this.weight = weight;
            return this;
        }

        /**
         * @param displayName the display name
         * @return this builder
         */
        public GroupBuilder displayName(String displayName) {
            this.displayName = new Node.DisplayName(displayName).name();
            return this;
        }
    }

    /** Configures one user in {@link Builder#user}. */
    public static final class UserBuilder extends SubjectBuilder<UserBuilder> {
        private String primaryGroup;

        private UserBuilder() {}

        @Override
        UserBuilder self() {
            return this;
        }

        /**
         * @param group the explicit primary group
         * @return this builder
         */
        public UserBuilder primaryGroup(String group) {
            this.primaryGroup = new SubjectRef.Group(group).name();
            return this;
        }
    }
}
