package io.github.jotrorox.rift.permissions.paper;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.github.jotrorox.rift.permissions.GroupInfo;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;
import java.util.Map;
import net.kyori.adventure.text.Component;
import net.kyori.adventure.text.event.ClickEvent;
import net.kyori.adventure.text.format.NamedTextColor;
import net.kyori.adventure.text.format.TextColor;
import net.kyori.adventure.text.serializer.plain.PlainTextComponentSerializer;
import org.junit.jupiter.api.Test;

/** Format lines and tab list orders, without a server. */
class DecorationsTest {
    private static final String CHAT = PaperConfig.Chat.DEFAULT_FORMAT;

    private static String plain(Component component) {
        return PlainTextComponentSerializer.plainText().serialize(component);
    }

    /** The colour each character is shown in, as the client resolves it through the parents. */
    private static TextColor colourOf(Component line, String text) {
        List<TextColor> found = new ArrayList<>();
        walk(line, null, text, found);
        assertEquals(1, found.size(), () -> "\"" + text + "\" in " + line);
        return found.get(0);
    }

    private static boolean walk(Component component, TextColor inherited, String text, List<TextColor> found) {
        TextColor colour = component.color() != null ? component.color() : inherited;
        if (component instanceof net.kyori.adventure.text.TextComponent own && own.content().contains(text)) {
            found.add(colour);
            return true;
        }
        for (Component child : component.children()) {
            if (walk(child, colour, text, found)) {
                return true;
            }
        }
        return false;
    }

    private static Component chat(String prefix, String suffix, Component message) {
        return Decorations.render(CHAT, prefix, suffix, Map.of(
                "name", Component.text("Alice"),
                "display_name", Component.text("Ally"),
                "group", Component.text("vip"),
                "message", message));
    }

    @Test
    void aLineCarriesPrefixNameSuffixAndMessage() {
        Component line = chat("&6[VIP] ", " &b*", Component.text("hello"));
        assertEquals("[VIP] Alice *: hello", plain(line));
        // The prefix colours the name, as in every legacy chat plugin; &r frees the message.
        assertEquals(NamedTextColor.GOLD, colourOf(line, "[VIP] "));
        assertEquals(NamedTextColor.GOLD, colourOf(line, "Alice"));
        assertEquals(NamedTextColor.AQUA, colourOf(line, "*"));
        assertEquals(NamedTextColor.GRAY, colourOf(line, ": "));
        assertNull(colourOf(line, "hello"));
    }

    @Test
    void missingDecorationsLeaveNothingBehind() {
        assertEquals("Alice: hello", plain(chat(null, null, Component.text("hello"))));
        assertEquals("[vip] Ally {unknown}", plain(Decorations.render("&7[{group}] {display_name} {unknown}",
                null, null, Map.of("group", Component.text("vip"), "display_name", Component.text("Ally")))));
        assertEquals(TextColor.color(0xffaa00),
                colourOf(Decorations.render("{prefix}{name}", "&#ffaa00+ ", null, Map.of("name", Component.text("Alice"))),
                        "Alice"));
    }

    @Test
    void whatAPlayerTypedIsNeverParsed() {
        // Colour codes and placeholders in the message, the name and the display name stay text.
        Component line = Decorations.render(CHAT, "&6[VIP] ", null, Map.of(
                "name", Component.text("&cEve{message}"),
                "message", Component.text("&4&lred {name} {prefix} {message}")));
        assertEquals("[VIP] &cEve{message}: &4&lred {name} {prefix} {message}", plain(line));
        assertNull(colourOf(line, "&4&lred"));
        // A component another plugin built keeps what it carries; nothing is added to it.
        Component link = Component.text("click").clickEvent(ClickEvent.openUrl("https://example.org"));
        assertTrue(chat(null, null, link).toString().contains("https://example.org"));
        assertEquals("Alice: click", plain(chat(null, null, link)));
    }

    @Test
    void colouredChatHasColoursAndNothingElse() {
        Component message = Decorations.coloured("&chi <click:run_command:/op me>there</click> https://example.org");
        assertEquals("hi <click:run_command:/op me>there</click> https://example.org", plain(message));
        assertEquals(NamedTextColor.RED, colourOf(message, "hi "));
        assertTrue(clickEvents(message).isEmpty(), message.toString());
    }

    private static List<ClickEvent> clickEvents(Component component) {
        List<ClickEvent> events = new ArrayList<>();
        if (component.clickEvent() != null) {
            events.add(component.clickEvent());
        }
        component.children().forEach(child -> events.addAll(clickEvents(child)));
        return events;
    }

    @Test
    void heavierPrimaryGroupsComeFirstInTheTabList() {
        List<GroupInfo> groups = List.of(
                new GroupInfo("default", 0, null, List.of()),
                new GroupInfo("muted", -5, null, List.of()),
                new GroupInfo("vip", 10, null, List.of()),
                new GroupInfo("mod", 50, null, List.of()),
                new GroupInfo("helper", 50, null, List.of()),
                new GroupInfo("admin", Integer.MAX_VALUE, null, List.of()),
                new GroupInfo("jail", Integer.MIN_VALUE, null, List.of()));
        // The client sorts by order, highest first, and then by name.
        record Entry(String name, String group) {}
        List<Entry> tab = new ArrayList<>(List.of(
                new Entry("Zoe", "vip"), new Entry("bob", "default"), new Entry("Carol", "admin"),
                new Entry("dave", "mod"), new Entry("Alice", "helper"), new Entry("Erin", "muted"),
                new Entry("Frank", "jail"), new Entry("Gus", "deleted")));
        tab.sort(Comparator.comparingInt((Entry entry) -> -Decorations.listOrder(entry.group(), groups))
                .thenComparing(Entry::name, String::compareToIgnoreCase));
        assertEquals(List.of("Carol", "Alice", "dave", "Zoe", "bob", "Gus", "Erin", "Frank"),
                tab.stream().map(Entry::name).toList());
        for (GroupInfo group : groups) {
            assertTrue(Decorations.listOrder(group.name(), groups) >= 0, group.name());
        }
        // Before the group table arrives everyone is equal.
        assertEquals(0, Decorations.listOrder("admin", List.of()));
    }
}
