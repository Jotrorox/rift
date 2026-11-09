package io.github.jotrorox.rift.permissions.paper;

import java.lang.reflect.Field;
import java.util.List;
import org.bukkit.entity.Player;
import org.bukkit.permissions.PermissibleBase;
import org.bukkit.permissions.PermissionAttachment;

/**
 * Swaps a player's {@code PermissibleBase} through reflection on {@code CraftHumanEntity#perm}, the
 * seam LuckPerms uses too. Attachments of the replaced permissible move to the new one when the
 * runtime allows rewriting their owner; otherwise they stay behind, which only matters for players
 * who were online before RiftPerms enabled.
 */
final class PermissibleInjector {
    private static final Field ATTACHMENTS = field(PermissibleBase.class, "attachments");
    private static final Field ATTACHMENT_OWNER = field(PermissionAttachment.class, "permissible");

    private PermissibleInjector() {}

    private static Field field(Class<?> type, String name) {
        try {
            Field field = type.getDeclaredField(name);
            field.setAccessible(true);
            return field;
        } catch (ReflectiveOperationException | RuntimeException e) {
            return null;
        }
    }

    private static Field permField(Player player) throws NoSuchFieldException {
        for (Class<?> type = player.getClass(); type != null; type = type.getSuperclass()) {
            try {
                Field field = type.getDeclaredField("perm");
                field.setAccessible(true);
                return field;
            } catch (NoSuchFieldException e) {
                // Keep walking towards CraftHumanEntity.
            }
        }
        throw new NoSuchFieldException("perm");
    }

    /**
     * Install {@code permissible} on {@code player}.
     *
     * @return the replaced permissible, for {@link #restore}
     */
    static PermissibleBase inject(Player player, RiftPermissible permissible) throws ReflectiveOperationException {
        Field perm = permField(player);
        PermissibleBase previous = (PermissibleBase) perm.get(player);
        if (previous != null && ATTACHMENTS != null && ATTACHMENT_OWNER != null) {
            @SuppressWarnings("unchecked")
            List<PermissionAttachment> attachments = (List<PermissionAttachment>) ATTACHMENTS.get(previous);
            for (PermissionAttachment attachment : List.copyOf(attachments)) {
                ATTACHMENT_OWNER.set(attachment, permissible);
                permissible.adopt(attachment);
                attachments.remove(attachment);
            }
        }
        perm.set(player, permissible);
        permissible.recalculatePermissions();
        return previous;
    }

    /** Put back the permissible that {@link #inject} replaced, with the current attachments. */
    static void restore(Player player, RiftPermissible current, PermissibleBase previous)
            throws ReflectiveOperationException {
        Field perm = permField(player);
        if (perm.get(player) != current) {
            return;
        }
        if (ATTACHMENTS != null && ATTACHMENT_OWNER != null) {
            @SuppressWarnings("unchecked")
            List<PermissionAttachment> attachments = (List<PermissionAttachment>) ATTACHMENTS.get(previous);
            for (PermissionAttachment attachment : current.attachments()) {
                ATTACHMENT_OWNER.set(attachment, previous);
                attachments.add(attachment);
            }
        }
        perm.set(player, previous);
        previous.recalculatePermissions();
    }
}
