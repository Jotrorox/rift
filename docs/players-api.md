# Online players API

`rift.players` lists the players who are in a world on this Rift process,
queues actions for them (messages, titles, boss bars, kicks, transfers) and
reads the player directory. This page describes the Lua API of
[`rift.lua`](extensions.md) and [Lua plugins](plugins-lua.md). WebAssembly
plugins call the same functions through the `players` interface of the
[host API](plugins.md#host-api), and bus services through the
[`rift.players.*` subjects](messaging-subjects.md#player-api).

```lua
rift.config.plugins.jobs.announcements = {
    every_ms = 300000,
    run = function()
        rift.players.broadcast("Vote for the network at example.com/vote")
    end,
}
rift.config.plugins.commands.alert = {
    permission = "network.staff",
    run = function(ctx)
        local sent = 0
        for _, player in ipairs(rift.players.list()) do
            if rift.permissions.check(player.uuid, "network.staff") == true
                and rift.players.message(player.uuid, "[Staff] " .. ctx.name .. ": " .. ctx.args)
            then
                sent = sent + 1
            end
        end
        return { message = "Alert sent to " .. sent .. " staff members." }
    end,
}
```

In `rift.lua` the API works in callbacks, jobs, and (when `rift.config.plugins`
is set) `on_message` and `on_http`, so a messaging handler can relay
cross-server chat or an HTTP endpoint can kick a player.

## Functions

- A `player` argument is a lowercase UUID (32 hex digits, no hyphens) or a
  case-insensitive name.
- A `server` argument names a configured backend or service group; a group
  matches all its instances.
- `text` and `reason` are a string or a chat component table
  ([rich text](#rich-text)).

| Function | Returns | Notes |
| --- | --- | --- |
| `rift.players.list(server)` | Array of [player tables](#player-tables) | `server` is optional |
| `rift.players.get(player)` | Player table, or nil when the player is offline | |
| `rift.players.names(prefix)` | Sorted array of player names | `prefix` (optional) keeps names starting with it, ignoring ASCII case |
| `rift.players.message(player, text)` | Boolean: a chat message was queued | |
| `rift.players.action_bar(player, text)` | Boolean: action-bar text was queued | |
| `rift.players.title(player, title)` | Boolean: a title was queued | `title` is text or an [options table](#titles) |
| `rift.players.tab_list(player, options)` | Boolean: a tab list header and footer were queued | Omit `options` to restore the configured ones |
| `rift.players.boss_bar(player, name, options)` | Boolean: a boss bar was queued | Omit `options` to remove the bar called `name` |
| `rift.players.dialog(player, dialog)` | Boolean: a [dialog](dialogs.md) was queued | False for players older than 1.21.6 |
| `rift.players.clear_dialog(player)` | Boolean: closing Rift's dialog was queued | |
| `rift.players.kick(player, reason)` | Boolean: a disconnect with this reason was queued | |
| `rift.players.transfer(player, server)` | Boolean: a transfer request was queued | |
| `rift.players.broadcast(text, server)` | Number of players a chat message was queued for | `server` is optional |
| `rift.players.lookup(target)` | A directory record, or nil for a player Rift never saw | `target` is a UUID or a name |
| `rift.players.search(prefix, limit)` | Array of directory records, most recently seen first | `limit` is 1–100 (default 20) |
| `rift.players.set_language(target, language)` | Boolean: the player is known and the language was stored | `language` nil goes back to the client's locale. Counts as an action call |

Invalid arguments, unknown servers and exceeded budgets fail the callback.

The last three use the directory of every player who joined, online or not. A
record is a table
`{ uuid, name, first_seen, last_seen, last_ip, last_server, online, language }`;
see
[the player directory and languages](plugin-events.md#the-player-directory-and-languages).

## Player tables

Players appear after their first usable Join Game and disappear when the
session closes. Pending logins and players on other Rift processes are not
listed.

| Field | Content |
| --- | --- |
| `uuid`, `name` | The player's UUID (lowercase hex, no hyphens) and name |
| `server` | The last committed backend |
| `connection_id` | A decimal string, matching `ctx.connection_id` |
| `protocol` | The client's protocol version |
| `ip` | The address the player connected from |
| `properties` | The signed profile properties of the login, a list of `{ name, value, signature }`; `textures` holds skin and cape |
| `visible_to` | The node a viewer needs to see the player, or nil ([visibility](permissions.md#visibility)); `list` and `get` include hidden players |
| `locale` | The language the client reported, as the client spells it (`en_us`; old clients send `en_US`), or nil while Rift does not know it yet |
| `language` | As `ctx.language`: what `set_language` stored, else the locale |

The client sends its settings after login: during configuration from 1.20.2,
after Join Game before. So `login` never has a `locale` (the player is not
listed yet either), `join` has one only for 1.20.2 and newer clients, and nil
means "not known yet", not "no language". The value follows later changes of
the client's language setting and survives server switches.

## How actions are delivered

Actions are delivered asynchronously. A call returns true once the action is
queued, or false when the player is offline or already has 32 undelivered
actions; it never waits for the client.

- The player's own session writes queued actions in order while it is in a
  usable world, so they wait while a transfer is in progress.
- An action other than a kick, tab list or boss bar that meets a
  backend-initiated reconfiguration is dropped (a dialog too: configuration
  closes every screen). Tab lists and boss bars apply after the next join.
- Undelivered actions are lost on disconnect.
- Each callback can make at most 256 action calls; a broadcast counts once.

## Titles

| Option | Default | Meaning |
| --- | --- | --- |
| `title`, `subtitle` | at least one | A string of 1–1024 bytes, or a chat component table (including `{ mini = ... }`). A title without a subtitle clears any earlier subtitle |
| `fade_in_ms`, `stay_ms`, `fade_out_ms` | 500, 3500 and 1000 | Integers from 0 to 60000, rounded down to 50 ms ticks |

Titles reach every supported client, 1.8.9 to 26.3; a client that cannot show
one keeps its session.

## Tab list

Options are `header` and `footer`, each a string or chat component table of up
to 4096 bytes, with the same placeholders as the `tab_list` setting
([server list and tab list](server-list.md)). An omitted field keeps the
configured text.

The player keeps this header and footer across server switches until the next
call or disconnect, and `rift.players.tab_list(player)` returns them to the
configured ones:

```lua
rift.config.plugins = {
    join = function(ctx)
        if rift.permissions.check(ctx.uuid, "network.vip") == true then
            rift.players.tab_list(ctx.uuid, { header = { text = "VIP Lounge", color = "light_purple" } })
        end
    end,
}
```

## Boss bars

You name each bar (1–64 lowercase letters, digits, `.`, `_` or `-`), so a later
call with the same name changes that bar.

| Option | Default | Values |
| --- | --- | --- |
| `text` | required | A string or chat component table of up to 4096 bytes |
| `progress` | 1 | 0 to 1 |
| `color` | `pink` | `pink`, `blue`, `red`, `green`, `yellow`, `purple` or `white` |
| `style` | `progress` | `progress`, `notched_6`, `notched_10`, `notched_12` or `notched_20` |
| `darken_screen`, `play_music`, `create_fog` | | Booleans |

- Rift sends only the fields that changed.
- A player keeps their bars across server switches until they are removed or
  the player disconnects, and can have at most 8; showing a ninth fails the
  callback.
- Boss bars need Minecraft 1.9 or newer, so the call returns false for 1.8.9
  players.
- They have their own UUIDs and never touch the backend's boss bars.

```lua
rift.config.plugins = {
    join = function(ctx)
        rift.players.boss_bar(ctx.uuid, "event", {
            text = { text = "Build contest: 40 of 50 entries", color = "gold" },
            progress = 0.8,
            color = "yellow",
            style = "notched_10",
        })
    end,
    commands = {
        hidebar = {
            permission = "network.player",
            run = function(ctx) rift.players.boss_bar(ctx.uuid, "event") end,
        },
    },
}
```

## Transfers and kicks

A transfer takes the same path as `/server`: static access, health and draining
checks, `before_transfer` with reason `extension`, capacity and queue
reservation, then `after_transfer`.

- A denied or unavailable target leaves the player where they are without a
  message, and the current server is never a target.
- A service group selects one of its instances.

A kick closes the connection with the given reason and runs `disconnect` as
usual.

## Dialogs

[Dialogs](dialogs.md) are screens with text, inputs and buttons (Minecraft
1.21.6 and newer), described as tables in the vanilla dialog format. A button
whose `custom` or `dynamic/custom` action has an `id` without a namespace calls
the `dialog` callback with `ctx.action` (that `id`), `ctx.values` (the player's
inputs) and `ctx.payload` (the action's `payload` or `additions`, which Rift
keeps instead of sending them to the client).

## Rich text

Chat messages, action bars, titles, broadcasts, kick reasons, deny reasons,
command replies and suggestion tooltips are plain text by default: a string of
1–1024 bytes, shown exactly as written.

For colors, line breaks, hover text and click events, give a
[JSON text component](https://minecraft.wiki/w/Text_component_format) instead,
or write it as markup or legacy codes in an explicit object such as
`{ mini = "<gold>Hi <player>", args = { player = name } }` or
`{ legacy = "&6Hi" }`. [Styled text](text.md) has the markup reference, the
theme, Rift's own message catalog and how each client version is served.

- In Lua (`rift.lua` and [Lua plugins](plugins-lua.md)), pass a chat component
  table where the string would go:
  `{ deny = { text = "Banned", color = "red" } }`,
  `rift.players.message(player, { text = "Hi", bold = true })`.
- In [bus replies](remote-decisions.md), pass
  an object or a list as the `reason`, including `{"mini": "<red>Banned"}`.
- In WebAssembly plugins, use the contract's [`text`](plugin-events.md#text)
  type: `plain(string)` or `json(string)`, the component as JSON text. The SDK
  builders produce the JSON form.

Rift never reads JSON out of a plain string, so a plugin that relays what a
player typed cannot be made to send click events.

**Limits.** A component may be at most 32 KiB of compact JSON (a Lua table at
most 1024 values in 16 levels). It must not be empty, must not contain `null`
and must have an NBT form.

**Validation.** Rift checks the fields it knows
([validation and limits](text.md#validation-and-limits)), and markup after
parsing it. An invalid component fails the call or the decision with an error
that names the key, instead of disconnecting the player.

**Click and hover events.** Minecraft 1.21.5 renamed `clickEvent` and
`hoverEvent` to `click_event` and `hover_event` and changed their fields
(`clickEvent.value` became `url`, `command`, `page` or `value`;
`hoverEvent.contents` became `value` for text and plain fields for items and
entities). Write either form: Rift rewrites events into the form each player's
client reads, and drops an event the client cannot express; the text stays
([how each client sees it](text.md#how-each-client-sees-it)).

Logs and admin replies show a component's words without formatting.

A party invitation with buttons, and a ban screen:

```lua
rift.players.message(target.uuid, {
    { text = inviter .. " invited you to their party. " },
    {
        text = "[Accept]", color = "green", bold = true,
        click_event = { action = "run_command", command = "/party accept " .. inviter },
        hover_event = { action = "show_text", value = "Join " .. inviter .. "'s party" },
    },
    { text = " " },
    {
        text = "[Decline]", color = "red", bold = true,
        click_event = { action = "run_command", command = "/party decline " .. inviter },
    },
})

rift.config.plugins = {
    login = function(ctx)
        local ban = rift.store.get("bans", ctx.uuid)
        if ban then
            return { deny = {
                text = "You are banned from this network\n\n", color = "red", bold = true,
                extra = {
                    { text = "Reason: ", color = "gray", bold = false },
                    { text = ban, color = "white", bold = false },
                },
            } }
        end
    end,
}
```

With markup, the invitation is one call, and `inviter` cannot add tags:

```lua
rift.players.message(target.uuid, rift.text.mini(
    "<inviter> invited you to their party. "
        .. "<green><b><click:run_command:/party accept><hover:show_text:'Join the party'>"
        .. "[Accept]</hover></click></b></green> "
        .. "<red><b><click:run_command:/party decline>[Decline]</click></b></red>",
    { inviter = inviter }
))
```

Arguments fill in text, not tag values: `<click:run_command:/party <x>>` is not
filled in. Build such values in Lua instead, and pass player-supplied parts
through `rift.text.escape` before splicing them into markup.
