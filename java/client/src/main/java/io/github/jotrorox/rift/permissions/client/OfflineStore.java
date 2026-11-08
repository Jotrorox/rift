package io.github.jotrorox.rift.permissions.client;

import io.github.jotrorox.rift.permissions.CompiledSubject;
import io.github.jotrorox.rift.permissions.CompiledSubject.Decoration;
import io.github.jotrorox.rift.permissions.CompiledSubject.Entry;
import io.github.jotrorox.rift.permissions.CompiledSubject.EntryKind;
import io.github.jotrorox.rift.permissions.CompiledSubject.MetaEntry;
import io.github.jotrorox.rift.permissions.Node;
import io.github.jotrorox.rift.permissions.RiftPermissions;
import io.github.jotrorox.rift.permissions.client.Json.Obj;
import io.github.jotrorox.rift.permissions.client.Json.Str;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.AtomicMoveNotSupportedException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.UUID;

/** Loads and saves the files behind an {@link OfflinePolicy}. */
final class OfflineStore {
    private final OfflinePolicy policy;
    private final Map<UUID, CompiledSubject> snapshots;
    private final CompiledSubject template;

    private OfflineStore(OfflinePolicy policy, Map<UUID, CompiledSubject> snapshots, CompiledSubject template) {
        this.policy = policy;
        this.snapshots = snapshots;
        this.template = template;
    }

    /**
     * @throws IOException when a policy file exists but cannot be read or parsed; a missing
     *     snapshot file is an empty snapshot, a missing fallback file is an error
     */
    static OfflineStore load(OfflinePolicy policy) throws IOException {
        return switch (policy) {
            case OfflinePolicy.Deny deny -> new OfflineStore(deny, Map.of(), null);
            case OfflinePolicy.LastSnapshot last -> {
                Map<UUID, CompiledSubject> snapshots = new HashMap<>();
                if (Files.exists(last.file())) {
                    for (CompiledSubject subject : Codec.readSubjectBatchReply(parse(last.file())).subjects()) {
                        snapshots.put(subject.uuid(), subject);
                    }
                }
                yield new OfflineStore(last, Map.copyOf(snapshots), null);
            }
            case OfflinePolicy.FallbackFile fallback -> new OfflineStore(fallback, Map.of(), template(fallback.file()));
        };
    }

    private static Json parse(Path file) throws IOException {
        try {
            return Json.parse(Files.readString(file, StandardCharsets.UTF_8));
        } catch (IllegalArgumentException e) {
            throw new IOException(file + ": " + e.getMessage(), e);
        }
    }

    /** The fallback nodes as a rank-0 subject; inherit nodes become memberships only. */
    private static CompiledSubject template(Path file) throws IOException {
        Json json = parse(file);
        try {
            if (!(json instanceof Obj obj) || obj.get("nodes") == null) {
                throw new IllegalArgumentException("expected {\"primary_group\": ..., \"nodes\": [...]}");
            }
            List<Entry> entries = new ArrayList<>();
            List<Decoration> prefix = new ArrayList<>();
            List<Decoration> suffix = new ArrayList<>();
            List<MetaEntry> meta = new ArrayList<>();
            for (Node node : Codec.readNodes(obj.get("nodes"))) {
                switch (node.payload()) {
                    case Node.Permission p ->
                            entries.add(new Entry(EntryKind.PERMISSION, p.key(), p.value(), node.contexts(), 0, node.expiresAt()));
                    case Node.Inherit p when p.value() ->
                            entries.add(new Entry(EntryKind.GROUP, p.group(), true, node.contexts(), 0, node.expiresAt()));
                    case Node.Prefix p -> prefix.add(new Decoration(p.priority(), p.text(), node.contexts(), 0, node.expiresAt()));
                    case Node.Suffix p -> suffix.add(new Decoration(p.priority(), p.text(), node.contexts(), 0, node.expiresAt()));
                    case Node.Meta p -> meta.add(new MetaEntry(p.key(), p.value(), node.contexts(), 0, node.expiresAt()));
                    default -> throw new IllegalArgumentException(
                            "fallback nodes are permission, positive inherit, prefix, suffix or meta nodes");
                }
            }
            String primary = obj.get("primary_group") instanceof Str str ? str.value() : RiftPermissions.DEFAULT_GROUP;
            return new CompiledSubject(0, new UUID(0, 0), primary, entries, prefix, suffix, meta);
        } catch (IllegalArgumentException e) {
            throw new IOException(file + ": " + e.getMessage(), e);
        }
    }

    /** The policy's subject for a player without a held snapshot, or null. */
    CompiledSubject subject(UUID uuid) {
        if (template != null) {
            return new CompiledSubject(0, uuid, template.primaryGroup(), template.entries(), template.prefix(),
                    template.suffix(), template.meta());
        }
        return snapshots.get(uuid);
    }

    /** Persist the replica for {@code last_snapshot}; other policies write nothing. */
    void save(long revision, List<CompiledSubject> subjects) throws IOException {
        if (!(policy instanceof OfflinePolicy.LastSnapshot last)) {
            return;
        }
        Path file = last.file().toAbsolutePath();
        Files.createDirectories(file.getParent());
        Path temporary = file.resolveSibling(file.getFileName() + ".tmp");
        Files.writeString(temporary, Codec.toJson(new Protocol.SubjectBatchReply(revision, subjects)).toString(),
                StandardCharsets.UTF_8);
        try {
            Files.move(temporary, file, StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE);
        } catch (AtomicMoveNotSupportedException e) {
            Files.move(temporary, file, StandardCopyOption.REPLACE_EXISTING);
        }
    }
}
