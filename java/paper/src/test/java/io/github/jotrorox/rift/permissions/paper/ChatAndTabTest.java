package io.github.jotrorox.rift.permissions.paper;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.github.jotrorox.rift.permissions.test.InMemoryRiftPermissions;
import io.github.jotrorox.rift.permissions.test.MockBukkitPermissions;
import io.papermc.paper.event.player.AsyncChatEvent;
import java.util.Set;
import net.kyori.adventure.audience.Audience;
import net.kyori.adventure.text.Component;
import net.kyori.adventure.text.serializer.plain.PlainTextComponentSerializer;
import org.bukkit.plugin.Plugin;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.mockbukkit.mockbukkit.MockBukkit;
import org.mockbukkit.mockbukkit.ServerMock;
import org.mockbukkit.mockbukkit.entity.PlayerMock;

/** The chat renderer and the tab list entries on a mock server. */
class ChatAndTabTest {
    private ServerMock server;
    private Plugin plugin;
    private InMemoryRiftPermissions perms;
    private PlayerMock staff;
    private PlayerMock guest;

    @BeforeEach
    void setUp() {
        server = MockBukkit.mock();
        plugin = MockBukkit.createMockPlugin();
        staff = server.addPlayer("Alice");
        guest = server.addPlayer("bob");
        perms = MockBukkitPermissions.builder("survival")
                .group("default", group -> group.suffix(1, " &7(guest)"))
                .group("staff", group -> group.weight(100).prefix(100, "&c[Staff] ")
                        .permission(ChatFormatter.COLOR_PERMISSION, true))
                .user(staff.getUniqueId(), user -> user.parent("staff").primaryGroup("staff"))
                .build();
        MockBukkitPermissions.inject(staff, perms);
        MockBukkitPermissions.inject(guest, perms);
    }

    @AfterEach
    void tearDown() {
        MockBukkit.unmock();
    }

    private static String plain(Component component) {
        return PlainTextComponentSerializer.plainText().serialize(component);
    }

    private String say(PlayerMock player, String message) {
        AsyncChatEvent event = new AsyncChatEvent(true, player, Set.<Audience>of(),
                io.papermc.paper.chat.ChatRenderer.defaultRenderer(), Component.text(message), Component.text(message),
                null);
        new ChatFormatter(perms, PaperConfig.Chat.DEFAULT_FORMAT).onChat(event);
        return plain(event.renderer().render(player, player.displayName(), event.message(), Audience.empty()));
    }

    @Test
    void chatShowsTheSendersDecorations() {
        assertEquals("[Staff] Alice (guest): hello", say(staff, "hello"));
        assertEquals("bob (guest): &chello", say(guest, "&chello"));
        // rift.chat.color lets the codes through.
        assertEquals("[Staff] Alice (guest): hello", say(staff, "&chello"));
    }

    @Test
    void tabEntriesCarryDecorationsAndAnOrder() {
        TabList tab = new TabList(plugin, perms, new PaperConfig.Tab(true, PaperConfig.Tab.DEFAULT_FORMAT, true));
        tab.apply(staff);
        tab.apply(guest);
        assertEquals("[Staff] Alice (guest)", plain(staff.playerListName()));
        assertEquals("bob (guest)", plain(guest.playerListName()));
        assertTrue(staff.getPlayerListOrder() > guest.getPlayerListOrder());

        tab.restoreAll(server.getOnlinePlayers());
        assertEquals("Alice", plain(staff.playerListName()));
        assertEquals(0, staff.getPlayerListOrder());
    }

    @Test
    void anEmptyFormatLeavesNamesAlone() {
        staff.playerListName(Component.text("custom"));
        TabList tab = new TabList(plugin, perms, new PaperConfig.Tab(true, "", false));
        tab.apply(staff);
        assertEquals("custom", plain(staff.playerListName()));
        assertEquals(0, staff.getPlayerListOrder());
        tab.restoreAll(server.getOnlinePlayers());
        assertEquals("custom", plain(staff.playerListName()));
    }
}
