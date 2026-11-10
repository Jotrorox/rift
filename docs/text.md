# Styled text

Every piece of text a player sees from Rift can be styled: colours, hex
colours, gradients, decorations, hover text and click events. You write it the
same way everywhere, and Rift shows it correctly on every supported client,
from 1.8.9 to 26.3.

Three rules hold throughout:

1. **One type.** Every surface takes plain text or a styled component:
   messages, action bars, titles, tab lists, boss bars, kicks, denials, command
   replies, suggestion tooltips, dialogs, the MOTD, the version name and the
   hover lines of the server list.
2. **One engine, in Rift.** Parsing, validation and per-client rendering live
   in Rift itself. Lua, the WebAssembly SDKs, the message bus and the
   configuration are thin front ends, so they cannot disagree.
3. **Markup is always explicit.** A bare string is always shown exactly as
   written. Rift never reads markup, legacy codes or JSON out of a plain
   string, so text relayed from a player cannot add colours or click events.

## Writing styled text

There are three authoring forms. All of them become a JSON text component at
the call; Rift never stores or sends markup.

| Form | Example |
| --- | --- |
| Markup | `{ mini = "<gold>Welcome, <player>!", args = { player = name } }` |
| Legacy codes | `{ legacy = "&6Welcome &lback", marker = "&" }` (marker defaults to `&`) |
| JSON component | `{ text = "Welcome", color = "gold" }` ([format](https://minecraft.wiki/w/Text_component_format)) |

The `mini` and `legacy` objects work wherever a component works, and can be
nested inside one: in `extra`, in `with`, in hover text and as a markup
argument. A `mini` object takes only `mini` and `args`; a `legacy` object only
`legacy` and `marker`.

| Front end | How |
| --- | --- |
| `rift.lua`, extensions, `on_status`, Lua plugins | A table where a string would go, or `rift.text.mini(markup, args)`, which returns a component table |
| Message bus replies | A JSON object as the `reason`: `{"action": "deny", "reason": {"mini": "<red>Banned"}}` |
| WebAssembly plugins | The `-json` host functions take the objects as JSON text. The SDK builders write them for you ([Rust](plugins-rust.md), [Go](plugins-go.md), [C](plugins-c.md)) |
| Configuration | Any text setting (`server_list`, `tab_list`, `deny_message`, ...) takes the tables, and `text.messages` takes markup directly |

```lua
-- Lua: all three are the same message.
rift.players.message(player, { mini = "<green>Saved." })
rift.players.message(player, rift.text.mini("<green>Saved."))
rift.players.message(player, { text = "Saved.", color = "green" })

-- An argument is shown as written; its text is never parsed.
rift.players.message(player, rift.text.mini("<gray>Hi <name>", { name = ctx.name }))
```

### `rift.text` in Lua

| Function | Returns |
| --- | --- |
| `rift.text.mini(markup, args)` | A component table. `args` maps names to strings or numbers (shown as written) or component tables |
| `rift.text.legacy(text, marker)` | A component table from `&` codes (or another marker) |
| `rift.text.to_legacy(text)` | A string with `§` codes |
| `rift.text.plain(text)` | The words without formatting |
| `rift.text.to_mini(text)` | Markup for a component |
| `rift.text.escape(text)` | `text` escaped so it can be spliced into markup safely |

## Markup

The tag syntax is the one many Minecraft developers know from MiniMessage.

| Tag | Effect |
| --- | --- |
| `<red>`, `<#ff8800>`, `<color:red>`, `<c:#ff8800>` | Colour. The 16 names, plus `grey` and `dark_grey` |
| `<bold>` `<b>`, `<italic>` `<i>` `<em>`, `<underlined>` `<u>`, `<strikethrough>` `<st>`, `<obfuscated>` `<obf>` | Decoration. `<!bold>` or `<bold:false>` turns it off |
| `<gradient:red:#5555ff[:...]>` | One colour per character, interpolated between two or more stops |
| `<rainbow>`, `<rainbow:0.5>` | A rainbow, optionally starting at another hue (0..1) |
| `<hover:show_text:'markup'>` | Hover text. Also `show_item:id[:count]` and `show_entity:type:uuid[:'name']` |
| `<click:run_command:/spawn>` | Click. Also `suggest_command`, `open_url` (http and https only), `copy_to_clipboard` and `change_page` |
| `<insert:text>` | Text inserted into chat on shift-click |
| `<font:minecraft:uniform>` | Font |
| `<shadow:black[:0.5]>`, `<!shadow>` | Text shadow colour and opacity (default 0.25) |
| `<newline>` `<br>` | Line break |
| `<key:key.jump>` | The player's key binding |
| `<lang:key[:'arg']...>`, `<lang_or:key:'fallback'[:'arg']...>` | Translation; the arguments are markup |
| `<reset>` | Closes every open tag |
| `<name>` | An argument, or a [theme](#theme) style such as `<accent>` |

- `</tag>` closes the latest open tag of that kind, and every tag opened after
  it. A colour closes with any colour tag (`<red>x</color>`). Tags left open
  end with the text.
- Arguments are quoted with `'` or `"` when they contain `:` or `>`; inside
  quotes, `\` escapes the quote and itself. Values such as URLs, fonts and
  item ids may also be written without quotes: `<click:open_url:https://example.com>`.
- `\<` writes a literal `<` and `\\` a backslash.
- A `<` that does not start a well-formed tag, as in `a < b` or `<3 you`, is
  literal text.
- A well-formed tag Rift does not know is an error, with its position:
  `<bolt> at byte 3: unknown tag; not built in, an argument or a style`.
  Typos show up at the call (or when the configuration loads), not in game.
- A gradient or rainbow may colour at most 512 characters.

## Legacy codes

`&0`–`&f` set a colour and end earlier decorations, as in the vanilla client.
`&k` obfuscated, `&l` bold, `&m` strikethrough, `&n` underlined and `&o`
italic add a decoration, and `&r` resets. Hex colours are written `&#rrggbb`
or `&x&r&r&g&g&b&b`. A marker that does not start a known code is kept as
written.

## Theme

Named styles are tags in every piece of markup: plugins, the bus, the
configuration and Rift's own messages. Operators define them in the
configuration as markup made only of formatting tags:

```lua
rift.setup {
    text = {
        theme = {
            accent = "<#ffaa00>",
            bad = "<#ff5555><bold>",
            link = "<aqua><u>",
        },
    },
}
```

| Built-in style | Default | Used for |
| --- | --- | --- |
| `heading` | `<gold>` | `/perms` headings |
| `good` | `<green>` | `/perms` successes |
| `bad` | `<red>` | `/perms` failures |
| `muted` | `<gray>` | `/perms` details |
| `accent` | `<aqua>` | Highlights |
| `info` | `<yellow>` | Notices |
| `plain` | none | Unstyled `/perms` lines |

Style names are 1–64 characters of `a-z`, `0-9`, `_` and `-`, and may not be
built-in tags. A configuration's own theme applies to the markup elsewhere in
the same configuration.

## Rift's own messages

Every message Rift shows players on its own behalf has a key in the message
catalogue. The defaults are Rift's English wording, unstyled, so nothing
changes until you restyle them. Replace any of them with markup:

```lua
rift.setup {
    text = {
        messages = {
            ["login.maintenance"] = "<gradient:gold:red>Maintenance</gradient><br><muted>Back soon.",
            ["queue.joined"] = "<good>Queued for <accent><server></accent>. Position: <position>.",
            ["switch.failed"] = "<bad>Could not switch: <reason>",
        },
    },
}
```

Templates are checked when the configuration loads: an unknown key, an
unknown tag or an argument the message does not have fails the load. An
argument is shown as written; `<reason>` keeps the formatting of the text it
carries, such as a backend's ban screen.

| Key | Arguments | Default |
| --- | --- | --- |
| `login.maintenance` | – | The network is under maintenance. Please try again later. |
| `login.rate_limited` | – | Too many login attempts. Please wait before reconnecting. |
| `login.unknown_host` | – | No server is configured for this hostname. |
| `login.online_mode_version` | – | Online authentication with Velocity forwarding requires Minecraft 1.19.3 or newer. |
| `login.invalid_start` | – | Invalid or missing login start. |
| `login.auth_denied` | – | Unable to verify your Minecraft account. Please sign in again. |
| `login.auth_invalid` | – | Invalid authentication response. Please reconnect. |
| `login.auth_unavailable` | – | Minecraft authentication is unavailable or timed out. Please try again later. |
| `login.extensions_version` | – | Extensions v1 requires Minecraft 1.19.3–26.3. |
| `login.extension_failed` | – | Login extension failed. Please try again later. |
| `login.no_access` | – | You do not have access to this server. |
| `login.duplicate_name` | – | That player is already connected to this network. |
| `login.invalid_name` | – | Invalid player name. |
| `login.unavailable` | – | The server is currently unavailable. Please try again later. |
| `login.already_connected` | – | This player is already connected. |
| `login.unsupported_version` | – | Unsupported Minecraft version. Rift supports Java 1.8.9–26.3. |
| `login.transfer_version` | – | Transfer requires Minecraft 1.20.5 or newer. |
| `decision.unavailable` | – | Unable to check your access, please try again later. |
| `connection.lost_no_fallback` | – | The server connection was lost and no fallback was available. |
| `connection.lost` | – | The server connection was lost. Please reconnect. |
| `connection.failed` | – | The server connection failed. Please reconnect. |
| `connection.backend_closed` | – | The current backend disconnected the player. |
| `connection.denied` | – | The destination server denied access. |
| `switch.incomplete` | – | The server switch could not be completed. |
| `switch.starting` | – | A server is starting. Please wait for the current transfer. |
| `switch.failed` | `<reason>` | Unable to switch servers: <reason> |
| `switch.busy` | – | The current server is busy. Please retry the switch. |
| `switch.no_server` | – | No available server could be reached. |
| `server.list` | `<current>`, `<servers>` | Current server: <current>. Available servers: <servers> |
| `server.already_on` | `<current>` | You are already on <current>. |
| `server.unavailable` | – | Server unavailable or you do not have access to it. |
| `server.usage` | – | Usage: /server [name] or /hub |
| `hub.already` | – | You are already in a hub. |
| `command.failed` | – | Extension command failed. |
| `command.no_permission` | – | You do not have permission to use this command. |
| `queue.joined` | `<server>`, `<position>` | Queued for <server>. Position: <position>. |
| `queue.left` | – | Left the queue. |
| `queue.no_access` | – | You do not have access to that server. |
| `queue.transfer_failed` | `<reason>` | Queue transfer failed: <reason> |
| `queue.expired` | – | Queue expired after five minutes. |
| `queue.not_configured` | – | No queue is configured for that server. |
| `queue.already_on` | – | You are already on that server. |
| `queue.full` | – | Queue is full. |
| `queue.server_full` | – | Server is full; use the queue command. |
| `status.motd` | – | Rift |
| `status.version` | – | Rift |
| `status.maintenance` | – | The network is under maintenance. |
| `status.unknown_host` | – | No server is configured for this hostname. |
| `status.unavailable` | – | The server is currently unavailable. Please try again later. |

## How each client sees it

Rift keeps components in the 1.21.5 spelling: an event written only as
`clickEvent` or `hoverEvent` is stored as `click_event` or `hover_event`. A
component that carries both spellings of one event keeps both, and each client
gets the one written for it. When sending, Rift rewrites components for each
player's client:

| Feature | Clients without it | What Rift sends |
| --- | --- | --- |
| `click_event` / `hover_event` spelling | before 1.21.5 | `clickEvent` / `hoverEvent`, with `contents` from 1.16 and `value` before |
| Hex colours | before 1.16 | The nearest of the 16 named colours |
| `font` | before 1.16 | Dropped |
| `shadow_color` | before 1.21.4 | Dropped |
| `keybind` content | before 1.12 | The key's name as text |
| `object` content (sprites, heads) | before 1.21.9 | Dropped, in dialogs too |
| `copy_to_clipboard` clicks | before 1.15 | Dropped |
| `show_dialog` and `custom` clicks | before 1.21.6 | Dropped |
| Item and entity hovers | before 1.16 | Dropped |

The server list's version name and hover lines are strings to every client, so
Rift writes them as `§` codes. No vanilla client reads hex codes in strings, so
hex colours become the nearest named colour there, and events are dropped. A
plain string in those fields is passed through as written.

## Validation and limits

Plain text is 1–1024 bytes, except where a surface sets its own limit (256
bytes for server-list lines and suggestions). A component is at most 32 KiB of
compact JSON. It must not be empty, must not contain `null` and must have an
NBT form. Rift checks the fields it knows, so a mistake fails at the call
instead of disconnecting the player:

| Key | Rule |
| --- | --- |
| content | exactly one of `text`, `translate`, `keybind`, `score`, `selector`, `nbt`, `object` (a bare string or a list is also content) |
| `color` | one of the 16 colour names, or `#rrggbb` |
| `bold`, `italic`, `underlined`, `strikethrough`, `obfuscated` | `true` or `false` |
| `shadow_color` | an ARGB integer, or a list of four numbers in 0..=1 |
| `font` | an identifier such as `minecraft:uniform` |
| `insertion` | a string |
| `extra`, `with` | a non-empty list of components (`with` also takes numbers and booleans) |
| `click_event` / `clickEvent` | a known action with the field it needs; `open_url` must be `http` or `https`; commands must not contain control characters or `§`; `show_dialog` and `custom` only in the `click_event` form |
| `hover_event` / `hoverEvent` | `show_text` with a valid component, or `show_item` / `show_entity` naming what it shows |

Other keys are passed through unchanged, so fields newer than Rift keep
working. Errors name the key, for example
`message: extra[1].color must be one of the 16 colour names or #rrggbb, got "orange"`.
