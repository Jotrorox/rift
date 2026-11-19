# Styled text

Every piece of text a player sees from Rift can be styled: colors, hex colors,
gradients, decorations, hover text and click events. You write it the same way
everywhere, and Rift shows it correctly on every supported client, from 1.8.9
to 26.3.

Every surface takes plain text or a styled component: messages, action bars,
titles, tab lists, boss bars, kicks, denials, command replies, suggestion
tooltips, dialogs, the MOTD, the version name and the hover lines of the
server list. Rift itself parses, validates and renders the text, so Lua, the
WebAssembly SDKs, the message bus and the configuration cannot disagree.

> [!NOTE]
> Markup is always explicit. Rift shows a bare string exactly as written and
> never reads markup, legacy codes or JSON out of it, so text relayed from a
> player cannot add colors or click events.

## Write styled text

There are four forms. Each becomes a JSON text component at the call; Rift
never stores or sends markup.

| Form | Example |
| --- | --- |
| Markup | `{ mini = "<gold>Welcome, <player>!", args = { player = name } }` |
| Legacy codes | `{ legacy = "&6Welcome &lback", marker = "&" }` (marker defaults to `&`) |
| Message | `{ message = "invited", args = { from = name } }` ([plugin messages](#send-a-message)) |
| JSON component | `{ text = "Welcome", color = "gold" }` ([format](https://minecraft.wiki/w/Text_component_format)) |

The `mini`, `legacy` and `message` objects work wherever a component works,
and nest inside one: in `extra`, in `with`, in hover text, as a markup
argument and in a dialog's texts. A `mini` object takes only `mini` and
`args`; a `legacy` object only `legacy` and `marker`; a `message` object only
`message` and `args`.

```lua
-- Lua: all three are the same message.
rift.players.message(player, { mini = "<green>Saved." })
rift.players.message(player, rift.text.mini("<green>Saved."))
rift.players.message(player, { text = "Saved.", color = "green" })

-- An argument is shown as written; its text is never parsed.
rift.players.message(player, rift.text.mini("<gray>Hi <name>", { name = ctx.name }))
```

Where you can write it:

| Front end | How |
| --- | --- |
| `rift.lua`, extensions, `on_status`, Lua plugins | A table where a string would go, or `rift.text.mini(markup, args)`, which returns a component table |
| Message bus replies | A JSON object as the `reason`: `{"action": "deny", "reason": {"mini": "<red>Banned"}}` |
| WebAssembly plugins | The contract's [`text`](plugin-events.md#text) type is `plain(string)` or `json(string)`, the objects as JSON text. The SDK builders produce the JSON form ([Rust](plugins-rust.md), [Go](plugins-go.md), [C](plugins-c.md)) |
| Configuration | Any text setting (`server_list`, `tab_list`, `deny_message`, ...) takes the tables, and `text.messages` and `text.languages` take markup directly |

## `rift.text` in Lua

| Function | Returns |
| --- | --- |
| `rift.text.mini(markup, args)` | A component table. `args` maps names to strings or numbers (shown as written), component tables or [styles](#style-arguments) |
| `rift.text.legacy(text, marker)` | A component table from `&` codes (or another marker) |
| `rift.text.to_legacy(text)` | A string with `§` codes |
| `rift.text.plain(text)` | The words without formatting |
| `rift.text.to_mini(text)` | Markup for a component |
| `rift.text.escape(text)` | `text` escaped so it can be spliced into markup safely |

## Markup tags

The tag syntax is the one many Minecraft developers know from MiniMessage.

| Tag | Effect |
| --- | --- |
| `<red>`, `<#ff8800>`, `<color:red>`, `<c:#ff8800>` | Color. The 16 names, plus `grey` and `dark_grey` |
| `<bold>` `<b>`, `<italic>` `<i>` `<em>`, `<underlined>` `<u>`, `<strikethrough>` `<st>`, `<obfuscated>` `<obf>` | Decoration. `<!bold>` or `<bold:false>` turns it off |
| `<gradient:red:#5555ff[:...][:phase]>` | One color per character, interpolated between two or more stops, shifted by `phase` |
| `<transition:red:#5555ff[:...][:phase]>` | One color for the whole text: the point `phase` along the stops |
| `<rainbow>`, `<rainbow:0.5>`, `<rainbow:!>`, `<rainbow:!0.5>` | A rainbow, optionally starting at another hue (-1..1). `!` reverses it |
| `<hover:show_text:'markup'>` | Hover text. Also `show_item:id[:count]` and `show_entity:type:uuid[:'name']` |
| `<click:run_command:/spawn>` | Click. Also `suggest_command`, `open_url` (http and https only), `copy_to_clipboard` and `change_page` |
| `<insert:text>` | Text inserted into chat on shift-click |
| `<font:minecraft:uniform>` | Font |
| `<shadow:black[:0.5]>`, `<!shadow>` | Text shadow color and opacity (default 0.25) |
| `<newline>` `<br>` | Line break |
| `<key:key.jump>` | The player's key binding |
| `<sprite:[atlas:]sprite>` | A sprite from an atlas: `<sprite:item/diamond>`, `<sprite:blocks:block/stone>` |
| `<head:name>`, `<head:uuid>`, `<head:name:false>` | A player's head. `false` leaves out the hat layer |
| `<lang:key[:'arg']...>`, `<lang_or:key:'fallback'[:'arg']...>` | Translation; the arguments are markup |
| `<reset>` | Closes every open tag |
| `<name>` | An argument, or a [theme](#theme) style such as `<accent>`. An argument wins over a theme style of the same name |

Rules:

- **Closing.** `</tag>` closes the latest open tag of that kind and every tag
  opened after it. A color closes with any color tag (`<red>x</color>`). Tags
  left open end with the text.
- **Quoting.** Quote an argument with `'` or `"` when it contains `:` or `>`;
  inside quotes, `\` escapes the quote and itself. URLs, fonts and item ids
  may go unquoted: `<click:open_url:https://example.com>`.
- **Literals.** `\<` writes a literal `<` and `\\` a backslash. A `<` that does
  not start a well-formed tag, as in `a < b` or `<3 you`, is literal text.
- **Unknown tags.** A well-formed tag Rift does not know is an error with its
  position:
  `<bolt> at byte 3: unknown tag; not built in, an argument or a style`.
  Typos show up at the call or when the configuration loads, not in game.

Gradients, transitions and rainbows:

- **Phase.** `phase` is a number from -1 to 1, default 0, as in MiniMessage.
  A trailing argument of `<gradient>` or `<transition>` that is a number is
  the phase; no color name is a number. A negative phase is `1 + phase` along
  the stops in reverse order, so `-1` reverses them.
- **`<transition>`** colors the whole text with one color: the point `phase`
  of the way from the first stop to the last. `0` is the first stop and `1`
  the last.
- **`<gradient>` with a phase** starts `phase` of the way along the stops
  and moves as far as it would without one. Past the last stop it runs back
  to the first, so a phase of `1` on two stops goes from the second color
  back to the first. A plugin animates text by sending the same template
  with a moving phase.
- **`<rainbow>`** takes a hue from -1 to 1 to start at; `-0.25` is the same
  hue as `0.75`. `!` before it runs the rainbow backwards: `<rainbow:!>`,
  `<rainbow:!0.5>`.

Sprites and heads:

- They write `object` content, which clients before 1.21.9 cannot show; Rift
  leaves it out for them.
- `<sprite:item/diamond>` is a sprite of the default atlas
  (`{"object": "atlas", "sprite": "item/diamond"}`); with two arguments the
  first is the atlas. Quote a namespaced id, which contains `:`:
  `<sprite:'minecraft:items':'minecraft:item/diamond'>`.
- `<head:Notch>` is the head of a player name: 1 to 16 letters, digits and
  `_`. A UUID, with or without dashes, is the player's id instead.
  `<head:Notch:false>` leaves out the outer (hat) layer.

## Style arguments

An argument can be a style instead of content. Its tag wraps whatever the
template puts inside it:

```lua
rift.text.mini("<team>[<tag>]</team> <name>", {
    team = { style = "<#55ffff><bold>" },
    tag = clan,
    name = ctx.name,
})
```

The JSON form is the same object: `{"mini": "<team>[<tag>]</team>", "args":
{"team": {"style": "<#55ffff><bold>"}, "tag": "x"}}`.

- The value is markup made only of formatting tags, the same rule as a
  [theme](#theme) style: color, decoration, font, shadow, insert, click and
  hover. An empty string styles nothing.
- The object takes only `style`.
- `</team>` closes it, and every tag opened inside it.

## Legacy codes

| Code | Effect |
| --- | --- |
| `&0`–`&f` | Color. Ends earlier decorations, as in the vanilla client |
| `&k`, `&l`, `&m`, `&n`, `&o` | Obfuscated, bold, strikethrough, underlined, italic |
| `&r` | Reset |
| `&#rrggbb` or `&x&r&r&g&g&b&b` | Hex color |

A marker that does not start a known code is kept as written.

## Theme

Named styles are tags in every piece of markup: plugins, the bus, the
configuration and Rift's own messages. An argument of the same name wins, so
a plugin's `<info>` argument is shown even when the theme has an `info`
style. Define them in the configuration as markup made only of formatting
tags:

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
catalog. The defaults are Rift's English wording, unstyled. Replace any of
them with markup:

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

Rift checks templates when the configuration loads. An unknown key, an unknown
tag or an argument the message does not have fails the load. An argument is
shown as written; `<reason>` keeps the formatting of the text it carries, such
as a backend's ban screen.

| Key | Arguments | Default |
| --- | --- | --- |
| `login.maintenance` | – | The network is under maintenance. Please try again later. |
| `login.rate_limited` | – | Too many login attempts. Please wait before reconnecting. |
| `login.unknown_host` | – | No server is configured for this hostname. |
| `login.forwarding_version` | – | This server requires Minecraft 1.13 or newer. |
| `login.invalid_start` | – | Invalid or missing login start. |
| `login.auth_denied` | – | Unable to verify your Minecraft account. Please sign in again. |
| `login.auth_invalid` | – | Invalid authentication response. Please reconnect. |
| `login.auth_unavailable` | – | Minecraft authentication is unavailable or timed out. Please try again later. |
| `login.extensions_version` | – | Player hooks are unavailable for this Minecraft version. |
| `login.extension_failed` | – | Login extension failed. Please try again later. |
| `login.no_access` | – | You do not have access to this server. |
| `login.dev_mode` | – | This server is in development mode. |
| `login.duplicate_name` | – | That player is already connected to this network. |
| `login.invalid_name` | – | Invalid player name. |
| `login.unavailable` | – | The server is currently unavailable. Please try again later. |
| `login.already_connected` | – | This player is already connected. |
| `login.unsupported_version` | – | Unsupported Minecraft version. Rift supports Java 1.8.9–26.3. |
| `login.transfer_version` | – | Transfer requires Minecraft 1.20.5 or newer. |
| `login.version` | – | This server does not accept your Minecraft version. |
| `login.no_version` | – | No server on this network supports your Minecraft version. |
| `decision.unavailable` | – | Unable to check your access, please try again later. |
| `connection.lost_no_fallback` | – | The server connection was lost and no fallback was available. |
| `connection.lost` | – | The server connection was lost. Please reconnect. |
| `connection.failed` | – | The server connection failed. Please reconnect. |
| `connection.backend_closed` | – | The current backend disconnected the player. |
| `connection.denied` | – | The destination server denied access. |
| `connection.kicked` | `<server>`, `<reason>` | You were kicked from \<server\>: \<reason\> |
| `switch.incomplete` | – | The server switch could not be completed. |
| `switch.starting` | – | A server is starting. Please wait for the current transfer. |
| `switch.failed` | `<reason>` | Unable to switch servers: \<reason\> |
| `switch.busy` | – | The current server is busy. Please retry the switch. |
| `switch.no_server` | – | No available server could be reached. |
| `server.list` | `<current>`, `<servers>` | Current server: \<current\>. Available servers: \<servers\> |
| `server.already_on` | `<current>` | You are already on \<current\>. |
| `server.unavailable` | – | Server unavailable or you do not have access to it. |
| `server.usage` | – | Usage: /server [name] or /hub |
| `hub.already` | – | You are already in a hub. |
| `command.failed` | – | Extension command failed. |
| `command.no_permission` | – | You do not have permission to use this command. |
| `chat.failed` | – | Your message could not be checked and was not sent. |
| `queue.joined` | `<server>`, `<position>` | Queued for \<server\>. Position: \<position\>. |
| `queue.left` | – | Left the queue. |
| `queue.no_access` | – | You do not have access to that server. |
| `queue.transfer_failed` | `<reason>` | Queue transfer failed: \<reason\> |
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

Rift sends `login.forwarding_version` only with Velocity forwarding.

## Languages

`text.languages` holds the same messages in other languages, by language tag:

```lua
rift.setup {
    text = {
        messages = {
            ["queue.left"] = "<muted>Left the queue.",
        },
        languages = {
            de = {
                ["queue.left"] = "<muted>Warteschlange verlassen.",
                ["queue.joined"] = "<good>In der Warteschlange für <server>. Position: <position>.",
            },
            de_at = { ["queue.full"] = "Die Warteschlange ist voll." },
        },
    },
}
```

Rift renders its messages in each player's language: the one set with
`players.set-language` (`rift.players.set_language` in Lua; see
[the player directory and languages](plugin-events.md#the-player-directory-and-languages)),
else the client's locale. For each message Rift takes the first template it
finds:

1. the exact tag (`de_at`),
2. its language part (`de`),
3. `text.messages`,
4. the built-in English text.

Tags are 2–16 bytes of `a-z`, `0-9` and `_`; `de-DE` is normalized to
`de_de`. A configuration may hold at most 64 languages. `text.messages` and
each language hold at most 1024 entries, counting
[plugin messages](#plugin-messages). Rift checks keys and templates when the
configuration loads, like `text.messages`.

## Plugin messages

A plugin can ship its own messages in a `messages.toml` beside its
`plugin.toml`. Rift renders them in each player's language with the network's
theme, and the operator can reword any of them without touching the plugin.

```toml
# plugins/party/messages.toml
[messages]
created = "<good>Party created."
invited = { text = "<accent><from></accent> invited you to a party.", args = ["from"] }
full    = { text = "<bad>The party is full (<size>/<max>).", args = ["size", "max"] }
tagged  = { text = "<team>[<tag>]</team> <name>", args = ["tag", "name"], styles = ["team"] }

[languages.de]
created = "<good>Party erstellt."
invited = "<accent><from></accent> hat dich in eine Party eingeladen."
```

- `[messages]` holds the defaults. A value is a template, or a table with
  `text`, the `args` the template may use and the
  [`styles`](#style-arguments) it may use.
- `[languages.<tag>]` holds templates only, for keys in `[messages]`. A
  language uses the arguments and styles its default declares and need not
  translate every key. Tags are normalized like `text.languages` tags.
- Templates are [markup](#markup-tags) with the operator's [theme](#theme).
- A key with a `.` must be quoted, as in `"queue.full" = "..."`: TOML reads a
  bare `queue.full` as a table.

The plugin folder's name is the catalog's **namespace**. Both persistent
plugins and script-mode folders may ship the file. `rift` is reserved for
Rift's own messages, so a folder named `rift` may not have one. Rift reads the
file with the configuration, without running plugin code: at startup, in
`rift check` and on a reload. A changed file takes effect on the next reload
without restarting the plugin. A plugin without the file has an empty catalog.

### Send a message

A `message` object sends a message from a catalog. It works wherever a
component works, like `mini`:

```lua
-- A persistent Lua plugin: its own message, in the target's language.
rift.players.message(player, { message = "invited", args = { from = ctx.name } })
-- rift.lua, a script-mode plugin or a bus service: a qualified key.
rift.players.message(player, { message = "party:invited", args = { from = ctx.name } })
```

```json
{"message": "party:invited", "args": {"from": "Steve"}}
```

- A key without `:` belongs to the caller's own catalog. Only a persistent
  plugin has one at the call.
- A **qualified** key names any catalog: `party:invited`, or
  `rift:queue.full` for one of [Rift's own messages](#rifts-own-messages) as
  the operator styled it.
- `rift.lua`, script-mode plugins and bus services always use qualified keys.
  A script-mode plugin names its own messages with its folder name, such as
  `party:invited`.
- `args` takes what `mini` arguments take: strings and numbers (shown as
  written, never parsed), components (which may hold `mini` or `message`
  objects themselves) and [style arguments](#style-arguments).
- Rift checks the call: an unknown key, a missing argument, an argument the
  message does not declare, or content where the message declares a style is
  an error at the call, whoever the recipient is. A `broadcast` with a bad
  message fails even when nobody is online.

### Whose language

Rift renders a message at the call, for the player who sees it, in the same
order as its own messages: the exact tag (`de_at`), its language part (`de`),
then the default. The player's language is the one `player-info.language`
reports: the one set with `players.set-language`, else the client's locale
([the player directory and languages](plugin-events.md#the-player-directory-and-languages)).

| Where the text goes | Language |
| --- | --- |
| `players.message`, `action-bar`, `title`, `tab-list`, `boss-bar`, `kick`, `dialog` | The target's |
| `players.broadcast` | Each recipient's own. Rift renders once per language in use |
| A decision (`deny`, `message`), a suggestion's tooltip | The player the event is about (`ctx.language`) |
| A deferred answer (`background.resolve`) | The player the deferred event is about |
| `status` (MOTD, version, sample), a console command's reply | The default: a ping carries no locale |
| Configuration text (`server_list`, `tab_list`, `deny_message`, ...) | The default, rendered once when the configuration loads |

Text never carries an unresolved message further into Rift: a tab list or a
boss bar keeps the text it was given in the target's language until it is
changed. Configuration text renders once, so it uses qualified keys and the
default language.

### Operator replacements

The operator rewords a plugin's message in `text.messages` and
`text.languages`, by its qualified key `namespace:key`:

```lua
rift.setup {
    text = {
        messages = {
            ["party:invited"] = "<accent><from></accent> wants you in their party.",
        },
        languages = {
            fr = { ["party:invited"] = "<accent><from></accent> vous invite." },
        },
    },
}
```

A key without a namespace stays one of [Rift's own messages](#rifts-own-messages),
so existing configurations do not change. Write those without `rift:`.

Rift takes the first template it finds for the player's language. At each
step the operator's template wins over the plugin's:

1. `text.languages[tag]`, else the plugin's `[languages.<tag>]`, for the exact
   tag (`de_at`),
2. the same pair for the language part (`de`),
3. `text.messages`, else the plugin's `[messages]`.

### Checks and limits

`rift check` and every configuration load check the catalogs:

- every template parses with the arguments and styles its message declares
  and no others, and with the theme of the configuration;
- a replacement names a message that exists. An unknown key in the namespace
  of an enabled plugin is an error. A namespace no enabled plugin has is a
  warning, and an error with `rift check --strict`, so that disabling a plugin
  for a day does not break the configuration. Rift ignores such a
  replacement.

`rift check` prints each plugin's message and language counts:

```text
rift: plugin party: mode script, sandboxed; messages 4, languages 1
```

Parsing is strict, like `plugin.toml`: an unknown key, a value of the wrong
type, a name in both `args` and `styles`, or a language key missing from
`[messages]` is an error that names the file and the key.

| Limit | Value |
| --- | --- |
| `messages.toml` | at most 256 KiB |
| Messages per catalog | 256 |
| Languages per catalog | 64 |
| Key | 1–64 characters of `a-z`, `0-9`, `_`, `-` and `.` |
| Template | 1–4096 bytes |
| Arguments and styles per message | 16 |
| Argument and style names | 1–64 characters of `a-z`, `0-9`, `_` and `-`, not a built-in tag |

Only size can make an accepted message fail to render: arguments can push a
component past 32 KiB. Rift then falls back to the plugin's default template,
then to the message's words.

## How each client sees it

When sending, Rift rewrites components for each player's client:

| Feature | Clients without it | What Rift sends |
| --- | --- | --- |
| `click_event` / `hover_event` spelling | before 1.21.5 | `clickEvent` / `hoverEvent`, with `contents` from 1.16 and `value` before |
| Hex colors | before 1.16 | The nearest of the 16 named colors |
| `font` | before 1.16 | Dropped |
| `shadow_color` | before 1.21.4 | Dropped |
| `keybind` content | before 1.12 | The key's name as text |
| `object` content (sprites, heads) | before 1.21.9 | Dropped, in dialogs too |
| `copy_to_clipboard` clicks | before 1.15 | Dropped |
| `show_dialog` and `custom` clicks | before 1.21.6 | Dropped |
| Item and entity hovers | before 1.16 | Dropped |

Rift keeps components in the 1.21.5 spelling: it stores an event written only
as `clickEvent` or `hoverEvent` as `click_event` or `hover_event`. A component
that carries both spellings of one event keeps both, and each client gets the
one written for it.

The server list's version name and hover lines are strings to every client, so
Rift writes them as `§` codes. No vanilla client reads hex codes in strings,
so hex colors become the nearest named color there, and events are dropped. A
plain string in those fields is passed through as written.

## Validation and limits

| Limit | Value |
| --- | --- |
| Plain text | 1–1024 bytes |
| Server-list lines and suggestions | 256 bytes |
| Component | at most 32 KiB of compact JSON |
| Gradient or rainbow | at most 512 colored characters |

A component must not be empty, must not contain `null` and must have an NBT
form. Rift checks the fields it knows, so a mistake fails at the call instead
of disconnecting the player:

| Key | Rule |
| --- | --- |
| content | exactly one of `text`, `translate`, `keybind`, `score`, `selector`, `nbt`, `object` (a bare string or a list is also content) |
| `color` | one of the 16 color names, or `#rrggbb` |
| `bold`, `italic`, `underlined`, `strikethrough`, `obfuscated` | `true` or `false` |
| `shadow_color` | an ARGB integer, or a list of four numbers in 0..=1 |
| `font` | an identifier such as `minecraft:uniform` |
| `insertion` | a string |
| `extra`, `with` | a non-empty list of components (`with` also takes numbers and booleans) |
| `click_event` / `clickEvent` | a known action with the field it needs; `open_url` must be `http` or `https`; commands must not contain control characters or `§`; `show_dialog` and `custom` only in the `click_event` form |
| `hover_event` / `hoverEvent` | `show_text` with a valid component, or `show_item` / `show_entity` naming what it shows |

Rift passes other keys through unchanged, so fields newer than Rift keep
working. Errors name the key, for example
`message: extra[1].color must be one of the 16 colour names or #rrggbb, got "orange"`.
