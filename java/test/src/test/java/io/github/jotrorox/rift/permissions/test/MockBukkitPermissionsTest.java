package io.github.jotrorox.rift.permissions.test;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.github.jotrorox.rift.permissions.Node;
import io.github.jotrorox.rift.permissions.RiftPermissions;
import org.bukkit.Bukkit;
import org.bukkit.GameMode;
import org.bukkit.plugin.Plugin;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.mockbukkit.mockbukkit.MockBukkit;
import org.mockbukkit.mockbukkit.ServerMock;
import org.mockbukkit.mockbukkit.entity.PlayerMock;

class MockBukkitPermissionsTest {
    private ServerMock server;
    private InMemoryRiftPermissions perms;

    @BeforeEach
    void setUp() {
        server = MockBukkit.mock();
        perms = MockBukkitPermissions.builder("survival")
                .group("default", group -> group.permission("kits.starter", true)
                        .node(Node.permission("fly", true).context("gamemode", "creative")))
                .build();
    }

    @AfterEach
    void tearDown() {
        MockBukkitPermissions.uninstall(perms);
        MockBukkit.unmock();
    }

    @Test
    void installsTheServiceAndAnswersHasPermission() {
        Plugin plugin = MockBukkit.createMockPlugin();
        MockBukkitPermissions.install(plugin, perms);
        assertSame(perms, Bukkit.getServicesManager().load(RiftPermissions.class));
        assertSame(perms, RiftPermissions.get());

        PlayerMock player = server.addPlayer();
        MockBukkitPermissions.inject(player, perms);
        assertTrue(player.hasPermission("kits.starter"));
        assertFalse(player.hasPermission("fly"), "fly needs gamemode=creative");
        player.setGameMode(GameMode.CREATIVE);
        assertTrue(player.hasPermission("fly"));
        assertEquals("survival", perms.contexts().of(player).get("server").orElseThrow());

        player.addAttachment(plugin, "kits.starter", false);
        assertFalse(player.hasPermission("kits.starter"), "attachments win, as in vanilla Bukkit");
        assertFalse(player.hasPermission("unset.node"), "undefined falls back to the Bukkit default");
        player.setOp(true);
        assertTrue(player.hasPermission("unset.node"), "the default for unregistered nodes is op");
    }
}
