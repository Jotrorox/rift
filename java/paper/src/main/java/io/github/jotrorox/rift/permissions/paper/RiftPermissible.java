package io.github.jotrorox.rift.permissions.paper;

import io.github.jotrorox.rift.permissions.CompiledSubject;
import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.RiftPermissions;
import io.github.jotrorox.rift.permissions.Tristate;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Objects;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CopyOnWriteArrayList;
import org.bukkit.Bukkit;
import org.bukkit.entity.Player;
import org.bukkit.permissions.PermissibleBase;
import org.bukkit.permissions.Permission;
import org.bukkit.permissions.PermissionAttachment;
import org.bukkit.permissions.PermissionAttachmentInfo;
import org.bukkit.plugin.Plugin;

/**
 * The permissible injected into every player. Order of answers:
 *
 * <ol>
 *   <li>permission attachments, as in vanilla Bukkit, because plugins grant themselves temporary
 *       permissions through them;
 *   <li>the replica, in the player's current contexts;
 *   <li>for undefined nodes, the registered permission's default, and for unregistered nodes the
 *       Bukkit default ({@code op}).
 * </ol>
 */
final class RiftPermissible extends PermissibleBase {
    private final Player player;
    private final RiftPermissions permissions;
    private final List<PermissionAttachment> attachments = new CopyOnWriteArrayList<>();
    private volatile Map<String, AttachedValue> attached = Map.of();

    private record AttachedValue(boolean value, PermissionAttachment attachment) {}

    RiftPermissible(Player player, RiftPermissions permissions) {
        super(player);
        this.player = player;
        this.permissions = permissions;
    }

    private static String normalize(String name) {
        return name.toLowerCase(Locale.ROOT);
    }

    private Tristate rift(String node) {
        return permissions.check(player.getUniqueId(), node, permissions.contexts().of(player));
    }

    private Tristate value(String name) {
        String node = normalize(name);
        AttachedValue fromAttachment = attached.get(node);
        return fromAttachment != null ? Tristate.of(fromAttachment.value()) : rift(node);
    }

    @Override
    public boolean isPermissionSet(String name) {
        return value(name).isDefined();
    }

    @Override
    public boolean isPermissionSet(Permission permission) {
        return isPermissionSet(permission.getName());
    }

    @Override
    public boolean hasPermission(String name) {
        Tristate answer = value(name);
        if (answer.isDefined()) {
            return answer.asBoolean(false);
        }
        Permission registered = Bukkit.getPluginManager().getPermission(normalize(name));
        return registered != null
                ? registered.getDefault().getValue(isOp())
                : Permission.DEFAULT_PERMISSION.getValue(isOp());
    }

    @Override
    public boolean hasPermission(Permission permission) {
        Tristate answer = value(permission.getName());
        return answer.isDefined() ? answer.asBoolean(false) : permission.getDefault().getValue(isOp());
    }

    @Override
    public PermissionAttachment addAttachment(Plugin plugin) {
        checkEnabled(plugin);
        PermissionAttachment attachment = new PermissionAttachment(plugin, this);
        attachments.add(attachment);
        recalculatePermissions();
        return attachment;
    }

    @Override
    public PermissionAttachment addAttachment(Plugin plugin, String name, boolean value) {
        PermissionAttachment attachment = addAttachment(plugin);
        attachment.setPermission(name, value);
        return attachment;
    }

    @Override
    public PermissionAttachment addAttachment(Plugin plugin, int ticks) {
        PermissionAttachment attachment = addAttachment(plugin);
        // The entity scheduler keeps this safe on Folia, where the Bukkit scheduler is absent.
        if (player.getScheduler().runDelayed(plugin, task -> attachment.remove(), null, Math.max(1, ticks)) == null) {
            attachment.remove();
        }
        return attachment;
    }

    @Override
    public PermissionAttachment addAttachment(Plugin plugin, String name, boolean value, int ticks) {
        PermissionAttachment attachment = addAttachment(plugin, ticks);
        attachment.setPermission(name, value);
        return attachment;
    }

    /** Adopt an attachment from the permissible this one replaced. */
    void adopt(PermissionAttachment attachment) {
        attachments.add(attachment);
    }

    List<PermissionAttachment> attachments() {
        return List.copyOf(attachments);
    }

    @Override
    public void removeAttachment(PermissionAttachment attachment) {
        Objects.requireNonNull(attachment, "attachment");
        if (!attachments.remove(attachment)) {
            throw new IllegalArgumentException("Given attachment is not part of Permissible object " + player);
        }
        if (attachment.getRemovalCallback() != null) {
            attachment.getRemovalCallback().attachmentRemoved(attachment);
        }
        recalculatePermissions();
    }

    /** Rebuild the attachment values, children included, as Bukkit's own permissible does. */
    @Override
    public void recalculatePermissions() {
        if (attachments == null) {
            return; // Called by the superclass constructor before this one ran.
        }
        Map<String, AttachedValue> values = new HashMap<>();
        for (PermissionAttachment attachment : attachments) {
            children(attachment.getPermissions(), false, attachment, values);
        }
        attached = Map.copyOf(values);
    }

    private static void children(Map<String, Boolean> permissions, boolean invert, PermissionAttachment attachment,
            Map<String, AttachedValue> values) {
        for (Map.Entry<String, Boolean> entry : permissions.entrySet()) {
            String name = normalize(entry.getKey());
            boolean value = entry.getValue() ^ invert;
            values.put(name, new AttachedValue(value, attachment));
            Permission permission = Bukkit.getPluginManager().getPermission(name);
            if (permission != null) {
                children(permission.getChildren(), !value, attachment, values);
            }
        }
    }

    @Override
    public void clearPermissions() {
        attached = Map.of();
    }

    /** Every node the player's snapshot decides in the current contexts, plus attachments. */
    @Override
    public Set<PermissionAttachmentInfo> getEffectivePermissions() {
        Map<String, PermissionAttachmentInfo> result = new ConcurrentHashMap<>();
        CompiledSubject subject = permissions.compiled(player.getUniqueId()).orElse(null);
        if (subject != null) {
            ContextSet contexts = permissions.contexts().of(player);
            for (CompiledSubject.Entry entry : subject.entries()) {
                String node = entry.kind() == CompiledSubject.EntryKind.GROUP ? "group." + entry.key() : entry.key();
                if (!result.containsKey(node)) {
                    Tristate answer = permissions.check(player.getUniqueId(), node, contexts);
                    if (answer.isDefined()) {
                        result.put(node, new PermissionAttachmentInfo(this, node, null, answer.asBoolean(false)));
                    }
                }
            }
        }
        attached.forEach((node, value) ->
                result.put(node, new PermissionAttachmentInfo(this, node, value.attachment(), value.value())));
        return new HashSet<>(result.values());
    }

    private static void checkEnabled(Plugin plugin) {
        Objects.requireNonNull(plugin, "plugin");
        if (!plugin.isEnabled()) {
            throw new IllegalArgumentException("Plugin " + plugin.getPluginMeta().getDisplayName() + " is disabled");
        }
    }
}
