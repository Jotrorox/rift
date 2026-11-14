package io.github.jotrorox.rift.permissions.paper;

import io.github.jotrorox.rift.permissions.GroupInfo;
import java.util.List;
import java.util.Map;
import java.util.TreeSet;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import net.kyori.adventure.text.Component;
import net.kyori.adventure.text.serializer.legacy.LegacyComponentSerializer;

/**
 * The pure half of the chat formatter and the tab list: a format line with {@code {placeholders}}
 * becomes a component, and a primary group becomes a player list order.
 *
 * <p>Formats, prefixes and suffixes are legacy text ({@code &6}, {@code &l}, {@code &#ffaa00}), the
 * syntax Rift's prefixes are written in and Vault hands out. Everything a player controls (their
 * name, their display name, what they typed) is inserted as a finished component and never parsed,
 * so it cannot bring colours, click events or other placeholders with it.
 */
final class Decorations {
    static final LegacyComponentSerializer LEGACY =
            LegacyComponentSerializer.builder().character('&').hexColors().build();
    private static final Pattern PLACEHOLDER = Pattern.compile("\\{([a-z_]+)}");

    private Decorations() {}

    /**
     * @param format legacy text with {@code {prefix}}, {@code {suffix}} and the keys of {@code parts}
     * @param prefix the winning prefix as legacy text, or null
     * @param suffix the winning suffix as legacy text, or null
     * @param parts the component behind every other placeholder; unknown placeholders stay as typed
     * @return the line, in which a part inherits the colour in effect where its placeholder stands
     */
    static Component render(String format, String prefix, String suffix, Map<String, Component> parts) {
        // Prefix and suffix join the text before it is parsed, so "&6[VIP] " colours the name after
        // it as it does in every legacy chat plugin.
        Matcher matcher = PLACEHOLDER.matcher(format);
        StringBuilder text = new StringBuilder();
        while (matcher.find()) {
            String replacement = switch (matcher.group(1)) {
                case "prefix" -> prefix == null ? "" : prefix;
                case "suffix" -> suffix == null ? "" : suffix;
                default -> matcher.group();
            };
            matcher.appendReplacement(text, Matcher.quoteReplacement(replacement));
        }
        matcher.appendTail(text);
        Component line = LEGACY.deserialize(text.toString());
        // One pass for all parts: a replacement is not searched again, so "{name}" typed into chat
        // stays what the player typed.
        return line.replaceText(config -> config.match(PLACEHOLDER).replacement((match, original) -> {
            Component part = parts.get(match.group(1));
            return part != null ? part : original;
        }));
    }

    /**
     * What a player typed, with its legacy colour codes applied: for players allowed to colour their
     * chat. Legacy text has colours and decorations only, so nothing clickable can come out of it.
     *
     * @param typed the message as plain text
     * @return the coloured message
     */
    static Component coloured(String typed) {
        return LEGACY.deserialize(typed);
    }

    /**
     * The player list order of a primary group. The client lists higher orders first and equal ones
     * by name. Orders are kept non-negative whatever the weights are: the order is the position of
     * the group's weight among all weights in the table, 0 for the lightest.
     *
     * @param primaryGroup the player's primary group
     * @param groups the group table; a group it does not list weighs 0
     * @return an order that is higher for a heavier group
     */
    static int listOrder(String primaryGroup, List<GroupInfo> groups) {
        int weight = 0;
        TreeSet<Integer> weights = new TreeSet<>();
        weights.add(0);
        for (GroupInfo group : groups) {
            weights.add(group.weight());
            if (group.name().equals(primaryGroup)) {
                weight = group.weight();
            }
        }
        return weights.headSet(weight).size();
    }
}
