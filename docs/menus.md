# Inventory menus

An inventory menu is a chest-style screen whose slots hold items that act as
buttons: a server selector, a shop page, a confirmation, a text prompt in an
anvil. Rift shows menus to any player on Minecraft 1.12 to 26.3, whatever
software the backend runs, and sends the clicks to the plugin that showed the
menu instead of to the backend. No backend plugin is needed.

[Dialogs](dialogs.md) work the same way for 1.21.6 and newer clients: the
plugin describes a screen, Rift shows it and reports what the player does.
Menus also reach the older clients that have no dialogs.

Two terms are used throughout. The **menu slots** are the menu's own slots,
the upper part of the screen. The **inventory rows** are the player's own 36
slots, which the client draws below every menu. Menus never use the inventory
rows, and a player cannot put items into a menu or take items out of it: a
click never moves, creates or destroys a real item.

## A server selector

```lua
-- rift.lua
local function selector(player)
    rift.players.menu(player, {
        id = "servers",
        title = { mini = "<dark_gray>Choose a server" },
        layout = {
            "#########",
            "# S C M #",
            "####X####",
        },
        keys = {
            ["#"] = { item = "gray_stained_glass_pane", name = " " },
            S = { item = "grass_block", name = { mini = "<green>Survival" },
                  lore = { "Click to join" },
                  id = "join", payload = { server = "survival" } },
            C = { item = "bricks", name = { mini = "<gold>Creative" },
                  id = "join", payload = { server = "creative" } },
            M = { item = "bow", name = { mini = "<red>Minigames" },
                  id = "join", payload = { server = "minigames" } },
            X = { item = "barrier", name = { mini = "<red>Close" }, close = true },
        },
    })
end

rift.command("servers", {
    permission = "", -- every player
    run = function(ctx) selector(ctx.uuid) end,
})

rift.menus.on("servers", {
    click = {
        join = function(ctx)
            rift.players.transfer(ctx.uuid, ctx.payload.server)
        end,
    },
})
```

The chest has three rows because the layout has three lines. Each character
is one slot, a space is an empty slot, and `keys` says what every other
character stands for. A button with an `id` reports its clicks; one without
is decoration. The barrier closes the menu by itself.

Commands and hooks in `rift.lua` need `authentication.online_mode`
([set up](extensions.md#set-up)). The `hello` examples in
[`examples/plugins`](../examples/plugins) open the same selector from a
persistent plugin in Lua, Rust, C and Go.

## Functions

| Mode | Open or redraw | Change a part | Close | Receive events |
| --- | --- | --- | --- | --- |
| `rift.lua` | `rift.players.menu(player, menu)` | `rift.players.update_menu(player, id, patch)` | `rift.players.close_menu(player, id)` | `rift.on("menu", fn)`, `menu` in `rift.config.plugins`, or `rift.menus.on(id, handlers)` |
| Persistent Lua | `rift.players.menu(player, menu)` | `rift.players.update_menu(player, id, patch)` | `rift.players.close_menu(player, id)` | `rift.on("menu", fn)` or `rift.menus.on(id, handlers)` |
| WebAssembly | `players.menu(target, menu-json)` | `players.update-menu(target, id, patch-json)` | `players.close-menu(target, id)` | the `menu` hook of `handler` ([from Rust, Go and C](#from-rust-go-and-c)) |

- `menu` and `patch` are a table (Lua) or JSON text in the format below. Lua
  also takes the JSON text.
- The calls are [queued actions](players-api.md#how-actions-are-delivered),
  like every other player action. They return true once queued, and false
  when the player is offline, older than 1.12 or has 32 undelivered actions.
  An invalid menu or patch fails the call.
- They work for players on any peer of a [cluster](clustering.md), as
  dialogs do; the events come back to the plugin on the peer that showed the
  menu.
- A player has at most one Rift screen. A menu replaces an open menu or
  dialog of Rift's, and a dialog replaces an open menu.
- `close_menu(player, id)` closes that menu if the calling plugin showed it.
  Without an `id` it closes whatever Rift menu is open, whoever showed it;
  its owner gets the `close` event.

## The menu definition

| Field | Default | Meaning |
| --- | --- | --- |
| `id` | required | 1–64 characters of `a-z`, `0-9`, `_`, `-`, `.`, `/`. Names the menu in every event. Showing a menu with the `id` of the menu the player has open from the same plugin [redraws it in place](#changing-an-open-menu) |
| `type` | `"chest"` | `chest`, `hopper`, `dispenser` or `anvil` |
| `rows` | lines of `layout`, else 3 | Chest only: 1 to 6. With a `layout` it must equal the number of its lines |
| `title` | empty | [Text](plugin-events.md#text): a string or a component, including `{ mini = ... }` and `{ message = ... }` |
| `layout` | none | Lines of characters, one per slot. See the table below |
| `keys` | none | The button each layout character stands for. A space is always an empty slot; every other character in the layout needs an entry. Entries the layout does not use are allowed |
| `slots` | none | A list of buttons placed by number, each with `slot = n` or `slots = { n, ... }`. Placed after the layout, so they replace what the layout put there. `{ slot = n, empty = true }` empties a slot |
| `state` | none | Any JSON value of at most 16 KiB that Rift keeps with the menu and hands back in every event. See [state](#state-and-payloads) |
| `text` | `""` | Anvil only: what the text field starts with |
| `typing` | `false` | Anvil only: report a `text` event while the player types |

Slots are numbered from 0, left to right and top to bottom, as in the
protocol and in every Minecraft interface library, in Lua too.

| Type | Menu slots | Layout | Slot numbers |
| --- | --- | --- | --- |
| `chest` | rows × 9 | `rows` lines of 9 characters | 0–8 is the top row |
| `hopper` | 5 | 1 line of 5 | 0–4 |
| `dispenser` | 9 | 3 lines of 3 | 0–2 is the top row |
| `anvil` | 3 | 1 line of 3 | 0 left input, 1 right input, 2 result |

Texts (the title, names and lore) are resolved when the call is made: markup,
legacy codes and `message` objects in the language of the player who sees
the menu ([whose language](text.md#whose-language)).

## Buttons

A button is an item with an optional action.

| Field | Default | Meaning |
| --- | --- | --- |
| `item` | required | An item name of the newest supported version, such as `diamond_sword`; `minecraft:` is optional. A client that lacks the item gets the [stand-in](translation.md#what-players-get) that translation uses |
| `count` | 1 | 1 to 99. Clients before 1.20.5 get at most 64 |
| `name` | the item's own name | Text. Not italic unless the text says so (vanilla italicizes renamed items). On 1.12 a name without a color is white |
| `lore` | none | A list of up to 32 texts, one per line. Not italic unless the text says so |
| `glint` | the item's own | `true` adds the enchantment glint. `false` removes an item's own glint on 1.20.5 and newer; older clients keep it |
| `model` | none | Custom model data for resource packs, from 1.14: a number, or `{ floats, flags, strings, colors }` (at most 64 values each) for 1.21.4 and newer. Older 1.14+ clients get the first of `floats` |
| `item_model` | none | A model ID such as `"rift:wand"`, for 1.21.2 and newer. Older clients ignore it |
| `head` | none | For `player_head` only: `{ player = "<name or UUID>" }` uses the skin of a player who is online, else the default skin; `{ texture = "<value>", signature = "<optional>" }` a texture property as head databases publish it |
| `color` | none | `"#RRGGBB"` for leather armor, potions and other dyeable items |
| `details` | `false` | `false` hides the lines the game adds on its own (attack damage, enchantments, potion effects). `true` shows them |
| `tooltip` | `true` | `false` hides the tooltip on 1.20.5 and newer; older clients get an empty name and no lore |
| `id` | none | The action name reported when the button is clicked: the same characters as a menu `id`. Without one the button is decoration and its clicks are not reported |
| `payload` | none | Any JSON value of at most 4 KiB, handed back with the click. Rift keeps it; the client never sees it. Needs an `id` |
| `clicks` | the four mouse clicks | The [click kinds](#click-kinds) that report: a list, or `"all"`. Needs an `id` |
| `close` | `false` | Close the menu when the button is clicked, without waiting for the plugin. It closes on the click kinds the button reports, so on the four mouse clicks unless `clicks` says otherwise. A button with `close` needs no `id` |

Names and lore are checked like every other [styled text](text.md) and are
spelled for each player's client version by the same code that converts a
backend's items.

## Events

Everything a player does in a menu arrives as a `menu` event at whoever
showed it: the configuration script, or one persistent plugin by name. A
WebAssembly plugin needs no entry in `registration.hooks`, as for dialogs.
The event is an observer: it has no answer.

| Field | In | Content |
| --- | --- | --- |
| the [context](extensions.md#context-fields) fields | all | The player. `server` is their current backend |
| `menu` | all | The menu's `id` |
| `kind` | all | `click`, `text` or `close` |
| `action` | `click` | The button's `id` |
| `slot` | `click` | The slot number, from 0 |
| `click` | `click` | The [click kind](#click-kinds) |
| `key` | `click` | 1 to 9 for the click kind `number_key` |
| `payload` | `click` | The button's `payload`, as the plugin wrote it |
| `text` | every event of an anvil | What the text field holds. It comes from the client: check it like chat |
| `reason` | `close` | Why the menu ended. See [lifetime](#how-long-a-menu-lives) |
| `state` | all | The menu's `state`, as the plugin last set it |

In Lua these are fields of `ctx`. `rift.menus.on(id, handlers)` sorts the
events of one menu, so a script needs no chain of `if`:

```lua
rift.menus.on("shop", {
    click = {                       -- by button id
        buy = function(ctx) ... end,
        page = function(ctx) ... end,
    },
    text = function(ctx) ... end,   -- anvil typing
    close = function(ctx) ... end,
})
```

`click` may also be one function that receives every click. The helper is
plain Lua on top of `rift.on("menu", fn)` and works the same in `rift.lua`
and in persistent Lua plugins; in a persistent plugin it adds `menu` to the
plugin's hooks, so call it while the entry script runs.

### Click kinds

| `click` | The player | Reported by default |
| --- | --- | --- |
| `left`, `right` | Clicks | yes |
| `shift_left`, `shift_right` | Clicks while holding Shift | yes |
| `middle` | Clicks the wheel. Clients from 1.18 send it only in creative mode | no |
| `drop`, `drop_all` | Presses Q, or Ctrl+Q, over the slot | no |
| `number_key` | Presses 1 to 9 over the slot; `key` says which | no |
| `offhand` | Presses F over the slot (1.16 and newer clients) | no |

- A double click arrives as two `left` clicks. The extra "collect" click the
  client sends after them is not reported.
- Drags, clicks outside the window and clicks on the inventory rows are not
  reported.
- A button reports the kinds in its `clicks`. By default that is the four
  mouse clicks, so pressing Q or a number over a button does nothing.
- **A click is reported only if the player saw what they clicked.** If what
  a click on the slot does changed after the content the click refers to,
  Rift drops the click, and a `close` button does not close. A player who
  double-clicks "next page" never buys the item that appears under the
  cursor.
  - What a click does is the button's `id`, `payload` and `close`. A slot
    that only looks different (a new name, lore, count or item) still reports
    its clicks, so a countdown that redraws every second swallows none.
  - Clients before 1.17.1 do not say what they saw, so their clicks are
    always reported. An action that costs the player something should ask
    for confirmation on those clients.
- A player with 32 menu events their plugin has not received yet gets no
  more clicks or `text` events reported until the plugin catches up, as with
  dialog clicks. `close` events are always delivered while the plugin
  runs, even when its queue is full.

## State and payloads

Callbacks in `rift.lua` start with a fresh Lua state every time, so a script
cannot remember which page a player is on. Two fields carry that:

- `payload` belongs to a button and comes back with its click. Put what the
  click means there: a server name, a product ID, a page step.
- `state` belongs to the menu and comes back with every event, `close`
  included. Put what the menu is showing there: the page, the filter, the
  player being inspected.

Rift keeps both and sends neither to the client, so a modified client can
neither read nor change them. A paged list in `rift.lua`:

```lua
local function show(player, page)
    local buttons = {}
    for i, kit in ipairs(kits_on(page)) do      -- up to 45 kits per page
        buttons[#buttons + 1] = {
            slot = i - 1, item = kit.icon, name = kit.name,
            id = "kit", payload = { kit = kit.id },
        }
    end
    buttons[#buttons + 1] = { slot = 45, item = "arrow", name = "Back",
                              id = "page", payload = { step = -1 } }
    buttons[#buttons + 1] = { slot = 53, item = "arrow", name = "Next",
                              id = "page", payload = { step = 1 } }
    rift.players.menu(player, {
        id = "kits", rows = 6, title = "Kits, page " .. page,
        slots = buttons, state = { page = page },
    })
end

rift.menus.on("kits", {
    click = {
        page = function(ctx) show(ctx.uuid, ctx.state.page + ctx.payload.step) end,
        kit = function(ctx) give_kit(ctx.uuid, ctx.payload.kit) end,
    },
})
```

`kits_on` and `give_kit` stand for the script's own code.

## Changing an open menu

**Redraw.** Call `menu` again with the same `id`. If the player still has
that menu of the same plugin open, Rift compares the new definition with what
the client shows and sends only the difference: the changed slots, and the
title if it changed. An unchanged definition sends nothing. The screen does
not close and the mouse does not move.

- A new title makes the client build the screen again, so Rift then sends
  every slot once more with the same window; in an anvil the text stays, with
  the text cursor at its end.
- A new type or number of rows opens the menu anew. This is no ending either:
  the plugin gets no `close` event.
- If the player has closed the menu meanwhile, it opens anew.

Redrawing is the simple way to change anything, and the paged list above
uses it.

**Patch.** `update_menu(player, id, patch)` changes a part without sending
the whole definition, for a clock or a countdown. It does nothing if the
player no longer has that menu of the calling plugin open.

| Patch field | Effect |
| --- | --- |
| `slots` | Buttons with `slot` or `slots`, replacing those slots. `{ slot = n, empty = true }` empties one |
| `keys` | New buttons for layout characters, applied to every slot the layout gave that character and no `slots` entry replaced since |
| `title` | A new title; `""` removes it |
| `state` | A new state |
| `text` | Anvil: replaces what the text field holds |

The patch itself is checked when the call is made, and an invalid one fails
it. Whether it fits the open menu is known only when the player's session
applies it: a slot outside the menu, or `text` for a menu that is not an
anvil. Such a patch changes nothing, the call has already returned true, and
Rift logs a warning that names the plugin, the menu and the reason:

```text
menu shop of plugin shops: patch not applied: slot 27: a chest with 3 rows has slots 0 to 26
```

## Text input with an anvil

An anvil menu is a one-line text prompt that works on every client, where a
dialog needs 1.21.6.

```lua
rift.players.menu(ctx.uuid, {
    id = "nick",
    type = "anvil",
    title = "Choose a nickname",
    text = current_nick,
})

rift.menus.on("nick", {
    click = {
        submit = function(ctx)
            if not valid_nick(ctx.text) then
                rift.players.update_menu(ctx.uuid, "nick", {
                    title = "Letters only, 3 to 16",
                })
                return
            end
            rift.store.set("nick", ctx.uuid, ctx.text)
            rift.players.close_menu(ctx.uuid, "nick")
        end,
    },
})
```

- With no buttons defined, the left slot holds a sheet of paper and the
  result slot a copy of it named after the typed text, with the action
  `submit`. A click on the result is the player's "OK", and closing the menu
  is "cancel".
- All three slots take buttons like any other menu. A layout such as
  `"P X"` puts a custom item left and a cancel button in the result slot.
- The left slot is never empty, because the client lets the player type only
  while it holds an item. Its item is named after the text, whatever its
  `name` says; emptying it brings back the paper.
- The result slot is never empty either: left empty, or emptied by a patch,
  it gets the default `submit` copy of the left item.
- Every event of an anvil carries `text`. With `typing = true` Rift also
  sends a `text` event while the player types, at most 5 a second. The final
  text is always reported: as a `text` event, or in the `close` event when
  the menu ends before the next one is due. A rename that does not change
  the text gives no event.
- A redraw keeps what the player typed; only a patch with `text` replaces
  it. Every click in an anvil puts its three slots back, which moves the text
  cursor to the end.
- The client limits the text to 50 characters, 35 before 1.17. Rift removes
  control characters and `§` and ignores a longer rename, as vanilla does. A
  `text` from the plugin is cut to the client's limit. The text is still
  player input.
- Clients from 1.13 to 1.19.4 do not tell the server when the field becomes
  empty. A player there who deletes everything is reported with the last
  character, so check a minimum length where it matters.
- On 1.12 and 1.13 the screen is always titled "Repair & Name", whatever
  `title` says, and on 1.12 the player has to click the field before typing.
- The client redraws the result and an "Enchantment cost" line by itself on
  every keystroke. Rift corrects both at once, but a custom result item can
  flicker for a moment while the player types.

## How long a menu lives

Every menu ends with exactly one `close` event while its plugin runs, so a
plugin that keeps something per open menu can always release it. A menu
that could not be shown ends that way too.

In `rift.lua` a `menu` callback is an observer like any other: one that
fails or misses its 50 ms deadline is logged and its event is gone
([when a callback fails](extensions.md#when-a-callback-fails)). The script
keeps no state between callbacks, so there is nothing to release.

| `reason` | The menu ended because |
| --- | --- |
| `player` | The player pressed Escape or the inventory key |
| `plugin` | A button with `close`, or `close_menu` |
| `replaced` | Rift showed another menu (a different `id`, or another plugin's) or a dialog |
| `server` | The backend opened a screen of its own (a container, a sign editor, a book or a dialog) or closed the player's screens, or the player respawned after a death or changed worlds. Also: the menu could not be built for the player's client, which Rift logs |
| `switch` | The player is switching servers: Rift closes the menu before the switch starts. Also: the menu reached the player between servers and was not shown |
| `disconnect` | The player left |

- A redraw is not a close.
- A menu is shown only to a player who is in a world: the client has
  received Join Game and no switch is under way.
- Rift closes its own menu before it passes on a backend's sign editor, book
  or dialog, so the player never has two screens.
- A click that arrives while the player is switching servers is dropped,
  like a command.
- **A plugin that stops** (it is removed, reloaded or restarted, it fails,
  or Rift shuts down) receives no more events, so it gets no `close` event
  for its menus: `shutdown` is its last call, and the place to release what
  it keeps per menu. Rift closes every menu it has open, for every player
  on every peer. The next instance starts with no menus and never receives
  an event of a menu an earlier one showed. After a failure the menus close
  before the plugin restarts.
- Reloading `rift.lua` closes none of its menus: its callbacks keep no
  state between events, so the next event of a menu it showed is handled
  as before.

## What Rift accepts

A menu the client cannot decode disconnects the player, so Rift checks the
definition first and fails the plugin's call instead. Unknown fields,
unknown item names, layout lines of the wrong length, a `rows` that
disagrees with the layout, and `payload` or `clicks` on a button without an
`id` are errors. The error names the field; list entries in it count from 1,
as in Lua:

```text
menu.layout[2]: a line of a chest with 2 rows has 9 characters, not 8
menu.layout[1]: the character 'Q' has no entry in keys
menu.slots[1].payload: a button without an id reports no clicks
```

A slot number outside the menu is an error that names the range, because a
Lua loop that counts from 1 is the likeliest mistake:
`slot 27: a chest with 3 rows has slots 0 to 26`.

| Limit | Value |
| --- | --- |
| JSON per menu or patch | 256 KiB |
| `state` | 16 KiB |
| `payload` per button | 4 KiB |
| Lore lines per button | 32 |
| Menu events per player not yet received by the plugin | 32 (clicks and `text`; `close` always arrives) |
| `text` events per second | 5 |

Menus are not offered to [bus responders](remote-decisions.md) and have no
[`rift.players.*` subject](messaging-subjects.md#player-api): their events
need a plugin that holds state.

## From Rust, Go and C

WebAssembly plugins use contract `rift:plugin@0.3.0`, which added menus
([moving from 0.2](plugins.md#moving-from-02)). The `menu` hook is a function
of `handler`, like every other hook, with a default in every SDK.

```wit
// interface players
menu: func(target: string, menu-json: string) -> result<bool, string>;
update-menu: func(target: string, menu: string, patch-json: string) -> result<bool, string>;
close-menu: func(target: string, menu: option<string>) -> result<bool, string>;

// interface handler
menu: func(event: menu-event);
```

`menu-event` has the [event fields](#events): `ctx`, `menu`, `kind`,
`action`, `slot`, `click`, `key`, `text`, `reason`, and `payload-json` and
`state-json` as JSON text. `kind`, `click` and `reason` are strings, so a
later 0.3 release can add a value without breaking the contract; each SDK
turns them into an enum with a case for a value it does not know.

Each SDK has the raw calls and a typed way to build the same JSON. The SDKs
check nothing themselves: Rift validates, so every language gets the same
errors.

**Rust** (`rift-plugin` 0.3.0):

```rust
use rift_plugin::menu::{Button, Menu, MenuEvent, MenuKind};
use rift_plugin::{Action, CommandSpec, Context, Plugin, Registration, players, text};

fn selector() -> Menu {
    Menu::chest("servers")
        .title(text::mini("<dark_gray>Choose a server"))
        .layout(["#########", "# S C M #", "####X####"])
        .key('#', Button::new("gray_stained_glass_pane").name(" "))
        .key('S', Button::new("grass_block")
            .name(text::mini("<green>Survival"))
            .lore(["Click to join"])
            .id("join")
            .payload("survival"))
        .key('C', Button::new("bricks").name(text::mini("<gold>Creative")).id("join").payload("creative"))
        .key('M', Button::new("bow").name(text::mini("<red>Minigames")).id("join").payload("minigames"))
        .key('X', Button::new("barrier").name(text::mini("<red>Close")).close())
}

#[derive(Default)]
struct Lobby;

impl Plugin for Lobby {
    fn init(&mut self, _config: &str) -> Result<Registration, String> {
        Ok(Registration::new().command(CommandSpec::new("servers")))
    }

    fn command(&mut self, ctx: Context) -> Action {
        let _ = players::show_menu(&ctx.uuid, &selector());
        Action::Continue
    }

    fn menu(&mut self, event: MenuEvent) {
        if let MenuKind::Click { action: "join", .. } = event.kind() {
            if let Ok(Some(server)) = event.payload::<String>() {
                let _ = players::transfer(&event.ctx.uuid, &server);
            }
        }
    }
}

rift_plugin::export!(Lobby);
```

| Contract | Rust | Go | C |
| --- | --- | --- | --- |
| `players.menu` | `players::show_menu(target, &Menu)`, or `players::menu` with JSON text | `rift.ShowMenu(target, rift.Menu{...})`, or `rift.ShowMenuJSON` | `rift_show_menu(&target, json)` |
| `players.update-menu` | `players::update_menu(target, id, &MenuPatch)`, or `players::update_menu_json` | `rift.UpdateMenu(target, id, rift.MenuPatch{...})`, or `rift.UpdateMenuJSON` | `rift_update_menu(&target, id, json)` |
| `players.close-menu` | `players::close_menu(target, Some(id))`; `None` closes whatever menu is open | `rift.CloseMenu(target, id)`; `""` closes whatever menu is open | `rift_close_menu(&target, id)`; `NULL` closes whatever menu is open |
| `menu` hook | `Plugin::menu(MenuEvent)`, empty by default. `event.kind()` is a `MenuKind`: `Click { action, slot, click, key }`, `Text { text }`, `Close { reason }` or `Other`; `event.payload::<T>()` and `event.state::<T>()` decode the JSON | `rift.OnMenu(func(rift.MenuEvent))`; `event.Kind`, `event.Click` and `event.Reason` are typed constants with an `Other` case; `DecodePayload`, `DecodeState`, `PayloadString` and `StateString` read the JSON | `exports_rift_plugin_handler_menu(rift_menu_event_t *)`, with a weak default that ignores events; `rift_menu_kind`, `rift_menu_click` and `rift_menu_close_reason` return enums with an `_OTHER` case |
| menu definition | `Menu::chest(id)`, `hopper`, `dispenser`, `anvil`; `.rows`, `.title`, `.layout`, `.key`, `.slot`, `.slots`, `.empty(slot)`, `.state`, `.text`, `.typing()`; `Button::new(item)` with one method per field, `.clicks([...])`, `.all_clicks()` and `.close()`; `Model` and `Head` for `model` and `head`; `MenuPatch::new()` | Plain structs `rift.Menu`, `rift.Button`, `rift.MenuPatch`; `rift.At(button, slots...)` and `rift.Empty(slots...)` place slots | JSON text |

The [Rust](plugins-rust.md), [Go](plugins-go.md) and [C](plugins-c.md)
guides have the details.

## For operators

### What the backend sees

Nothing of the menu itself. Rift writes the menu's packets to the client in
the client's protocol and takes the client's packets for the menu out of the
stream, so the backend and the [translator](translation.md) never see them.
Menus therefore work on vanilla, Paper, Spigot, Pumpkin and any other
software, and across versions when the backend runs another one than the
client.

The backend notices two things:

- **A screen it had open closes.** When a menu opens over a backend's
  container, Rift tells the backend the player closed it, as the client
  would on Escape.
- **Inventory requests.** A click can disturb the player's inventory rows on
  the client (a shift-click, a number key, a double click). Rift does not
  keep a copy of anybody's inventory; it asks the backend to send the
  inventory again with a click that changes nothing. Backend plugins see a
  harmless click on the player's own inventory; which event Paper and Spigot
  fire for it was not checked. Rift sends at most one request at a time and
  at least 250 ms apart, so a player who clicks fast costs the backend a few
  inventory packets a second at most. A 1.17 backend is asked only when the
  menu closes, so until then the player's inventory rows can show what they
  dragged.

No item is ever at risk: the backend's inventory never changes because of a
menu. [How menus work](menus-internals.md) has the packets.

### What works on which client version

Every menu type works on every client from 1.12 to 26.3. What differs:

| Clients | Clicks | Anvil | Buttons |
| --- | --- | --- | --- |
| 1.8.9–1.11.2 | No menus: `menu`, `update_menu` and `close_menu` return false | – | – |
| 1.12–1.12.2 | Always reported (no stale check); `middle` in every game mode; no `offhand` | Titled "Repair & Name"; the player clicks the field before typing; 35 characters; an emptied field is reported | At most 64; `glint = false` keeps an item's own glint; `tooltip = false` leaves an empty name and no lore; no `model` or `item_model`; a name without a color is white |
| 1.13–1.13.2 | As 1.12 | Titled "Repair & Name"; 35 characters; an emptied field is not reported | As 1.12, without the white names |
| 1.14–1.15.2 | As 1.12 | The title shows; 35 characters; an emptied field is not reported | `model` as one number |
| 1.16–1.16.5 | `offhand` | As 1.14 | As 1.14 |
| 1.17 | As 1.16 | 50 characters | As 1.14 |
| 1.17.1 | Stale clicks dropped | As 1.17 | As 1.14 |
| 1.18–1.19.4 | `middle` only in creative mode | As 1.17 | As 1.14 |
| 1.20–1.20.4 | As 1.18 | An emptied field is reported | As 1.14 |
| 1.20.5–1.21.1 | As 1.18 | As 1.20 | Counts up to 99; `glint = false` and `tooltip = false` work |
| 1.21.2–1.21.3 | As 1.18 | As 1.20 | `item_model` |
| 1.21.4–26.3 | As 1.18 | As 1.20 | `model` with lists |

Clients up to 1.21.1 ignore single-slot updates of their inventory rows while
a menu is open. When the backend sent one meanwhile (a backend plugin gave an
item, say), Rift asks for the whole inventory once more when the menu closes.

## On the wire

For packets, container IDs, the inventory request per backend version and
how menus were tested, see [how menus work](menus-internals.md).
