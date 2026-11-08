package io.github.jotrorox.rift.permissions.client;

import io.github.jotrorox.rift.permissions.Actor;
import io.github.jotrorox.rift.permissions.Applied;
import io.github.jotrorox.rift.permissions.Change;
import io.github.jotrorox.rift.permissions.CompiledSubject;
import io.github.jotrorox.rift.permissions.CompiledSubject.Decoration;
import io.github.jotrorox.rift.permissions.CompiledSubject.Entry;
import io.github.jotrorox.rift.permissions.CompiledSubject.EntryKind;
import io.github.jotrorox.rift.permissions.CompiledSubject.MetaEntry;
import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.ErrorCode;
import io.github.jotrorox.rift.permissions.Filter;
import io.github.jotrorox.rift.permissions.GroupInfo;
import io.github.jotrorox.rift.permissions.MetaView;
import io.github.jotrorox.rift.permissions.Node;
import io.github.jotrorox.rift.permissions.NodeKind;
import io.github.jotrorox.rift.permissions.Patch;
import io.github.jotrorox.rift.permissions.PermissionChangeEvent;
import io.github.jotrorox.rift.permissions.PermissionChangeEvent.GroupAttributes;
import io.github.jotrorox.rift.permissions.SubjectRef;
import io.github.jotrorox.rift.permissions.Trace;
import io.github.jotrorox.rift.permissions.Tristate;
import io.github.jotrorox.rift.permissions.Uuids;
import io.github.jotrorox.rift.permissions.client.Json.Arr;
import io.github.jotrorox.rift.permissions.client.Json.Bool;
import io.github.jotrorox.rift.permissions.client.Json.Null;
import io.github.jotrorox.rift.permissions.client.Json.Num;
import io.github.jotrorox.rift.permissions.client.Json.Obj;
import io.github.jotrorox.rift.permissions.client.Json.Str;
import io.github.jotrorox.rift.permissions.client.Protocol.ApplyRequest;
import io.github.jotrorox.rift.permissions.client.Protocol.CommandReply;
import io.github.jotrorox.rift.permissions.client.Protocol.CommandRequest;
import io.github.jotrorox.rift.permissions.client.Protocol.ContextReport;
import io.github.jotrorox.rift.permissions.client.Protocol.Err;
import io.github.jotrorox.rift.permissions.client.Protocol.GroupsGetRequest;
import io.github.jotrorox.rift.permissions.client.Protocol.GroupsReply;
import io.github.jotrorox.rift.permissions.client.Protocol.HelloReply;
import io.github.jotrorox.rift.permissions.client.Protocol.HelloRequest;
import io.github.jotrorox.rift.permissions.client.Protocol.Limits;
import io.github.jotrorox.rift.permissions.client.Protocol.Ok;
import io.github.jotrorox.rift.permissions.client.Protocol.PermissionDefault;
import io.github.jotrorox.rift.permissions.client.Protocol.RegisteredPermission;
import io.github.jotrorox.rift.permissions.client.Protocol.Reply;
import io.github.jotrorox.rift.permissions.client.Protocol.SubjectBatchReply;
import io.github.jotrorox.rift.permissions.client.Protocol.SubjectBatchRequest;
import io.github.jotrorox.rift.permissions.client.Protocol.SubjectGetRequest;
import java.math.BigDecimal;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.OptionalLong;
import java.util.UUID;
import java.util.function.Function;

/**
 * The protocol's JSON forms, identical to the Rust crate's serde output: every reader validates
 * like its Rust counterpart (and ignores unknown members), every writer omits what serde omits.
 * Readers throw {@link IllegalArgumentException} for malformed input.
 */
public final class Codec {
    private Codec() {}

    // ---- common values ----

    /**
     * @param contexts a context set
     * @return {@code {"k": "v", "k2": ["a", "b"]}}
     */
    public static Json toJson(ContextSet contexts) {
        Map<String, Json> members = new LinkedHashMap<>();
        contexts.asMap().forEach((key, values) -> members.put(key,
                values.size() == 1 ? new Str(values.getFirst()) : strings(values)));
        return new Obj(members);
    }

    /**
     * @param json a context set
     * @return the normalised set
     */
    public static ContextSet readContextSet(Json json) {
        ContextSet.Builder builder = ContextSet.builder();
        for (Map.Entry<String, Json> member : object(json, "context set").members().entrySet()) {
            switch (member.getValue()) {
                case Str value -> builder.add(member.getKey(), value.value());
                case Arr values -> values.items().forEach(v -> builder.add(member.getKey(), string(v, "context value")));
                default -> throw invalid("context values are strings or arrays of strings");
            }
        }
        return builder.build();
    }

    /**
     * @param value a tristate
     * @return {@code true}, {@code false} or {@code null}
     */
    public static Json toJson(Tristate value) {
        return switch (value) {
            case TRUE -> new Bool(true);
            case FALSE -> new Bool(false);
            case UNDEFINED -> Null.INSTANCE;
        };
    }

    /**
     * @param json {@code true}, {@code false} or {@code null}
     * @return the tristate
     */
    public static Tristate readTristate(Json json) {
        return switch (json) {
            case Null ignored -> Tristate.UNDEFINED;
            case Bool value -> Tristate.of(value.value());
            default -> throw invalid("a tristate is true, false or null");
        };
    }

    // ---- nodes ----

    /**
     * @param node a node
     * @return the typed node JSON, omitting global contexts, permanence and source {@code store}
     */
    public static Json toJson(Node node) {
        Map<String, Json> members = new LinkedHashMap<>();
        members.put("kind", new Str(node.kind().wireName()));
        switch (node.payload()) {
            case Node.Permission p -> {
                members.put("key", new Str(p.key()));
                members.put("value", new Bool(p.value()));
            }
            case Node.Inherit p -> {
                members.put("group", new Str(p.group()));
                members.put("value", new Bool(p.value()));
            }
            case Node.Prefix p -> {
                members.put("priority", Num.of(p.priority()));
                members.put("text", new Str(p.text()));
            }
            case Node.Suffix p -> {
                members.put("priority", Num.of(p.priority()));
                members.put("text", new Str(p.text()));
            }
            case Node.Meta p -> {
                members.put("key", new Str(p.key()));
                members.put("value", new Str(p.value()));
            }
            case Node.Weight p -> members.put("weight", Num.of(p.weight()));
            case Node.DisplayName p -> members.put("name", new Str(p.name()));
        }
        if (!node.contexts().isEmpty()) {
            members.put("contexts", toJson(node.contexts()));
        }
        node.expiresAt().ifPresent(at -> members.put("expires_at", Num.of(at)));
        if (!node.source().equals(Node.SOURCE_STORE)) {
            members.put("source", new Str(node.source()));
        }
        return new Obj(members);
    }

    /**
     * @param json a typed node
     * @return the normalised node
     */
    public static Node readNode(Json json) {
        Fields f = new Fields(json, "node");
        NodeKind kind = NodeKind.fromWireName(f.string("kind"));
        Node.Payload payload = switch (kind) {
            case PERMISSION -> new Node.Permission(f.string("key"), f.bool("value"));
            case INHERIT -> new Node.Inherit(f.string("group"), f.bool("value"));
            case PREFIX -> new Node.Prefix(f.i32("priority"), f.string("text"));
            case SUFFIX -> new Node.Suffix(f.i32("priority"), f.string("text"));
            case META -> new Node.Meta(f.string("key"), f.string("value"));
            case WEIGHT -> new Node.Weight(f.i32("weight"));
            case DISPLAY_NAME -> new Node.DisplayName(f.string("name"));
        };
        Json contexts = f.defaulted("contexts");
        Json source = f.defaulted("source");
        return new Node(payload,
                contexts == null ? ContextSet.empty() : readContextSet(contexts),
                f.optionalU64("expires_at"),
                source == null ? Node.SOURCE_STORE : string(source, "source"));
    }

    private static Json nodes(List<Node> nodes) {
        return new Arr(nodes.stream().map(Codec::toJson).toList());
    }

    /**
     * @param json an array of typed nodes
     * @return the nodes
     */
    public static List<Node> readNodes(Json json) {
        return list(json, Codec::readNode);
    }

    // ---- compiled subjects ----

    /**
     * @param subject a compiled subject
     * @return its wire form
     */
    public static Json toJson(CompiledSubject subject) {
        Map<String, Json> members = new LinkedHashMap<>();
        members.put("revision", Num.of(subject.revision()));
        members.put("uuid", uuid(subject.uuid()));
        members.put("primary_group", new Str(subject.primaryGroup()));
        members.put("entries", new Arr(subject.entries().stream().map(Codec::toJson).toList()));
        members.put("prefix", new Arr(subject.prefix().stream().map(Codec::toJson).toList()));
        members.put("suffix", new Arr(subject.suffix().stream().map(Codec::toJson).toList()));
        members.put("meta", new Arr(subject.meta().stream().map(Codec::toJson).toList()));
        return new Obj(members);
    }

    /**
     * @param json a compiled subject
     * @return the subject; fails on an unknown entry kind, which the protocol version rules out
     */
    public static CompiledSubject readCompiledSubject(Json json) {
        Fields f = new Fields(json, "compiled subject");
        return new CompiledSubject(
                f.u64("revision"),
                readUuid(f.required("uuid")),
                f.string("primary_group"),
                list(f.required("entries"), Codec::readEntry),
                f.defaultedList("prefix", Codec::readDecoration),
                f.defaultedList("suffix", Codec::readDecoration),
                f.defaultedList("meta", Codec::readMetaEntry));
    }

    private static Json toJson(Entry entry) {
        Map<String, Json> members = new LinkedHashMap<>();
        members.put("k", new Str(entry.kind().wireName()));
        members.put("key", new Str(entry.key()));
        members.put("v", new Bool(entry.value()));
        members.put("ctx", toJson(entry.contexts()));
        members.put("rank", Num.of(entry.rank()));
        entry.expiresAt().ifPresent(at -> members.put("exp", Num.of(at)));
        return new Obj(members);
    }

    private static Entry readEntry(Json json) {
        Fields f = new Fields(json, "entry");
        EntryKind kind = switch (f.string("k")) {
            case "perm" -> EntryKind.PERMISSION;
            case "group" -> EntryKind.GROUP;
            default -> throw invalid("unknown entry kind; the protocol version changes before new kinds appear");
        };
        return new Entry(kind, f.string("key"), f.bool("v"), f.contextsOrEmpty("ctx"), f.u32("rank"),
                f.optionalU64("exp"));
    }

    private static Json toJson(Decoration decoration) {
        Map<String, Json> members = new LinkedHashMap<>();
        members.put("p", Num.of(decoration.priority()));
        members.put("t", new Str(decoration.text()));
        members.put("ctx", toJson(decoration.contexts()));
        members.put("rank", Num.of(decoration.rank()));
        decoration.expiresAt().ifPresent(at -> members.put("exp", Num.of(at)));
        return new Obj(members);
    }

    private static Decoration readDecoration(Json json) {
        Fields f = new Fields(json, "decoration");
        return new Decoration(f.i32("p"), f.string("t"), f.contextsOrEmpty("ctx"), f.u32("rank"), f.optionalU64("exp"));
    }

    private static Json toJson(MetaEntry meta) {
        Map<String, Json> members = new LinkedHashMap<>();
        members.put("key", new Str(meta.key()));
        members.put("v", new Str(meta.value()));
        members.put("ctx", toJson(meta.contexts()));
        members.put("rank", Num.of(meta.rank()));
        meta.expiresAt().ifPresent(at -> members.put("exp", Num.of(at)));
        return new Obj(members);
    }

    private static MetaEntry readMetaEntry(Json json) {
        Fields f = new Fields(json, "meta entry");
        return new MetaEntry(f.string("key"), f.string("v"), f.contextsOrEmpty("ctx"), f.u32("rank"), f.optionalU64("exp"));
    }

    // ---- check results ----

    /**
     * @param view a meta view
     * @return {@code {"prefix", "suffix", "meta", "primary_group"}}
     */
    public static Json toJson(MetaView view) {
        Map<String, Json> members = new LinkedHashMap<>();
        members.put("prefix", nullableString(view.prefix()));
        members.put("suffix", nullableString(view.suffix()));
        Map<String, Json> meta = new LinkedHashMap<>();
        view.meta().forEach((key, value) -> meta.put(key, new Str(value)));
        members.put("meta", new Obj(meta));
        members.put("primary_group", new Str(view.primaryGroup()));
        return new Obj(members);
    }

    /**
     * @param json a meta view
     * @return the view
     */
    public static MetaView readMetaView(Json json) {
        Fields f = new Fields(json, "meta view");
        Map<String, String> meta = new LinkedHashMap<>();
        object(f.required("meta"), "meta").members().forEach((key, value) -> meta.put(key, string(value, "meta value")));
        return new MetaView(f.optionalString("prefix"), f.optionalString("suffix"), meta, f.string("primary_group"));
    }

    /**
     * @param trace a trace
     * @return its wire form
     */
    public static Json toJson(Trace trace) {
        Map<String, Json> members = new LinkedHashMap<>();
        members.put("node", new Str(trace.node()));
        members.put("result", toJson(trace.result()));
        members.put("winner", trace.winner() == null ? Null.INSTANCE : toJson(trace.winner()));
        members.put("losers", new Arr(trace.losers().stream().map(Codec::toJson).toList()));
        return new Obj(members);
    }

    /**
     * @param json a trace
     * @return the trace
     */
    public static Trace readTrace(Json json) {
        Fields f = new Fields(json, "trace");
        Json winner = f.optional("winner");
        return new Trace(f.string("node"), readTristate(f.required("result")),
                winner == null ? null : readCandidate(winner), list(f.required("losers"), Codec::readCandidate));
    }

    private static Json toJson(Trace.Candidate candidate) {
        Map<String, Json> members = new LinkedHashMap<>();
        members.put("entry", toJson(candidate.entry()));
        Trace.Specificity specificity = candidate.specificity();
        members.put("specificity", switch (specificity.level()) {
            case ANY -> new Str("any");
            case EXACT -> new Str("exact");
            case PREFIX -> new Obj(Map.of("prefix", Num.of(specificity.length())));
        });
        members.put("outcome", new Str(candidate.outcome().wireName()));
        return new Obj(members);
    }

    private static Trace.Candidate readCandidate(Json json) {
        Fields f = new Fields(json, "candidate");
        Trace.Specificity specificity = switch (f.required("specificity")) {
            case Str s when s.value().equals("any") -> Trace.Specificity.ANY;
            case Str s when s.value().equals("exact") -> Trace.Specificity.EXACT;
            case Obj o when o.members().size() == 1 && o.get("prefix") != null ->
                    Trace.Specificity.prefix(nonNegativeInt(o.get("prefix"), "prefix"));
            default -> throw invalid("specificity is \"any\", {\"prefix\": n} or \"exact\"");
        };
        String outcome = f.string("outcome");
        for (Trace.Outcome value : Trace.Outcome.values()) {
            if (value.wireName().equals(outcome)) {
                return new Trace.Candidate(readEntry(f.required("entry")), specificity, value);
            }
        }
        throw invalid("unknown outcome \"" + outcome + "\"");
    }

    // ---- changes ----

    private static Json toJson(SubjectRef subject) {
        return switch (subject) {
            case SubjectRef.User user -> new Obj(Map.of("user", uuid(user.uuid())));
            case SubjectRef.Group group -> new Obj(Map.of("group", new Str(group.name())));
        };
    }

    private static SubjectRef readSubjectRef(Json json) {
        Obj obj = object(json, "subject");
        if (obj.members().size() != 1) {
            throw invalid("a subject is {\"user\": uuid} or {\"group\": name}");
        }
        Map.Entry<String, Json> only = obj.members().entrySet().iterator().next();
        return switch (only.getKey()) {
            case "user" -> SubjectRef.user(readUuid(only.getValue()));
            case "group" -> SubjectRef.group(string(only.getValue(), "group"));
            default -> throw invalid("a subject is {\"user\": uuid} or {\"group\": name}");
        };
    }

    /**
     * @param change a change
     * @return its wire form, tagged by {@code op}
     */
    public static Json toJson(Change change) {
        Map<String, Json> m = new LinkedHashMap<>();
        switch (change) {
            case Change.SetNode c -> {
                m.put("op", new Str("set_node"));
                m.put("subject", toJson(c.subject()));
                m.put("node", toJson(c.node()));
            }
            case Change.UnsetNode c -> {
                m.put("op", new Str("unset_node"));
                m.put("subject", toJson(c.subject()));
                m.put("kind", new Str(c.kind().wireName()));
                m.put("key", new Str(c.key()));
                if (!c.contexts().isEmpty()) {
                    m.put("contexts", toJson(c.contexts()));
                }
            }
            case Change.ClearSubject c -> {
                m.put("op", new Str("clear_subject"));
                m.put("subject", toJson(c.subject()));
                if (c.contexts() != null) {
                    m.put("contexts", toJson(c.contexts()));
                }
            }
            case Change.CreateGroup c -> {
                m.put("op", new Str("create_group"));
                m.put("name", new Str(c.name()));
                m.put("weight", Num.of(c.weight()));
                if (c.displayName() != null) {
                    m.put("display_name", new Str(c.displayName()));
                }
            }
            case Change.DeleteGroup c -> {
                m.put("op", new Str("delete_group"));
                m.put("name", new Str(c.name()));
            }
            case Change.SetWeight c -> {
                m.put("op", new Str("set_weight"));
                m.put("group", new Str(c.group()));
                m.put("weight", Num.of(c.weight()));
            }
            case Change.SetDisplayName c -> {
                m.put("op", new Str("set_display_name"));
                m.put("group", new Str(c.group()));
                m.put("display_name", nullableString(c.displayName()));
            }
            case Change.SetPrimaryGroup c -> {
                m.put("op", new Str("set_primary_group"));
                m.put("user", uuid(c.user()));
                m.put("group", nullableString(c.group()));
            }
            case Change.SetTrack c -> {
                m.put("op", new Str("set_track"));
                m.put("name", new Str(c.name()));
                m.put("groups", strings(c.groups()));
            }
            case Change.DeleteTrack c -> {
                m.put("op", new Str("delete_track"));
                m.put("name", new Str(c.name()));
            }
            case Change.Promote c -> track(m, "promote", c.user(), c.track(), c.contexts());
            case Change.Demote c -> track(m, "demote", c.user(), c.track(), c.contexts());
            case Change.Bulk c -> {
                m.put("op", new Str("bulk"));
                m.put("filter", toJson(c.filter()));
                m.put("patch", toJson(c.patch()));
            }
            case Change.Batch c -> {
                m.put("op", new Str("batch"));
                m.put("changes", new Arr(c.changes().stream().map(Codec::toJson).toList()));
            }
        }
        return new Obj(m);
    }

    private static void track(Map<String, Json> m, String op, UUID user, String track, ContextSet contexts) {
        m.put("op", new Str(op));
        m.put("user", uuid(user));
        m.put("track", new Str(track));
        if (!contexts.isEmpty()) {
            m.put("contexts", toJson(contexts));
        }
    }

    /**
     * @param json a change
     * @return the normalised, validated change
     */
    public static Change readChange(Json json) {
        Fields f = new Fields(json, "change");
        return switch (f.string("op")) {
            case "set_node" -> new Change.SetNode(readSubjectRef(f.required("subject")), readNode(f.required("node")));
            case "unset_node" -> new Change.UnsetNode(readSubjectRef(f.required("subject")),
                    NodeKind.fromWireName(f.string("kind")), f.string("key"), f.contextsOrEmpty("contexts"));
            case "clear_subject" -> {
                Json contexts = f.optional("contexts");
                yield new Change.ClearSubject(readSubjectRef(f.required("subject")),
                        contexts == null ? null : readContextSet(contexts));
            }
            case "create_group" -> {
                Json weight = f.defaulted("weight");
                yield new Change.CreateGroup(f.string("name"), weight == null ? 0 : int32(weight, "weight"),
                        f.optionalString("display_name"));
            }
            case "delete_group" -> new Change.DeleteGroup(f.string("name"));
            case "set_weight" -> new Change.SetWeight(f.string("group"), f.i32("weight"));
            case "set_display_name" -> new Change.SetDisplayName(f.string("group"), f.optionalString("display_name"));
            case "set_primary_group" -> new Change.SetPrimaryGroup(readUuid(f.required("user")),
                    f.optionalString("group"));
            case "set_track" -> new Change.SetTrack(f.string("name"), list(f.required("groups"), v -> string(v, "group")));
            case "delete_track" -> new Change.DeleteTrack(f.string("name"));
            case "promote" -> new Change.Promote(readUuid(f.required("user")), f.string("track"),
                    f.contextsOrEmpty("contexts"));
            case "demote" -> new Change.Demote(readUuid(f.required("user")), f.string("track"),
                    f.contextsOrEmpty("contexts"));
            case "bulk" -> new Change.Bulk(readFilter(f.required("filter")), readPatch(f.required("patch")));
            case "batch" -> new Change.Batch(list(f.required("changes"), Codec::readChange));
            default -> throw invalid("unknown change op");
        };
    }

    /**
     * @param json an array of changes
     * @return the changes
     */
    public static List<Change> readChanges(Json json) {
        return list(json, Codec::readChange);
    }

    private static Json toJson(Filter filter) {
        Map<String, Json> m = new LinkedHashMap<>();
        if (filter.subjects() != Filter.Subjects.ANY) {
            m.put("subjects", new Str(filter.subjects().name().toLowerCase(java.util.Locale.ROOT)));
        }
        if (filter.subject() != null) {
            m.put("subject", toJson(filter.subject()));
        }
        if (filter.kind() != null) {
            m.put("kind", new Str(filter.kind().wireName()));
        }
        putIfPresent(m, "key_like", filter.keyLike());
        putIfPresent(m, "value", filter.value());
        if (filter.contexts() != null) {
            m.put("contexts", toJson(filter.contexts()));
        }
        putIfPresent(m, "source", filter.source());
        return new Obj(m);
    }

    private static Filter readFilter(Json json) {
        Fields f = new Fields(json, "filter");
        Json subjects = f.defaulted("subjects");
        Filter.Subjects scope = subjects == null ? Filter.Subjects.ANY : switch (string(subjects, "subjects")) {
            case "any" -> Filter.Subjects.ANY;
            case "users" -> Filter.Subjects.USERS;
            case "groups" -> Filter.Subjects.GROUPS;
            default -> throw invalid("subjects is any, users or groups");
        };
        Json subject = f.optional("subject");
        String kind = f.optionalString("kind");
        Json contexts = f.optional("contexts");
        return new Filter(scope, subject == null ? null : readSubjectRef(subject),
                kind == null ? null : NodeKind.fromWireName(kind), f.optionalString("key_like"),
                f.optionalString("value"), contexts == null ? null : readContextSet(contexts),
                f.optionalString("source"));
    }

    private static Json toJson(Patch patch) {
        Map<String, Json> m = new LinkedHashMap<>();
        if (patch.delete()) {
            m.put("delete", new Bool(true));
        }
        if (patch.value() != null) {
            m.put("value", new Bool(patch.value()));
        }
        if (patch.contexts() != null) {
            m.put("contexts", toJson(patch.contexts()));
        }
        if (patch.expiresAt() != null) {
            m.put("expires_at", Num.of(patch.expiresAt()));
        }
        if (patch.permanent()) {
            m.put("permanent", new Bool(true));
        }
        putIfPresent(m, "source", patch.source());
        return new Obj(m);
    }

    private static Patch readPatch(Json json) {
        Fields f = new Fields(json, "patch");
        Json value = f.optional("value");
        Json contexts = f.optional("contexts");
        OptionalLong expiresAt = f.optionalU64("expires_at");
        return new Patch(f.flag("delete"), value == null ? null : bool(value, "value"),
                contexts == null ? null : readContextSet(contexts),
                expiresAt.isPresent() ? expiresAt.getAsLong() : null, f.flag("permanent"),
                f.optionalString("source"));
    }

    // ---- results and events ----

    /**
     * @param applied an apply result
     * @return its wire form
     */
    public static Json toJson(Applied applied) {
        Map<String, Json> m = new LinkedHashMap<>();
        m.put("revision", Num.of(applied.revision()));
        m.put("ops", Num.of(applied.ops()));
        m.put("changed_users", uuids(applied.changedUsers()));
        m.put("changed_groups", strings(applied.changedGroups()));
        return new Obj(m);
    }

    /**
     * @param json an apply result
     * @return the result
     */
    public static Applied readApplied(Json json) {
        Fields f = new Fields(json, "applied");
        return new Applied(f.u64("revision"), f.size("ops"), list(f.required("changed_users"), Codec::readUuid),
                list(f.required("changed_groups"), v -> string(v, "group")));
    }

    /**
     * @param event a change event
     * @return the {@code rift.perms.update} payload
     */
    public static Json toJson(PermissionChangeEvent event) {
        Map<String, Json> m = new LinkedHashMap<>();
        m.put("revision", Num.of(event.revision()));
        m.put("at", Num.of(event.at()));
        m.put("actor", new Str(event.actor().toString()));
        putIfPresent(m, "reason", event.reason());
        m.put("ops", new Arr(event.ops().stream().map(Codec::toJson).toList()));
        if (event.opsTruncated()) {
            m.put("ops_truncated", new Bool(true));
        }
        m.put("changed", uuids(event.changed()));
        m.put("groups", strings(event.groups()));
        return new Obj(m);
    }

    /**
     * @param json a {@code rift.perms.update} payload
     * @return the event
     */
    public static PermissionChangeEvent readChangeEvent(Json json) {
        Fields f = new Fields(json, "change event");
        return new PermissionChangeEvent(f.u64("revision"), f.u64("at"), Actor.parse(f.string("actor")),
                f.optionalString("reason"), list(f.required("ops"), Codec::readOp), f.flag("ops_truncated"),
                f.defaultedList("changed", Codec::readUuid), f.defaultedList("groups", v -> string(v, "group")));
    }

    private static Json toJson(PermissionChangeEvent.Op op) {
        Map<String, Json> m = new LinkedHashMap<>();
        switch (op) {
            case PermissionChangeEvent.NodeChange c -> {
                m.put("op", new Str("node"));
                m.put("subject", toJson(c.subject()));
                m.put("before", c.before() == null ? Null.INSTANCE : toJson(c.before()));
                m.put("after", c.after() == null ? Null.INSTANCE : toJson(c.after()));
            }
            case PermissionChangeEvent.GroupChange c -> {
                m.put("op", new Str("group"));
                m.put("name", new Str(c.name()));
                m.put("before", toJson(c.before()));
                m.put("after", toJson(c.after()));
            }
            case PermissionChangeEvent.TrackChange c -> {
                m.put("op", new Str("track"));
                m.put("name", new Str(c.name()));
                m.put("before", c.before() == null ? Null.INSTANCE : strings(c.before()));
                m.put("after", c.after() == null ? Null.INSTANCE : strings(c.after()));
            }
            case PermissionChangeEvent.PrimaryGroupChange c -> {
                m.put("op", new Str("primary_group"));
                m.put("user", uuid(c.user()));
                m.put("before", nullableString(c.before()));
                m.put("after", nullableString(c.after()));
            }
        }
        return new Obj(m);
    }

    private static Json toJson(GroupAttributes attributes) {
        if (attributes == null) {
            return Null.INSTANCE;
        }
        Map<String, Json> m = new LinkedHashMap<>();
        m.put("weight", Num.of(attributes.weight()));
        putIfPresent(m, "display_name", attributes.displayName());
        return new Obj(m);
    }

    private static GroupAttributes readGroupAttributes(Json json) {
        if (json == null) {
            return null;
        }
        Fields f = new Fields(json, "group attributes");
        return new GroupAttributes(f.i32("weight"), f.optionalString("display_name"));
    }

    private static PermissionChangeEvent.Op readOp(Json json) {
        Fields f = new Fields(json, "op");
        return switch (f.string("op")) {
            case "node" -> {
                Json before = f.optional("before");
                Json after = f.optional("after");
                yield new PermissionChangeEvent.NodeChange(readSubjectRef(f.required("subject")),
                        before == null ? null : readNode(before), after == null ? null : readNode(after));
            }
            case "group" -> new PermissionChangeEvent.GroupChange(f.string("name"),
                    readGroupAttributes(f.optional("before")), readGroupAttributes(f.optional("after")));
            case "track" -> {
                Json before = f.optional("before");
                Json after = f.optional("after");
                yield new PermissionChangeEvent.TrackChange(f.string("name"),
                        before == null ? null : list(before, v -> string(v, "group")),
                        after == null ? null : list(after, v -> string(v, "group")));
            }
            case "primary_group" -> new PermissionChangeEvent.PrimaryGroupChange(readUuid(f.required("user")),
                    f.optionalString("before"), f.optionalString("after"));
            default -> throw invalid("unknown op");
        };
    }

    // ---- groups ----

    /**
     * @param group a group listing
     * @return its wire form
     */
    public static Json toJson(GroupInfo group) {
        Map<String, Json> m = new LinkedHashMap<>();
        m.put("name", new Str(group.name()));
        m.put("weight", Num.of(group.weight()));
        putIfPresent(m, "display_name", group.displayName());
        m.put("nodes", nodes(group.nodes()));
        return new Obj(m);
    }

    private static GroupInfo readGroupInfo(Json json) {
        Fields f = new Fields(json, "group");
        return new GroupInfo(f.string("name"), f.i32("weight"), f.optionalString("display_name"),
                readNodes(f.required("nodes")));
    }

    // ---- protocol messages ----

    /**
     * @param request a hello request
     * @return its wire form
     */
    public static Json toJson(HelloRequest request) {
        Map<String, Json> m = new LinkedHashMap<>();
        m.put("server", new Str(request.server()));
        m.put("revision", Num.of(request.revision()));
        m.put("client_version", new Str(request.clientVersion()));
        return new Obj(m);
    }

    /**
     * @param json a hello request
     * @return the request
     */
    public static HelloRequest readHelloRequest(Json json) {
        Fields f = new Fields(json, "hello");
        return new HelloRequest(f.string("server"), f.u64("revision"), f.string("client_version"));
    }

    /**
     * @param reply a hello reply
     * @return its wire form
     */
    public static Json toJson(HelloReply reply) {
        Map<String, Json> m = new LinkedHashMap<>();
        m.put("revision", Num.of(reply.revision()));
        m.put("protocol", Num.of(reply.protocol()));
        m.put("now", Num.of(reply.now()));
        Limits l = reply.limits();
        Map<String, Json> limits = new LinkedHashMap<>();
        limits.put("groups", Num.of(l.groups()));
        limits.put("group_nodes", Num.of(l.groupNodes()));
        limits.put("user_nodes", Num.of(l.userNodes()));
        limits.put("parents", Num.of(l.parents()));
        limits.put("contexts", Num.of(l.contexts()));
        limits.put("tracks", Num.of(l.tracks()));
        limits.put("compiled_entries", Num.of(l.compiledEntries()));
        limits.put("batch", Num.of(l.batch()));
        m.put("limits", new Obj(limits));
        m.put("external_contexts", strings(reply.externalContexts()));
        return new Obj(m);
    }

    /**
     * @param json a hello reply
     * @return the reply; absent limits take the proxy's defaults
     */
    public static HelloReply readHelloReply(Json json) {
        Fields f = new Fields(json, "hello reply");
        Fields l = new Fields(f.required("limits"), "limits");
        Limits d = Limits.DEFAULT;
        Limits limits = new Limits(l.sizeOr("groups", d.groups()), l.sizeOr("group_nodes", d.groupNodes()),
                l.sizeOr("user_nodes", d.userNodes()), l.sizeOr("parents", d.parents()),
                l.sizeOr("contexts", d.contexts()), l.sizeOr("tracks", d.tracks()),
                l.sizeOr("compiled_entries", d.compiledEntries()), l.sizeOr("batch", d.batch()));
        return new HelloReply(f.u64("revision"), f.u32("protocol"), f.u64("now"), limits,
                list(f.required("external_contexts"), v -> string(v, "context key")));
    }

    /**
     * @param request a subject request
     * @return its wire form
     */
    public static Json toJson(SubjectGetRequest request) {
        Map<String, Json> m = new LinkedHashMap<>();
        m.put("uuid", uuid(request.uuid()));
        putIfPresent(m, "server", request.server());
        return new Obj(m);
    }

    /**
     * @param json a subject request
     * @return the request
     */
    public static SubjectGetRequest readSubjectGetRequest(Json json) {
        Fields f = new Fields(json, "subject request");
        return new SubjectGetRequest(readUuid(f.required("uuid")), f.optionalString("server"));
    }

    /**
     * @param request a batch request
     * @return its wire form
     */
    public static Json toJson(SubjectBatchRequest request) {
        return new Obj(Map.of("uuids", uuids(request.uuids())));
    }

    /**
     * @param json a batch request
     * @return the request
     */
    public static SubjectBatchRequest readSubjectBatchRequest(Json json) {
        return new SubjectBatchRequest(list(new Fields(json, "batch request").required("uuids"), Codec::readUuid));
    }

    /**
     * @param reply a batch reply
     * @return its wire form
     */
    public static Json toJson(SubjectBatchReply reply) {
        Map<String, Json> m = new LinkedHashMap<>();
        m.put("revision", Num.of(reply.revision()));
        m.put("subjects", new Arr(reply.subjects().stream().map(Codec::toJson).toList()));
        return new Obj(m);
    }

    /**
     * @param json a batch reply
     * @return the reply
     */
    public static SubjectBatchReply readSubjectBatchReply(Json json) {
        Fields f = new Fields(json, "batch reply");
        return new SubjectBatchReply(f.u64("revision"), list(f.required("subjects"), Codec::readCompiledSubject));
    }

    /**
     * @param request a groups request
     * @return its wire form
     */
    public static Json toJson(GroupsGetRequest request) {
        Map<String, Json> m = new LinkedHashMap<>();
        if (request.names() != null) {
            m.put("names", strings(request.names()));
        }
        if (request.permissions()) {
            m.put("permissions", new Bool(true));
        }
        return new Obj(m);
    }

    /**
     * @param json a groups request
     * @return the request
     */
    public static GroupsGetRequest readGroupsGetRequest(Json json) {
        Fields f = new Fields(json, "groups request");
        Json names = f.optional("names");
        return new GroupsGetRequest(names == null ? null : list(names, v -> string(v, "group")), f.flag("permissions"));
    }

    /**
     * @param reply a groups reply
     * @return its wire form
     */
    public static Json toJson(GroupsReply reply) {
        Map<String, Json> m = new LinkedHashMap<>();
        m.put("revision", Num.of(reply.revision()));
        m.put("groups", new Arr(reply.groups().stream().map(Codec::toJson).toList()));
        return new Obj(m);
    }

    /**
     * @param json a groups reply
     * @return the reply
     */
    public static GroupsReply readGroupsReply(Json json) {
        Fields f = new Fields(json, "groups reply");
        return new GroupsReply(f.u64("revision"), list(f.required("groups"), Codec::readGroupInfo));
    }

    /**
     * @param request an apply request
     * @return its wire form
     */
    public static Json toJson(ApplyRequest request) {
        Map<String, Json> m = new LinkedHashMap<>();
        m.put("change", toJson(request.change()));
        if (request.actorUuid() != null) {
            m.put("actor_uuid", uuid(request.actorUuid()));
        }
        putIfPresent(m, "reason", request.reason());
        return new Obj(m);
    }

    /**
     * @param json an apply request
     * @return the request
     */
    public static ApplyRequest readApplyRequest(Json json) {
        Fields f = new Fields(json, "apply request");
        Json actor = f.optional("actor_uuid");
        return new ApplyRequest(readChange(f.required("change")), actor == null ? null : readUuid(actor),
                f.optionalString("reason"));
    }

    /**
     * @param permissions registered permissions
     * @return the {@code rift.perms.register} payload
     */
    public static Json toRegisterJson(List<RegisteredPermission> permissions) {
        return new Arr(permissions.stream().map(p -> {
            Map<String, Json> m = new LinkedHashMap<>();
            m.put("node", new Str(p.node()));
            putIfPresent(m, "description", p.description());
            m.put("default", new Str(p.defaultValue().value()));
            putIfPresent(m, "plugin", p.plugin());
            return (Json) new Obj(m);
        }).toList());
    }

    /**
     * @param json a {@code rift.perms.register} payload
     * @return the permissions
     */
    public static List<RegisteredPermission> readRegisterRequest(Json json) {
        return list(json, item -> {
            Fields f = new Fields(item, "registered permission");
            return new RegisteredPermission(f.string("node"), f.optionalString("description"),
                    new PermissionDefault(f.string("default")), f.optionalString("plugin"));
        });
    }

    /**
     * @param request a command request
     * @return its wire form
     */
    public static Json toJson(CommandRequest request) {
        Map<String, Json> m = new LinkedHashMap<>();
        if (request.actorUuid() != null) {
            m.put("actor_uuid", uuid(request.actorUuid()));
        }
        m.put("args", strings(request.args()));
        return new Obj(m);
    }

    /**
     * @param json a command request
     * @return the request
     */
    public static CommandRequest readCommandRequest(Json json) {
        Fields f = new Fields(json, "command request");
        Json actor = f.optional("actor_uuid");
        return new CommandRequest(actor == null ? null : readUuid(actor), list(f.required("args"), v -> string(v, "arg")));
    }

    /**
     * @param reply a command reply
     * @return its wire form
     */
    public static Json toJson(CommandReply reply) {
        Map<String, Json> m = new LinkedHashMap<>();
        m.put("revision", Num.of(reply.revision()));
        m.put("component", Json.parse(reply.component()));
        return new Obj(m);
    }

    /**
     * @param json a command reply
     * @return the reply, with the component as JSON text
     */
    public static CommandReply readCommandReply(Json json) {
        Fields f = new Fields(json, "command reply");
        return new CommandReply(f.u64("revision"), f.required("component").toString());
    }

    /**
     * @param report a context report
     * @return its wire form
     */
    public static Json toJson(ContextReport report) {
        Map<String, Json> m = new LinkedHashMap<>();
        m.put("uuid", uuid(report.uuid()));
        m.put("contexts", toJson(report.contexts()));
        return new Obj(m);
    }

    /**
     * @param json a context report
     * @return the report
     */
    public static ContextReport readContextReport(Json json) {
        Fields f = new Fields(json, "context report");
        return new ContextReport(readUuid(f.required("uuid")), readContextSet(f.required("contexts")));
    }

    /**
     * @param reply a reply envelope
     * @param data writes the data of a successful reply
     * @param <T> the data type
     * @return {@code {"ok": true, "data": ...}} or {@code {"ok": false, "error", "code"}}
     */
    public static <T> Json toJson(Reply<T> reply, Function<T, Json> data) {
        Map<String, Json> m = new LinkedHashMap<>();
        switch (reply) {
            case Ok<T> ok -> {
                m.put("ok", new Bool(true));
                m.put("data", data.apply(ok.data()));
            }
            case Err<T> err -> {
                m.put("ok", new Bool(false));
                m.put("error", new Str(err.error()));
                m.put("code", new Str(err.code().wireName()));
            }
        }
        return new Obj(m);
    }

    /**
     * @param json a reply envelope
     * @param data reads the data of a successful reply
     * @param <T> the data type
     * @return the reply; a missing or unknown code reads as {@link ErrorCode#INTERNAL}
     */
    public static <T> Reply<T> readReply(Json json, Function<Json, T> data) {
        Fields f = new Fields(json, "reply");
        if (f.bool("ok")) {
            Json value = f.optional("data");
            if (value == null) {
                throw invalid("a successful reply needs data");
            }
            return new Ok<>(data.apply(value));
        }
        String error = f.optionalString("error");
        return new Err<>(ErrorCode.fromWireName(f.optionalString("code")), error == null ? "" : error);
    }

    // ---- helpers ----

    private static Json uuid(UUID uuid) {
        return new Str(Uuids.toHex(uuid));
    }

    private static UUID readUuid(Json json) {
        return Uuids.parse(string(json, "uuid"));
    }

    private static Json uuids(List<UUID> uuids) {
        return new Arr(uuids.stream().map(Codec::uuid).toList());
    }

    private static Json strings(List<String> values) {
        return new Arr(values.stream().map(v -> (Json) new Str(v)).toList());
    }

    private static Json nullableString(String value) {
        return value == null ? Null.INSTANCE : new Str(value);
    }

    private static void putIfPresent(Map<String, Json> members, String name, String value) {
        if (value != null) {
            members.put(name, new Str(value));
        }
    }

    private static IllegalArgumentException invalid(String reason) {
        return new IllegalArgumentException("invalid permission message: " + reason);
    }

    private static Obj object(Json json, String what) {
        if (json instanceof Obj obj) {
            return obj;
        }
        throw invalid(what + " must be an object");
    }

    private static String string(Json json, String what) {
        if (json instanceof Str str) {
            return str.value();
        }
        throw invalid(what + " must be a string");
    }

    private static boolean bool(Json json, String what) {
        if (json instanceof Bool value) {
            return value.value();
        }
        throw invalid(what + " must be a boolean");
    }

    private static <T> List<T> list(Json json, Function<Json, T> item) {
        if (!(json instanceof Arr array)) {
            throw invalid("expected an array");
        }
        List<T> result = new ArrayList<>(array.items().size());
        for (Json value : array.items()) {
            result.add(item.apply(value));
        }
        return result;
    }

    private static BigDecimal integer(Json json, String what) {
        if (json instanceof Num num && num.value().scale() <= 0) {
            return num.value();
        }
        throw invalid(what + " must be an integer");
    }

    private static long unsigned64(Json json, String what) {
        BigDecimal value = integer(json, what);
        if (value.signum() < 0 || value.compareTo(BigDecimal.valueOf(Long.MAX_VALUE)) > 0) {
            throw invalid(what + " is out of range");
        }
        return value.longValueExact();
    }

    private static int int32(Json json, String what) {
        BigDecimal value = integer(json, what);
        if (value.compareTo(BigDecimal.valueOf(Integer.MIN_VALUE)) < 0
                || value.compareTo(BigDecimal.valueOf(Integer.MAX_VALUE)) > 0) {
            throw invalid(what + " is out of range");
        }
        return value.intValueExact();
    }

    private static int nonNegativeInt(Json json, String what) {
        int value = int32(json, what);
        if (value < 0) {
            throw invalid(what + " is out of range");
        }
        return value;
    }

    /** Typed access to an object's members with serde's missing-field rules. */
    private static final class Fields {
        private final Obj obj;
        private final String what;

        Fields(Json json, String what) {
            this.obj = object(json, what);
            this.what = what;
        }

        Json required(String name) {
            Json value = obj.get(name);
            if (value == null) {
                throw invalid(what + " is missing \"" + name + "\"");
            }
            return value;
        }

        /** An {@code Option} field: absent or {@code null} is null. */
        Json optional(String name) {
            Json value = obj.get(name);
            return value instanceof Null ? null : value;
        }

        /** A {@code #[serde(default)]} field: absent is null, {@code null} is a type error later. */
        Json defaulted(String name) {
            return obj.get(name);
        }

        String string(String name) {
            return Codec.string(required(name), what + "." + name);
        }

        String optionalString(String name) {
            Json value = optional(name);
            return value == null ? null : Codec.string(value, what + "." + name);
        }

        boolean bool(String name) {
            return Codec.bool(required(name), what + "." + name);
        }

        /** A {@code bool} defaulting to false. */
        boolean flag(String name) {
            Json value = defaulted(name);
            return value != null && Codec.bool(value, what + "." + name);
        }

        long u64(String name) {
            return unsigned64(required(name), what + "." + name);
        }

        OptionalLong optionalU64(String name) {
            Json value = optional(name);
            return value == null ? OptionalLong.empty() : OptionalLong.of(unsigned64(value, what + "." + name));
        }

        int i32(String name) {
            return int32(required(name), what + "." + name);
        }

        int u32(String name) {
            return nonNegativeInt(required(name), what + "." + name);
        }

        int size(String name) {
            return nonNegativeInt(required(name), what + "." + name);
        }

        int sizeOr(String name, int fallback) {
            Json value = defaulted(name);
            return value == null ? fallback : nonNegativeInt(value, what + "." + name);
        }

        ContextSet contextsOrEmpty(String name) {
            Json value = defaulted(name);
            return value == null ? ContextSet.empty() : readContextSet(value);
        }

        <T> List<T> defaultedList(String name, Function<Json, T> item) {
            Json value = defaulted(name);
            return value == null ? List.of() : list(value, item);
        }
    }
}
