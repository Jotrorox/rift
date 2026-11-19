# Plugin text: messages, style arguments and helpers

> [!NOTE]
> **Status: proposal.** Nothing in this document is implemented. It is the
> design for the branch that adds it, and it grows with the branch: when the
> branch merges, its content moves into [styled text](text.md) and the plugin
> guides, and this page goes away. The choices it rests on are recorded under
> [decisions](#decisions), with the options that were weighed.

[Styled text](text.md) already puts on a player's screen nearly everything the
Adventure library can: every content type, every style, hover and click
events, markup in MiniMessage's syntax, legacy codes, and a rewrite for each
client from 1.8.9 to 26.3 that Adventure leaves to the platform.

What a plugin can *do* with text is narrower than in Adventure. This design
closes the gaps that block whole kinds of plugins and declines the rest:

| Adventure | Rift today | This design |
| --- | --- | --- |
| `GlobalTranslator`, `TranslationStore`: a plugin's own messages in each player's language | Only Rift's own messages are translated ([languages](text.md#languages)). A plugin reads `ctx.language` and picks a template itself; one `broadcast` reaches every player in one language | [Message catalogs](#message-catalogs): yes |
| `Placeholder.styling`: a style chosen at the call, as a tag | Arguments are text or components. Named styles come only from the operator's theme | [Style arguments](#style-arguments): yes |
| `<transition>`, a phase on `<gradient>`, reversed `<rainbow>`, `<sprite>`, `<head>` | Missing. Sprites and heads need the JSON form | [Markup additions](#markup-additions): yes |
| Serializers (plain, legacy, MiniMessage) usable from any plugin | Lua only (`rift.text.*`). WebAssembly plugins have none, and no `escape` | [The `text` interface](#the-text-interface): yes |
| Builders for every field | The SDK builders cover color, decorations, four clicks, hover text and insertion | [SDK builders](#sdk-builders): yes |
| `<pride>` | Missing | No |
| `<selector>`, `<score>`, `<nbt>` | Refused in markup on purpose; the JSON form passes them through | No |
| Custom `TagResolver`s, `Formatter.number`, `date`, `choice` | Missing | No, for now |
| `replaceText`, `compact`, `join`, walking a component's children | Missing | No |
| ANSI serializer for consoles | The console prints a reply's words | No, for now |

## Goals

1. **A plugin's messages follow the player's language and the operator's
   style.** A plugin ships templates; Rift renders each one in the language of
   the player who sees it, with the network's theme, and the operator can
   reword or restyle any of them without touching the plugin.
2. **Mistakes fail when the configuration loads.** A plugin's templates and
   the operator's replacements are checked by `rift check`, like Rift's own
   messages, without running plugin code.
3. **Every engine has the same text functions.** What `rift.text` does in Lua,
   a WebAssembly plugin can do too.
4. **Nothing existing changes shape.** The contract stays on the `0.2` track:
   only additions, so compiled plugins keep loading.

### Non-goals

- **Compatibility with MiniMessage.** Rift's markup keeps its syntax where
  both have a tag, and stays strict: an unknown tag is an error.
- **Editing components.** Adventure passes components around as objects, so it
  needs `replaceText`, `compact` and tree walks. A Rift plugin writes text and
  hands it to Rift. Text that arrives in events (a chat message, a kick
  reason, a command line) is a plain string.
- **Tags that run plugin code.** A tag resolver would call back into a plugin
  while Rift renders, on every message. Arguments and style arguments cover
  the cases that came up.
- **Number, date and plural formatting inside templates.** A plugin formats a
  value before it passes it. See [decision 6](#6-plural-and-number-formatting).
- **Catalogs for `rift.lua` and bus services.** They can send any catalog's
  messages by qualified key, but do not get a catalog of their own. See
  [decision 7](#7-catalogs-for-riftlua-and-bus-services).

## Message catalogs

### `messages.toml`

A plugin folder may hold a `messages.toml` beside its `plugin.toml`:

```toml
# plugins/party/messages.toml
[messages]
created = "<good>Party created."
invited = { text = "<accent><from></accent> invited you to a party.", args = ["from"] }
full    = { text = "<bad>The party is full (<size>/<max>).", args = ["size", "max"] }

[languages.de]
created = "<good>Party erstellt."
invited = "<accent><from></accent> hat dich in eine Party eingeladen."
```

- `[messages]` holds the defaults. A value is a template, or a table with
  `text`, the `args` the template may use and the `styles` it may use
  ([style arguments](#style-arguments)).
- `[languages.<tag>]` holds templates only. A language uses the arguments and
  styles its default declares and need not translate every key.
- Parsing is strict, like `plugin.toml`: an unknown key is an error.
- Templates are [markup](text.md#markup-tags) with the operator's
  [theme](text.md#theme), as in `text.messages`.

The folder's name is the catalog's **namespace**. Both persistent plugins and
script-mode folders may ship the file. Rift reads it with the configuration:
at startup, in `rift check` and on a reload. A changed file takes effect on
the next configuration reload, without restarting the plugin.

### Send a message

A fourth authoring object joins `mini` and `legacy`:

| Form | Example |
| --- | --- |
| Message | `{ message = "invited", args = { from = name } }` |

```lua
-- Lua
rift.players.message(player, { message = "invited", args = { from = ctx.name } })
rift.players.broadcast(rift.text.message("created"))
```

```rust
// Rust
players::message(&target, text::message("invited").arg("from", &ctx.name))?;
```

- A `message` object takes only `message` and `args`, and works wherever a
  component works: in `extra`, in hover text, as a markup argument, in a
  dialog.
- A key without `:` belongs to the caller's own catalog. A **qualified** key
  names any catalog: `party:invited`, or `rift:queue.full` for one of
  [Rift's own messages](text.md#rifts-own-messages) as the operator styled it.
  `rift.lua` and bus services have no catalog, so they always qualify.
- Arguments are strings or numbers (shown as written, never parsed),
  components, or [style arguments](#style-arguments).
- Rift checks the call: an unknown key, a missing argument, an argument the
  message does not declare, or content where the message declares a style is
  an error at the call, whoever the recipient is.

### Whose language

Rift renders a message for the player who sees it, in the same order as its
own messages: the exact tag (`de_at`), its language part (`de`), then the
default.

| Where the text goes | Language |
| --- | --- |
| `players.message`, `action-bar`, `title`, `tab-list`, `boss-bar`, `kick`, `dialog` | The target's |
| `players.broadcast` | Each recipient's own. Rift renders once per language in use |
| A decision (`deny`, `message`), a suggestion's tooltip | The player the event is about |
| `status` (MOTD, version, sample), a console command's reply | The default: a ping carries no locale |

`players.set-language` and the client's locale decide a player's language, as
[today](plugin-events.md#the-player-directory-and-languages).

Rift resolves a message at the call, in the language of the call's recipient,
so text never carries an unresolved message further into Rift. Text in the
configuration itself, such as the MOTD or a tab list template, is rendered
once when the configuration loads, in the default language.

### Operator replacements

The operator rewords a plugin's message in the tables that already exist, by
its qualified key:

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

An unqualified key stays one of Rift's own, so existing configurations do not
change. For each step of the language order, the operator's template wins
over the plugin's:

1. `text.languages[tag]`, else the plugin's `[languages.<tag>]`, for the exact
   tag,
2. the same pair for the language part,
3. `text.messages`, else the plugin's `[messages]`.

### Checks and limits

`rift check` and every configuration load check the catalogs without running
plugin code:

- every template parses with the arguments its message declares and no
  others, and with the theme in effect;
- a replacement names a message that exists. An unknown key in the namespace
  of an enabled plugin is an error. A namespace no enabled plugin has is a
  warning, and an error with `--strict`, so that disabling a plugin for a day
  does not break the configuration;
- `rift check` prints each plugin's message and language counts.

| Limit | Value |
| --- | --- |
| `messages.toml` | at most 256 KiB |
| Messages per catalog | 256 |
| Languages per catalog | 64 |
| Key | 1–64 characters of `a-z`, `0-9`, `_`, `-` and `.` |
| Template | 1–4096 bytes |
| Arguments per message | 16 |

A render cannot fail after the call was accepted, except by size: arguments
can push a component past 32 KiB. Rift then falls back as it does for its own
messages: the default template, then the message's words.

## Style arguments

An argument can be a style instead of content. It becomes a tag that wraps
whatever the template puts inside it:

```lua
rift.text.mini("<team>[<tag>]</team> <name>", {
    team = { style = "<#55ffff><bold>" },
    tag = clan,
    name = ctx.name,
})
```

- The value is markup made only of formatting tags, the same rule as a
  [theme](text.md#theme) style: color, decoration, font, shadow, insert,
  click and hover.
- It works in `mini` and `message` objects. A catalog declares a style
  argument under `styles` instead of `args`, so its templates can be checked:
  `{ text = "<team>[<tag>]</team> <name>", args = ["tag", "name"], styles = ["team"] }`.
  The template decides what the style wraps.
- This is what lets an operator's replacement keep a color the plugin
  computes, such as a rank or team color: a component argument can only carry
  text the plugin chose, not text the template chose.

**An argument beats a theme style of the same name.** Today the theme wins
(`src/text/markup.rs`, the lookup after the built-in tags), so an argument
named `info` or `accent` is silently never shown. A plugin author cannot know
the names in every operator's theme, so the caller's argument takes
precedence. See [decision 4](#4-arguments-and-theme-styles-with-one-name).

## Markup additions

| Tag | Effect |
| --- | --- |
| `<transition:red:#5555ff[:...][:phase]>` | One color for the whole text: the point `phase` along the stops |
| `<gradient:red:#5555ff[:...][:phase]>` | The gradient, shifted by `phase` |
| `<rainbow:!>`, `<rainbow:!0.5>` | The rainbow reversed |
| `<sprite:[atlas:]sprite>` | A sprite from an atlas: `<sprite:item/diamond>`, `<sprite:blocks:block/stone>` |
| `<head:name>`, `<head:uuid>`, `<head:name:false>` | A player's head; `false` leaves out the hat layer |

- `phase` is a number from -1 to 1, default 0, as in MiniMessage. `<rainbow>`
  takes the same range; today it takes 0 to 1. A plugin animates a boss bar
  or an action bar by sending the same template with a moving phase.
- `<sprite>` and `<head>` write the `object` content Rift already validates
  and already drops for clients before 1.21.9.
- A trailing argument of `<gradient>` or `<transition>` that parses as a
  number is the phase; no color name is a number.

Not added:

- `<pride>`: a fixed list of flag gradients. `<gradient>` writes any of them.
- `<selector>`, `<score>`, `<nbt>`: a backend resolves these, and text a proxy
  sends never passes through one. The JSON form stays available for text that
  does.
- `<click:show_dialog>` and `<click:custom>`: their values are objects.
  MiniMessage has no markup for them either, and the JSON form does.

## The `text` interface

The contract (`rift:plugin@0.2.1`) gains one interface. It is an addition, so
components built against `0.2.0` keep loading; a plugin that calls it sets
`min_api = "0.2.1"`.

```wit
/// What `rift.text` does in Lua, for every engine.
interface text {
    use types.{text};

    /// The JSON text component `text` becomes: markup, legacy codes and
    /// messages resolved, in `language` or the default. Also the way to
    /// check a template from the plugin's own configuration during `init`.
    render: func(text: text, language: option<string>) -> result<string, string>;
    /// The words without formatting.
    plain: func(text: text, language: option<string>) -> result<string, string>;
    /// The text with `§` codes.
    to-legacy: func(text: text, language: option<string>) -> result<string, string>;
    /// Markup that writes the text.
    to-mini: func(text: text, language: option<string>) -> result<string, string>;
}
```

A plugin needs these for text that leaves through something other than the
`text` type: a bus payload for a backend, an `http` response, a log line, a
value in the store. `render` in `init` turns a bad template in the plugin's
configuration into a failed start instead of an error in front of a player.

Lua gains the same additions:

| Function | Returns |
| --- | --- |
| `rift.text.message(key, args)` | The `message` object as a table |
| `rift.text.render(text, language)` | A component table with markup, legacy codes and messages resolved |
| `rift.text.plain`, `to_legacy`, `to_mini` | As today, with an optional `language` as the second argument |

`escape` needs no host call. It is two replacements (`<` and `\`), so each SDK
implements it, pinned to `rift.text.escape` by shared test vectors.

## SDK builders

The Rust, Go and C SDKs (version 0.2.1) build every field the format has, so
no plugin needs raw JSON for an ordinary component:

| Addition | Rust | Go | C |
| --- | --- | --- | --- |
| Message | `text::message(key)` with `arg`, `arg_component`, `arg_style` | `rift.Msg(key)` | `rift_msg` |
| Style argument | `Mini::arg_style` | `MiniText.ArgStyle` | `rift_mini_arg_style` |
| Escape | `text::escape` | `rift.Escape` | `rift_text_escape` |
| Content | `Component::translate` (with arguments and a fallback), `keybind`, `sprite`, `head` | the same | the same |
| Style | `font`, `shadow`, turning a decoration off | the same | the same |
| Events | `change_page`, `hover_item`, `hover_entity` | the same | the same |
| Any other field | `Component::field(key, value)` | exists (`Field`) | exists (`rift_component_field`) |
| The `text` interface | `text::render`, `plain`, `to_legacy`, `to_mini` | the same | the same |

The `hello` example in all four languages gains a `messages.toml` with one
translated message, so the examples keep identical behavior.

## Code changes

| Where | Change |
| --- | --- |
| `src/text/markup.rs` | `Arg::Style`; arguments before theme styles; `<transition>`, phases, `<rainbow:!>`, `<sprite>`, `<head>`; `to_mini` writes the new tags back |
| `src/text/catalog.rs` | A `Catalog` holds namespaces beside Rift's notices: declared arguments, defaults, languages and the operator's replacements by qualified key. The lookup order and the fallback stay one function for both |
| `src/text/mod.rs` | `expand` knows the `message` object and resolves it with the caller's namespace and the recipient's language, which each host call sets around the conversion. Text for several recipients stays a template until `broadcast` renders it |
| `src/extensions/players.rs`, `src/plugins/services.rs` | Every action looks up its target's language before it converts the text; `broadcast` renders once per language in use instead of once |
| Plugin loading | Read and check `messages.toml` where the manifest is read; pass the plugin's namespace to every host call that takes text |
| `wit/rift-plugin.wit`, `src/plugins/wasm/bindings.rs`, `src/text/lua.rs` | The `text` interface and the `rift.text` additions |
| `sdk/` | Builders, `escape`, the interface bindings, version 0.2.1 |
| `src/check.rs` | Catalog checks and the per-plugin counts |

## Plan

1. Design (this document).
2. Markup: style arguments, argument precedence, `<transition>`, phases,
   `<rainbow:!>`, `<sprite>`, `<head>`.
3. Catalogs: `messages.toml`, its checks, operator replacements, `rift check`
   output.
4. The `message` object, rendered per recipient across the players API,
   decisions, dialogs and `broadcast`.
5. Contract 0.2.1: the `text` interface and the `rift.text` additions.
6. SDKs 0.2.1: builders, `escape`, messages; the `hello` examples.
7. Documentation: this page folded into [styled text](text.md) and the plugin
   guides, and the changelog.

The branch implements the steps in order and merges when the last one is
done. Steps 2 and 5 do not depend on the catalogs and can be split off if the
catalogs need more discussion.

## Decisions

### 1. Where a plugin's templates live

**A `messages.toml` file in the plugin folder.**

- *Registered from `init`* (a host function or a field of `registration`):
  the plugin would have to run before its templates could be checked, and
  persistent plugins never run during `rift check`. `registration` also
  cannot gain a field on the `0.2` track.
- *In `plugin.toml`*: the manifest is limited to 64 KiB and is not something
  a translator should edit.
- *A file per language*: more files for the same content. One file keeps a
  message and its translations side by side; the limits keep it small.

### 2. How a plugin names a message

**A `message` authoring object inside `json` text.** The `text` variant cannot
gain a case on the `0.2` track, and an object works in every front end at
once: Lua, the SDKs, bus replies and the configuration.

### 3. When a message is rendered

**At the call, in the recipient's language.** Every call that takes text
names who sees it: a target, or the player an event is about. `broadcast` is
the one call with several recipients, and it renders once per language in
use.

- *When the text is sent to the client*: text would carry unresolved
  messages through every path that handles it (queues, tab lists, boss bars,
  the bus), and one path that forgot to resolve would send clients an object
  they cannot read. The only gain is a language that changes between the call
  and the send.

### 4. Arguments and theme styles with one name

**The argument wins.** The alternative, refusing a catalog whose argument
collides with a theme style, would let an operator's new theme entry break a
plugin they did not write. This changes one existing case: markup that passes
an argument named like a theme style now shows the argument.

### 5. Replacements for a plugin that is not enabled

**A warning, an error with `--strict`.** Rift is strict about unknown keys so
that typos surface, but commenting out one `rift.plugin()` line should not
make the configuration fail to load.

### 6. Plural and number formatting

**Not now.** Rift's own messages avoid plurals ("Position: 3."), and a plugin
can pass `"1 player"` or `"3 players"` as an argument. That stops working
once an operator translates the template into a language with other plural
rules. If catalogs are adopted and that comes up, the fix is a `choice`
argument form, not tags that run code. It is left out until there is a
catalog that needs it.

### 7. Catalogs for `rift.lua` and bus services

**Not now.** The operator owns `rift.lua`, so there is nobody else to restyle
its messages, and a loop over `rift.players.list()` already sends each player
their language. A bus service gets `language` in each event. Both can send
plugin and `rift:` messages by qualified key. A `text.catalogs` table in the
configuration would add this later without changing anything above.

### 8. A color console

**Not now.** `rift admin plugins run` prints the words of a command's reply.
Writing ANSI colors when the output is a terminal is small and independent of
everything here; it can follow on its own.
