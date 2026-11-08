package io.github.jotrorox.rift.permissions.paper;

import io.github.jotrorox.rift.permissions.Node;
import io.github.jotrorox.rift.permissions.client.Protocol.PermissionDefault;
import io.github.jotrorox.rift.permissions.client.Protocol.RegisteredPermission;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import org.bukkit.permissions.Permission;
import org.bukkit.plugin.Plugin;

/** Collects the permissions every loaded plugin declares, for {@code rift.perms.register}. */
final class PermissionRegistrar {
    private PermissionRegistrar() {}

    static List<RegisteredPermission> collect(Plugin[] plugins) {
        Map<String, RegisteredPermission> byNode = new LinkedHashMap<>();
        for (Plugin plugin : plugins) {
            for (Permission permission : plugin.getPluginMeta().getPermissions()) {
                String node;
                try {
                    node = Node.normalizePattern(permission.getName());
                } catch (IllegalArgumentException e) {
                    continue; // Rift cannot store it, so there is nothing to describe.
                }
                String description = permission.getDescription();
                byNode.putIfAbsent(node, new RegisteredPermission(node,
                        description == null || description.isBlank() ? null : description,
                        switch (permission.getDefault()) {
                            case TRUE -> PermissionDefault.TRUE;
                            case FALSE -> PermissionDefault.FALSE;
                            case OP -> PermissionDefault.OP;
                            case NOT_OP -> PermissionDefault.NOT_OP;
                        },
                        plugin.getName()));
            }
        }
        return List.copyOf(byNode.values());
    }
}
