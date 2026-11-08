package io.github.jotrorox.rift.permissions.paper;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.github.jotrorox.rift.permissions.Node;
import io.github.jotrorox.rift.permissions.test.InMemoryRiftPermissions;
import io.github.jotrorox.rift.permissions.test.MockBukkitPermissions;
import java.util.Map;
import java.util.stream.Collectors;
import org.bukkit.Bukkit;
import org.bukkit.GameMode;
import org.bukkit.permissions.Permission;
import org.bukkit.permissions.PermissionAttachment;
import org.bukkit.permissions.PermissionAttachmentInfo;
import org.bukkit.permissions.PermissionDefault;
import org.bukkit.plugin.Plugin;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.mockbukkit.mockbukkit.MockBukkit;
import org.mockbukkit.mockbukkit.ServerMock;
import org.mockbukkit.mockbukkit.entity.PlayerMock;

class RiftPermissibleTest {
    private ServerMock server;
    private Plugin plugin;
    private PlayerMock player;
    private RiftPermissible permissible;

    @BeforeEach
    void setUp() {
        server = MockBukkit.mock();
        plugin = MockBukkit.createMockPlugin();
        player = server.addPlayer();
        InMemoryRiftPermissions perms = MockBukkitPermissions.builder("survival")
                .group("default", group -> group.permission("essentials.*", true)
                        .permission("essentials.nuke", false)
                        .node(Node.permission("worldedit.wand", true).context("gamemode", "creative")))
                .build();
        permissible = new RiftPermissible(player, perms);
    }

    @AfterEach
    void tearDown() {
        MockBukkit.unmock();
    }

    @Test
    void answersFromTheReplicaInCurrentContexts() {
        assertTrue(permissible.hasPermission("essentials.fly"));
        assertTrue(permissible.hasPermission("Essentials.Fly"), "nodes are case-insensitive");
        assertFalse(permissible.hasPermission("essentials.nuke"));
        assertTrue(permissible.isPermissionSet("essentials.nuke"));
        assertTrue(permissible.hasPermission("group.default"));
        assertFalse(permissible.hasPermission("worldedit.wand"));
        player.setGameMode(GameMode.CREATIVE);
        assertTrue(permissible.hasPermission("worldedit.wand"));
    }

    @Test
    void undefinedFallsBackToRegisteredDefaultsThenOp() {
        Permission everyone = new Permission("kits.starter", PermissionDefault.TRUE);
        Bukkit.getPluginManager().addPermission(everyone);
        assertFalse(permissible.isPermissionSet("kits.starter"));
        assertTrue(permissible.hasPermission("kits.starter"));
        assertTrue(permissible.hasPermission(everyone));
        assertFalse(permissible.hasPermission("unregistered.node"));
        player.setOp(true);
        assertTrue(permissible.hasPermission("unregistered.node"), "Bukkit's default for unknown nodes is op");
    }

    @Test
    void attachmentsWinAndExpandChildren() {
        Permission parent = new Permission("kit.all", PermissionDefault.FALSE, Map.of("kit.a", true, "kit.b", false));
        Bukkit.getPluginManager().addPermission(parent);
        PermissionAttachment attachment = permissible.addAttachment(plugin, "essentials.nuke", true);
        attachment.setPermission("kit.all", true);
        assertTrue(permissible.hasPermission("essentials.nuke"), "attachments are consulted first");
        assertTrue(permissible.hasPermission("kit.a"));
        assertFalse(permissible.hasPermission("kit.b"));
        attachment.remove();
        assertFalse(permissible.hasPermission("essentials.nuke"));
        assertFalse(permissible.hasPermission("kit.a"));
    }

    @Test
    void effectivePermissionsAreMaterialised() {
        permissible.addAttachment(plugin, "plugin.temp", true);
        Map<String, Boolean> effective = permissible.getEffectivePermissions().stream()
                .collect(Collectors.toMap(PermissionAttachmentInfo::getPermission, PermissionAttachmentInfo::getValue));
        assertEquals(Map.of("essentials.*", true, "essentials.nuke", false, "group.default", true, "plugin.temp", true),
                effective);
    }
}
