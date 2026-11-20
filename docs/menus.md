# Inventory menus

> [!NOTE]
> **Status: design draft, nothing is implemented.** This page is the plan for
> the branch that adds inventory menus. It describes the plugin API first,
> because that is the part to agree on, and then how the proxy would do it.
> [Decisions](#decisions) records what the owner chose, and
> [what was checked](#what-was-checked) says how the client and server
> behavior this design relies on was confirmed. The page grows with the
> branch and becomes the user guide when it merges.

An inventory menu is a chest-style screen whose slots hold items that act as
buttons: a server selector, a shop page, a confirmation, a text prompt in an
anvil. Today a network needs a backend plugin on every server for that. With
this design a Rift plugin opens the menu through the proxy, on any backend
software and on every client from 1.12 to 26.3, and the clicks go to the
plugin instead of to the backend.

[Dialogs](dialogs.md) already work this way for 1.21.6 and newer clients.
Menus follow the same model (the plugin describes a screen, Rift shows it and
reports what the player does) and reach the older clients that have no
dialogs.

Two terms are used throughout. The **menu slots** are the menu's own slots,
the upper part of the screen. The **inventory rows** are the player's own 36
slots, which the client draws below every menu.

## Goals

1. **Four menu types:** a chest with 1 to 6 rows, a hopper (5 slots), a
   dispenser (3×3) and an anvil with its text field.
2. **Free content.** Every menu slot takes any item with any name, lore,
   model, head texture and count, and the title is any
   [styled text](text.md), so resource-pack interfaces work.
3. **One definition for every mode:** `rift.lua`, persistent Lua, and
   WebAssembly plugins in Rust, C and Go describe a menu the same way.
4. **Nothing reaches the backend.** A click on a menu never moves, creates or
   destroys a real item. The worst possible failure is a wrong picture on the
   client, which the backend's next inventory update corrects.
5. **Little code for the common case.** A server selector is one table and
   one handler.

### Non-goals

- **Menus that store items.** A player cannot put their own items into a
  menu or take items out. Menus are buttons, not containers.
- **Other menu types** (furnace, beacon, merchant, crafting, loom and so on).
  The design does not exclude them; they are not planned.
- **Clients older than 1.12.** Rift has no item or packet tables for them
  (see [decision 3](#3-which-clients-get-menus)). `menu` returns false for
  them, so supporting them later changes no plugin.
- **Remote plugins.** Like dialogs, menus are not offered to
  [bus responders](remote-decisions.md): their clicks need a plugin that
  holds state.
- **Using the inventory rows as buttons.** See [later](#later).

## For plugin developers

### A server selector

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

rift.command("servers", { run = function(ctx) selector(ctx.uuid) end })

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
is decoration.

### Functions

| Mode | Open or redraw | Change a part | Close | Receive events |
| --- | --- | --- | --- | --- |
| `rift.lua`, persistent Lua | `rift.players.menu(player, menu)` | `rift.players.update_menu(player, id, patch)` | `rift.players.close_menu(player, id)` | `rift.on("menu", fn)` or `rift.menus.on(id, handlers)` |
| WebAssembly | `players.menu(target, menu-json)` | `players.update-menu(target, id, patch-json)` | `players.close-menu(target, id)` | the `menu` hook ([contract](#the-contract)) |

- `menu` is a table (Lua) or JSON text in the format below.
- The calls are [queued actions](players-api.md#how-actions-are-delivered),
  like every other player action. They return true once queued, and false
  when the player is offline, older than 1.12 or has 32 undelivered actions.
  An invalid menu fails the call.
- A menu is shown only to a player who is in a world. One that meets a server
  switch is dropped, like a dialog.
- A player has at most one Rift screen. A menu replaces an open menu or
  dialog, and a dialog replaces an open menu.
- `close_menu` without an `id` closes whatever Rift menu is open, for any
  plugin. With an `id` it closes only that menu of the calling plugin.

### The menu definition

| Field | Default | Meaning |
| --- | --- | --- |
| `id` | required | 1–64 characters of `a-z`, `0-9`, `_`, `-`, `.`, `/`. Names the menu in every event. Showing a menu with the `id` of the menu the player has open [redraws it in place](#changing-an-open-menu) |
| `type` | `"chest"` | `chest`, `hopper`, `dispenser` or `anvil` |
| `rows` | lines of `layout`, else 3 | Chest only: 1 to 6. With a `layout` it must be the number of its lines |
| `title` | empty | [Text](plugin-events.md#text): a string or a component, including `{ mini = ... }` |
| `layout` | none | Lines of characters, one per slot. See the table below |
| `keys` | none | The button each layout character stands for. A space is always an empty slot; every other character in the layout needs an entry |
| `slots` | none | A list of buttons placed by number, each with `slot = n` or `slots = { n, ... }`. Placed after the layout, so they replace what the layout put there. `{ slot = n, empty = true }` empties one |
| `state` | none | Any JSON value of at most 16 KiB that Rift keeps with the menu and hands back in every event. See [state](#state-and-payloads) |
| `text` | `""` | Anvil only: what the text field starts with |
| `typing` | `false` | Anvil only: report a `text` event while the player types |

Slots are numbered from 0, left to right and top to bottom, as in the
protocol and in every Minecraft interface library.

| Type | Menu slots | Layout | Slot numbers |
| --- | --- | --- | --- |
| `chest` | rows × 9 | `rows` lines of 9 characters | 0–8 is the top row |
| `hopper` | 5 | 1 line of 5 | 0–4 |
| `dispenser` | 9 | 3 lines of 3 | 0–2 is the top row |
| `anvil` | 3 | 1 line of 3 | 0 left input, 1 right input, 2 result |

### Buttons

A button is an item with an optional action.

| Field | Default | Meaning |
| --- | --- | --- |
| `item` | required | An item name of the newest supported version, such as `diamond_sword`; `minecraft:` is optional. A client that lacks the item gets the [stand-in](translation.md#what-players-get) that translation already uses |
| `count` | 1 | 1 to 99. Clients before 1.20.5 get at most 64 |
| `name` | the item's own name | Text. Not italic unless the text says so (vanilla italicizes renamed items). On 1.12 a name without a color is white |
| `lore` | none | A list of up to 32 texts, one per line. Not italic unless the text says so |
| `glint` | the item's own | `true` adds the enchantment glint. `false` removes an item's own glint on 1.20.5 and newer; older clients keep it |
| `model` | none | Custom model data for resource packs: a number, or `{ floats, flags, strings, colors }` for 1.21.4 and newer |
| `item_model` | none | A model ID for 1.21.2 and newer. Older clients ignore it |
| `head` | none | For `player_head`: `{ player = "<name or UUID>" }` uses the skin of an online player, `{ texture = "<value>", signature = "<optional>" }` a texture property as head databases publish it |
| `color` | none | `"#RRGGBB"` for leather armor, potions and other dyeable items |
| `details` | `false` | `false` hides the lines the game adds on its own (attack damage, enchantments, potion effects). `true` shows them |
| `tooltip` | `true` | `false` hides the tooltip altogether on 1.20.5 and newer; older clients get an empty name |
| `id` | none | The action name reported when the button is clicked: the same character set as a menu `id`. Without one the button is decoration and its clicks are not reported |
| `payload` | none | Any JSON value of at most 4 KiB, handed back with the click. Rift keeps it; the client never sees it |
| `clicks` | the four mouse clicks | The [click kinds](#click-kinds) that report: a list, or `"all"`. Needs an `id` |
| `close` | `false` | Close the menu when the button is clicked with one of the four mouse clicks, without waiting for the plugin. A button with `close` needs no `id` |

Names and lore are checked like every other [styled text](text.md) and are
spelled for each player's client version by the same code that converts a
backend's items.

### Events

Everything a player does in a menu arrives as a `menu` event at whoever
showed it: the configuration script, or one persistent plugin by name. A
WebAssembly plugin needs no entry in `registration.hooks`, as for dialogs.
The event is an observer: it has no answer and no deadline of its own.

| Field | In | Content |
| --- | --- | --- |
| the [context](plugin-events.md) fields | all | The player. `server` is their current backend |
| `menu` | all | The menu's `id` |
| `kind` | all | `click`, `text` or `close` |
| `action` | `click` | The button's `id` |
| `slot` | `click` | The slot number |
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
and in persistent Lua plugins.

#### Click kinds

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
  mouse clicks, so pressing Q or a number over a button does nothing
  ([decision 5](#5-which-clicks-a-button-reports-by-default)).
- **A click is reported only if the player saw what they clicked.** If what
  a click on the slot does changed after the content the click refers to,
  Rift drops the click. A player who double-clicks "next page" never buys the
  item that appears under the cursor.
  - What a click does is the button's `id`, `payload` and `close`. A slot
    that only looks different (a new name, lore, count or item) still reports
    its clicks, so a countdown that redraws every second swallows none.
  - Clients before 1.17.1 do not say what they saw, so their clicks are
    always reported. An action that costs the player something should ask
    for confirmation on those clients.
- A player with 32 unanswered menu events gets no more until the plugin
  catches up, as with dialog clicks.

### State and payloads

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

### Changing an open menu

**Redraw.** Call `menu` again with the same `id`. If the player still has
that menu open, Rift compares the new definition with what the client shows
and sends only the difference: changed slots, and the title if it changed.
The screen does not close and the mouse does not move. A new title makes the
client build the screen again, so Rift then sends every slot once more; in an
anvil the text stays, with the text cursor at its end. If the type or the
number of rows changed, or the player has closed the menu meanwhile, the menu
opens anew. Redrawing is the simple way to change anything, and the paged
list above uses it.

**Patch.** `update_menu(player, id, patch)` changes a part without sending
the whole definition, for a clock or a countdown. It does nothing if the
player no longer has that menu open.

| Patch field | Effect |
| --- | --- |
| `slots` | Buttons with `slot` or `slots`, replacing those slots. `{ slot = n, empty = true }` empties one |
| `keys` | New buttons for layout characters, applied to every slot the layout gave that character |
| `title` | A new title |
| `state` | A new state |
| `text` | Anvil: replaces what the text field holds |

A redraw of an anvil keeps what the player typed; only a patch with `text`
replaces it.

### Text input with an anvil

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
  result slot a copy of it named after the typed text, with the `id`
  `submit`. A click on the result is the player's "OK", and closing the menu
  is "cancel".
- All three slots take buttons like any other menu. A layout such as
  `"P X"` puts a custom item left and a cancel button in the result slot.
  The left slot is never empty, because the client only allows typing while
  it holds an item.
- Every event of an anvil carries `text`. With `typing = true` Rift also
  sends a `text` event while the player types, at most 5 a second and always
  for the final value, for live validation.
- The client limits the text to 50 characters, 35 before 1.17. Rift enforces
  the same limit and removes control characters. The text is still player
  input.
- Clients from 1.13 to 1.19.4 do not tell the server when the field becomes
  empty. A player there who deletes everything is reported with the last
  character, so check a minimum length where it matters.
- On 1.12 and 1.13 the screen is always titled "Repair & Name", whatever
  `title` says, and on 1.12 the player has to click the field before typing.
- The client redraws the result and an "Enchantment cost" line by itself on
  every keystroke. Rift corrects both at once, but a custom result item can
  flicker for a moment while the player types.

### How long a menu lives

Every menu that was shown ends with exactly one `close` event, so a plugin
that keeps something per open menu can always release it.

| `reason` | The menu ended because |
| --- | --- |
| `player` | The player pressed Escape or the inventory key |
| `plugin` | A button with `close`, or `close_menu` |
| `replaced` | Rift showed another menu (a different `id`, or another plugin's) or a dialog |
| `server` | The backend opened a screen of its own, closed the player's screens, or the player died or changed worlds |
| `switch` | The player is switching servers. Rift closes the menu before the switch starts |
| `disconnect` | The player left |

A redraw is not a close. A click that arrives while the player is switching
servers is dropped, like a command.

### What Rift accepts

A menu the client cannot decode disconnects the player, so Rift checks the
definition first and fails the plugin's call instead. Unknown fields, unknown
item names and layout lines of the wrong length are errors.

A slot number outside the menu is an error that names the range, because a
Lua loop that counts from 1 is the likeliest mistake:
`slot 27: a chest with 3 rows has slots 0 to 26`.

| Limit | Value |
| --- | --- |
| JSON per menu | 256 KiB |
| `state` | 16 KiB |
| `payload` per button | 4 KiB |
| Lore lines per button | 32 |
| Undelivered menu events per player | 32 |
| `text` events per second | 5 |

### The contract

The additions make `rift:plugin@0.3.0`
([decision 2](#2-how-the-hook-reaches-webassembly-plugins)). The `menu` hook
is a function of `handler`, like every other hook. A component must export
exactly what `handler` lists, so a new hook is a breaking change and takes
the next minor version of the contract.

```wit
package rift:plugin@0.3.0;

// interface types
/// Something a player did in a menu this plugin showed with `players.menu`.
record menu-event {
    /// The player. `server` is their current backend.
    ctx: context,
    /// The menu's `id`.
    menu: string,
    /// `click`, `text` or `close`. Later versions may add kinds: ignore the
    /// ones you do not know.
    kind: string,
    /// `click`: the button's `id`.
    action: option<string>,
    /// `click`: the slot number, from 0.
    slot: option<u32>,
    /// `click`: `left`, `right`, `shift_left`, `shift_right`, `middle`,
    /// `drop`, `drop_all`, `number_key` or `offhand`.
    click: option<string>,
    /// `click` with `number_key`: 1 to 9.
    key: option<u32>,
    /// Anvil menus: what the text field holds. It comes from the client.
    text: option<string>,
    /// `close`: `player`, `plugin`, `replaced`, `server`, `switch` or
    /// `disconnect`.
    reason: option<string>,
    /// `click`: the button's `payload` as JSON, as the plugin wrote it.
    payload-json: option<string>,
    /// The menu's `state` as JSON, as the plugin last set it.
    state-json: option<string>,
}

// interface players
/// Show a menu to a player on Minecraft 1.12 or newer, or redraw the open
/// menu with the same `id`. `menu-json` is a menu definition; see
/// docs/menus.md. Returns false when the player is offline, older than 1.12
/// or has too many undelivered actions; an invalid menu is an error.
menu: func(target: string, menu-json: string) -> result<bool, string>;
/// Change part of the menu `menu`, if the player still has it open.
update-menu: func(target: string, menu: string, patch-json: string) -> result<bool, string>;
/// Close the menu `menu` of this plugin, or with `none` any Rift menu.
close-menu: func(target: string, menu: option<string>) -> result<bool, string>;

// interface handler
/// A player clicked, typed in or left a menu this plugin showed. Needs no
/// entry in `registration.hooks`, like `dialog`.
menu: func(event: menu-event);
```

`kind`, `click` and `reason` are strings, not enums. A later 0.3 release can
then add a close reason or a click kind without another breaking version.
Each SDK turns them into an enum of its own with a case for a value it does
not know, so a plugin author never compares strings.

#### Moving from 0.2

0.3.0 is 0.2.1 plus the `menu` hook and the three functions. Nothing else
changes, so the move needs no change in a plugin's source:

| Plugin | What changes |
| --- | --- |
| `rift.lua` | Nothing |
| Persistent Lua | `api = "0.3"` in `plugin.toml` |
| WebAssembly | `api = "0.3"`, the 0.3.0 SDK and a rebuild. Every SDK gives `menu` a default, as it does for the other hooks |

- A Rift with this contract loads only `api = "0.3"` plugins. It refuses a
  0.2 manifest with a pointer to a "Moving from 0.2" section in
  [plugins](plugins.md), as Rift does today for 0.1
  ([decision 7](#7-what-happens-to-02-plugins)).
- `min_api` starts again on the new track: a plugin that uses menus needs
  none, because every 0.3 host has them.
- The [stability policy](stability.md) keeps the contract compatible across
  patch releases of Rift, so 0.3.0 ships in the next minor release of Rift.
- Every WebAssembly plugin is rebuilt for 0.3.0 either way. Any other change
  to the contract that needs a breaking version should be collected before
  step 7 of the [plan](#plan), so that plugins are rebuilt once. This page
  plans none.

### The SDKs

Each SDK gets the raw calls and a typed builder that produces the same JSON.
They are released as 0.3.0, after the contract.

**Rust**

```rust
use rift_plugin::menu::{Button, Menu, MenuEvent, MenuKind};
use rift_plugin::{Context, Plugin, players, text};

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
        .key('X', Button::new("barrier").name(text::mini("<red>Close")).close())
}

impl Plugin for Lobby {
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
```

| Contract | Rust | Go | C |
| --- | --- | --- | --- |
| `players.menu` | `players::show_menu(target, &Menu)`, or `players::menu` with JSON text | `rift.ShowMenu(target, rift.Menu{...})` | `rift_show_menu(&target, json)` |
| `players.update-menu` | `players::update_menu(target, id, &MenuPatch)` | `rift.UpdateMenu(target, id, rift.MenuPatch{...})` | `rift_update_menu(&target, id, json)` |
| `players.close-menu` | `players::close_menu(target, Some(id))` | `rift.CloseMenu(target, id)` | `rift_close_menu(&target, id)` |
| `menu` hook | `Plugin::menu(MenuEvent)`, a trait method with an empty default. `event.kind()` is an enum with an `Other` case; `event.payload::<T>()` and `event.state::<T>()` decode the JSON | `rift.OnMenu(func(rift.MenuEvent))`; `event.Kind` and `event.Click` are typed constants | `exports_rift_plugin_handler_menu(rift_menu_event_t *)`, with a weak default that ignores events; `rift_menu_kind(event)` and `rift_menu_click(event)` return enums |
| menu definition | `Menu::chest(id)`, `::hopper`, `::dispenser`, `::anvil`; `.rows`, `.title`, `.layout`, `.key`, `.slot`, `.state`; `Button::new(item)` with one method per field | Plain structs: `rift.Menu`, `rift.Button` | JSON text |

The builders check nothing themselves: the host validates, so all four
languages get the same errors. `examples/plugins/hello-*` gains the same menu
in all four languages, as the conformance fixture.

## How the proxy does it

The backend never learns that a menu is open. Rift writes the menu's packets
to the client itself, in the client's protocol, and takes the client's
packets for that menu out of the stream before the backend or the translator
sees them. This is how [dialogs](network-protocol.md#dialogs) work, with more
packets.

### Packets

| Step | To the client | From the client (never forwarded) |
| --- | --- | --- |
| Open | `open_screen` with Rift's container ID, the menu type and the title; then one `container_set_slot` per filled menu slot | |
| Redraw or patch | `container_set_slot` for changed slots. For a new title `open_screen` again with the same ID and then every slot, because the client builds the menu anew | |
| Click | The slots the click could have changed, again, and an empty cursor | `container_click`; `set_creative_mode_slot` from 1.21.2 |
| Anvil | The result slot and a cost of 0 (`container_set_data`) after each change of the text | `rename_item`; the `MC\|ItemName` plugin message on 1.12 |
| Close | `container_close` when Rift closes the menu | `container_close` when the player does |

`rift-translation` already reads and writes all of these in every protocol's
layout (`codec/item/window.rs`, `codec/misc/screen.rs`); it handles these
differences:

| | 1.12–1.13.2 | 1.14–1.16.5 | 1.17 | 1.17.1–1.21.1 | 1.21.2 and newer |
| --- | --- | --- | --- | --- | --- |
| Menu type | A window name and a slot count (0 for an anvil) | A `menu` registry ID | same | same | same |
| Container ID | A byte | A byte | A byte | A byte | A VarInt |
| A click names | An action number and the clicked stack | same | The slots it changed | The slots it changed and a state ID | same; stacks are hashes from 1.21.5 |
| Empty the cursor | `container_set_slot` with container −1 | same | same | same | `set_cursor_item` |

**The container ID.** Vanilla servers number their containers 1 to 100. Rift
takes its IDs from 101 to 127, which fit every layout, and uses a new one for
each menu it opens, so a late click on an earlier menu cannot be mistaken for
a click on the current one. A client packet belongs to Rift when it names the
ID of the open Rift menu, or of the last one if the backend has not opened
that ID since.

**State IDs.** From 1.17.1 Rift counts its own state IDs for its container
and remembers, per slot, the state in which the slot's `id`, `payload` or
`close` last changed. A click that names an older state than its slot's is
the stale click that is [dropped](#click-kinds).

**Opening over a backend screen.** If the backend has a container open when
Rift opens a menu, the client replaces it without telling anyone and drops
what the player held on the cursor on its side only. Rift sends
the backend the `container_close` the client would have sent on Escape, so
the backend's idea of the player stays right. For this Rift remembers the ID
of the backend's open container: one integer per session, read from
`open_screen` and the close packets.

### Clicks and what the client predicts

The client does not wait for the server. It applies every click to its own
copy of the screen first and then sends it, whatever the click is. Rift has
to put the picture back, because nothing really moved.

| The click | What the client changed on its side |
| --- | --- |
| Left or right click | The clicked slot and the cursor |
| Shift-click on a menu slot | The clicked slot and any slots of the inventory rows |
| Number key, or F from 1.16 | The clicked slot and one hotbar slot or the off hand |
| Q or Ctrl+Q | The clicked slot |
| Middle click | The cursor, in creative mode |
| Double click | The cursor and every slot that holds the same item, inventory rows included |
| Drag | The slots dragged over and the cursor |

- **Menu slots.** Rift knows what every menu slot holds and re-sends the ones
  the click could have changed. From 1.17 the click lists the slots the
  client changed. Before that Rift re-sends the clicked slot, or all menu
  slots for a double click or a drag.
- **The cursor.** Rift empties it with the packet the client's version
  applies while a menu is open: `container_set_slot` for container −1 up to
  1.21.1, `set_cursor_item` from 1.21.2, which ignores the older form.
- **The inventory rows.** Rift does not know what the player carries, so it
  cannot re-send those slots. The next section is about them. From 1.17 the
  click's list says whether it touched them at all. Before 1.17 Rift assumes
  it for a shift-click, a number key, F, a double click, a drag and any click
  on the inventory rows.
- A click on a decoration button, or of a kind the button does not report,
  is undone the same way and reported to nobody.
- **Creative mode.** From 1.21.2 a client in creative mode that presses Q
  over a slot also asks the server to create the dropped item
  (`set_creative_mode_slot`). Rift drops that packet while one of its menus
  is open, so a button never becomes a real item.

### The inventory rows

When a menu opens, the client keeps showing its own inventory in the lower
rows, so Rift sends nothing for them. They only become wrong when a click
changes them on the client: a click on an inventory slot, a shift-click, a
number key or F over a menu slot, or a double click.

After such a click Rift asks the backend to send the inventory again. The
protocol has a path for this, because a client and a server can disagree
about an inventory in normal play. These requests were run against 26 real
servers ([what was checked](#what-was-checked)):

| Backend | Rift sends the backend | The backend answers |
| --- | --- | --- |
| 1.17.1 and newer | A left click between the slots of the player's own inventory (container 0, slot −1) with the state ID −1, which no server expects | One `container_set_content` with all 46 slots and the cursor |
| 1.12 to 1.16.5 | The same click, claiming that it picked up a stack. The server computes an empty result and disagrees | A rejection (`container_transaction`), the whole inventory, and the cursor. The backend then ignores every click of this player until the rejection is acknowledged |
| 1.17 | The same click, claiming a wrong stack for every slot and the cursor | One `container_set_slot` for each of the 46 slots, and the cursor |

The answer travels to the client like any backend packet, through the
translator when there is one. The client applies it to its inventory, and
the inventory rows are right again.

- The click between the slots does nothing on any server, whatever the
  player holds. A click outside the window (slot −999) would drop what the
  player holds, so Rift never sends that.
- Rift writes the request in the backend's protocol, past the translator. A
  translator would repair the state ID that is wrong on purpose.
- **A rejection is Rift's to acknowledge.** Rift takes the rejection of its
  own request out of the stream and sends the backend the acknowledgement
  itself, so this works the same for every client and translator. Until then
  the backend ignores clicks, the player's real ones included, so Rift
  acknowledges at once.
- Rift never has more than one request on its way, counted until the answer
  and, for a rejection, its acknowledgement. Further clicks meanwhile cause
  one more request afterwards, so a player who clicks fast costs the backend
  at most a few inventory packets a second.
- Rift keeps no copy of the player's inventory and parses no item for this.
  The picture is repaired from the only place that knows the truth.
- The repair takes one round trip to the backend. For that moment the player
  sees their own item where they dragged it.

**Single-slot updates while a menu is open.** While another container is
open, clients up to 1.21.1 apply a `container_set_slot` for container 0 only
to the hotbar. Two things follow:

- A backend that changes the player's inventory while a Rift menu is open,
  for example a plugin that gives an item, sends one slot at a time, and such
  a client drops the update. Rift notes that it passed one on and asks for
  the inventory once more when the menu closes.
- A 1.17 backend answers the request slot by slot. Rift asks a 1.17 backend
  when the menu closes, not while it is open. Until then the inventory rows
  of that player can show what they dragged.

What this costs: the backend sees a harmless click on the player's own
inventory. On Paper and Spigot that is expected to fire an
`InventoryClickEvent` with the action `NOTHING`; Pumpkin fires its own click
event. Some Paper and Spigot versions also send one extra update for the
empty crafting result slot. A backend that does not answer leaves the
inventory rows wrong until its next inventory update. No item is ever at
risk, because the backend's inventory never changed.
[Decision 1](#1-how-the-inventory-rows-are-repaired) says what follows from
the test results.

### The anvil

- The client lets the player type only while the left slot holds an item. It
  starts the text field with that item's name and fills it again whenever
  that item changes. Rift puts the left button there with `text` as its name
  and does not send that slot again while the player types.
- The client sends the text when it changes and when the left slot is set:
  the `rename_item` packet from 1.13, the `MC|ItemName` plugin message on
  1.12, which sends it on every key. Rift ignores a text it already has.
  Clients from 1.13 to 1.19.4 do not send an empty text, so there an emptied
  field is reported with its last character.
- Each change makes the client compute a result (the left item, renamed) and
  a cost of at least 1 level by itself. Rift remembers the text, sends the
  result slot and a cost of 0 again, and reports a `text` event if the menu
  asked for it. A cost of 0 hides the "Enchantment cost" line and stops the
  client from taking the result on its own.
- The result slot is never empty. From 1.14 the client draws a red cross
  over the arrow when it is.
- A click on the result slot is always sent. Only a click that races a
  keystroke finds a cost above 0. The client then takes the cost off the
  shown levels and empties the input slots on its own, so after every result
  click Rift re-sends the three slots, the cost and the last
  `set_experience` it saw from the backend. That packet has three fixed
  fields, so remembering it costs nothing.
- On 1.12 the anvil is announced with 0 slots. With any other number the
  client shows a chest. Clients on 1.12 and 1.13 always title the screen
  "Repair & Name".

### What ends a menu

Rift ends its menu, with the [reason](#how-long-a-menu-lives) for the plugin,
when it sees:

| Rift sees | Reason |
| --- | --- |
| `container_close` from the client for Rift's ID | `player` |
| `open_screen`, `mount_screen_open`, `open_sign_editor`, `open_book` or a dialog from the backend, or its `container_close`, which closes whatever the client has open | `server` |
| A respawn, or a new login from the backend | `server` |
| The start of a server switch or of a reconfiguration | `switch` |
| The end of the session | `disconnect` |

### Items

A plugin names an item once. Each client needs it in its own layout: numeric
IDs with a data value on 1.12, flattened IDs from 1.13, NBT tags until
1.20.4, data components from 1.20.5, with names and lore spelled differently
along the way.

`rift-translation` converts exactly this for backend items, one version step
at a time, with stand-ins for items a client lacks. Rift builds each button
as a stack of the newest protocol (26.3, data components) and runs it down
the same chain to the client's version. This needs one new entry point in the
crate: convert one stack from the newest protocol to a given one, outside a
connection. It needs no second item encoder.

- A converted stack depends only on the button and the protocol, so it is
  cached per menu definition and protocol. A menu shown to 200 players on the
  same version is encoded once.
- Texts are resolved (markup, legacy codes, placeholders) before the stack is
  built, as for dialogs.
- `head = { player = ... }` reads the signed `textures` property Rift already
  holds for online players.

### What stays true

- The backend's inventory, cursor and containers are never changed by a menu.
- A client packet for a Rift container never reaches a backend, even after
  the menu closed.
- Rift never trusts a stack the client sends. It reads only the slot number,
  the button, the mode and the state ID from a click, and the string from a
  rename.
- A malformed click or rename closes the session, as for dialogs and as
  vanilla does.
- A menu costs a session nothing until one opens: the only state kept for
  every player is the backend's open container ID and its last
  `set_experience`.

## Code changes

| Where | What |
| --- | --- |
| `src/menu.rs` | The definition: parsing, validation, limits, the layout, diffing two definitions, resolving a click to a button. The counterpart of `src/dialog.rs` |
| `src/protocol/menu.rs` | Building and reading the packets above for every protocol, on top of the translation crate's window and screen codecs |
| `crates/rift-translation` | A public way to convert one stack from the newest protocol to a client's; item and menu lookups by name; the window writers made public where they are not |
| `src/session.rs` | The open menu and its owner, taking Rift's packets out of the client stream, undoing clicks, the inventory request, every ending |
| `src/plugins/`, `src/lua/api.lua` | `menu`, `update_menu`, `close_menu` and the `menu` event for the script, persistent Lua and WebAssembly; `rift.menus.on` |
| `wit/`, `sdk/rust`, `sdk/c`, `sdk/go` | Contract 0.3.0, bindings, builders, the `hello` examples |
| `src/plugins/manifest.rs`, every `plugin.toml` in the repository | `api = "0.3"`; a 0.2 manifest is refused with a pointer to the guide |
| `docs/` | This page as the guide; rows in [players API](players-api.md), [plugin events](plugin-events.md), the language guides and [network protocol](network-protocol.md); "Moving from 0.2" in [plugins](plugins.md); the contract row in [stability](stability.md) |

## Plan

Each step is a commit series on the branch with its tests. The branch merges
when all are done.

1. **Design** (this page), and the owner's [decisions](#decisions).
2. **Check the ground.** Done: [what was checked](#what-was-checked). The
   probe for the inventory request is `tests/menus_probe.py`.
3. **Items.** The stack entry point in `rift-translation`, buttons to stacks,
   text defaults, heads, the cache. Tests compare the bytes for every protocol
   with stacks from real servers.
4. **Chest menus in Lua.** The definition, opening, clicks, undoing, the
   inventory request and every ending, with `rift.players.menu`,
   `close_menu`, the `menu` event and `rift.menus.on` for `rift.lua` and
   persistent Lua. Session tests with a scripted client and backend, as for
   dialogs.
5. **Hopper and dispenser; redraw and patch.** Diffing, `update_menu`, state
   IDs and the stale-click rule.
6. **Anvil.** Text tracking, result and cost, the experience bar, `typing`.
7. **Contract 0.3.0.** The WIT with the `menu` hook in `handler`, the host
   side, `api = "0.3"` in the manifest check and in every example, the Rust,
   C and Go SDKs as 0.3.0 with builders, the `hello` examples in four
   languages, and "Moving from 0.2" in [plugins](plugins.md) with the
   changelog entry.
8. **Real clients and the guide.** The real-client run on the oldest and
   newest clients for every menu type, the compatibility matrix, and this
   page rewritten as the user guide.

## Decisions

The owner decided these on 2026-11-20. Each entry names what was chosen, what
it costs and what was turned down.

### 1. How the inventory rows are repaired

**Decided: ask the backend**, as described [above](#the-inventory-rows).

- It is exact and Rift keeps no inventory. It needs nothing installed on the
  backend: the request is a packet every server already understands.
- It depends on the backend answering the way vanilla does, takes one round
  trip, and plugins on the backend see a harmless click.
- Turned down: **keeping a copy** of every player's inventory in Rift. The
  client changes its inventory in many ways the server confirms without a
  packet (accepted clicks in backend containers, placed blocks, used items),
  so a copy drifts unless Rift simulates the game's inventory rules, and a
  drifted copy shows the player items they do not have. Every session would
  also pay for parsing its inventory packets, whether it ever sees a menu or
  not.

Step 2 of the [plan](#plan) settles what happens next:

| Step 2 finds | Then |
| --- | --- |
| Vanilla, Paper, Spigot and Pumpkin all answer | Nothing else is built. **This is what step 2 found**, for [26 servers](#what-was-checked) |
| Only Pumpkin or a 1.17 backend does not answer | Those are documented: the inventory rows may stay wrong until the backend's next inventory update |
| Vanilla or Paper does not answer, or the round trip is plainly visible in play | Rift also keeps a copy and corrects it by asking |

### 2. How the hook reaches WebAssembly plugins

**Decided: a `menu` function in `handler`, as `rift:plugin@0.3.0`.** See
[the contract](#the-contract).

- One interface holds every hook, in the WIT and in each SDK.
- Every plugin moves to `api = "0.3"` and every WebAssembly component is
  rebuilt, without a change to its source.
- Turned down: **a separate optional interface in 0.2.1**, which would have
  kept 0.2 components loading at the price of hooks in two interfaces and
  one export the host looks up by hand. **Holding the WebAssembly side
  back** until 0.3 was due for other reasons.

### 3. Which clients get menus

**Decided: 1.12 to 26.3**, the range `rift-translation` has item and packet
tables for.

- `menu` returns false for older clients, as `dialog` does below 1.21.6. A
  plugin that serves them needs another way, such as chat.
- Turned down for now: **1.8.9 to 1.11.2**, which needs hand-written packet
  IDs and an item table for seven more protocols. Adding them later changes
  no plugin.

### 4. How slots are numbered

**Decided: from 0 in every language**, with layouts for most uses.

- The same number in Lua, Rust, Go, C, JSON, events and the protocol, and in
  every tutorial a plugin author has read.
- It is not how Lua counts, so an out-of-range slot is an
  [error that names the range](#what-rift-accepts).
- Turned down: **from 1 in Lua.** The same menu would have different numbers
  in two languages, and the Lua side would convert in definitions, patches
  and events.

### 5. Which clicks a button reports by default

**Decided: the four mouse clicks.**

- A player who presses Q or a number over a "buy" button buys nothing. The
  other kinds are one field away (`clicks`).
- Turned down: **every kind**, which would make every handler filter.

### 6. Whether stale clicks are dropped

**Decided: yes.** A click on a slot whose action changed after what the
player saw is not reported (1.17.1 and newer clients).

- A slot counts as changed when its `id`, `payload` or `close` changed, not
  when it only looks different. See [click kinds](#click-kinds).
- Turned down: **reporting every click** and leaving the guard to each
  plugin.

### 7. What happens to 0.2 plugins

**Decided: Rift refuses them, for every engine.** A Rift with contract 0.3
loads only `api = "0.3"` plugins.

- A 0.2 manifest is refused with a pointer to the guide, as a 0.1 manifest
  is today. The move is one line in the manifest, and a rebuild for
  WebAssembly.
- Persistent Lua plugins change `api` too, although nothing else changes for
  them: one rule for every engine, as in the move from 0.1.
- The [stability policy](stability.md) asks for one minor release of
  deprecation before an interface is removed. The owner skips it here: the
  contract has no users besides the owner yet.
- Turned down: **loading both versions for one minor release**, which means
  two sets of host bindings, and **accepting `api = "0.2"` for Lua
  plugins**, which would make `api` mean different things per engine.

## What was checked

Step 2 of the [plan](#plan) checked the behavior this design relies on in two
ways, on 2026-11-20. The sections above already say what was found.

### Servers, by experiment

`tests/menus_probe.py` joins a real server as a scripted client, without
Rift, and sends the [inventory request](#the-inventory-rows). It ran against
26 servers:

| Software | Versions |
| --- | --- |
| Vanilla | 1.12.2, 1.13.2, 1.14.4, 1.15.2, 1.16.5, 1.17, 1.17.1, 1.18.2, 1.19.4, 1.20.4, 1.20.6, 1.21.1, 1.21.4, 1.21.5, 1.21.11, 26.3 |
| Paper | 1.12.2, 1.16.5, 1.17.1, 1.21.11, 26.3 |
| Spigot | 1.12.2, 1.16.5, 1.21.11, 26.3 |
| Pumpkin | 0.2.0 for 26.3 |

- Every server sent the whole inventory and the cursor back for every
  request, with and without an item on the cursor.
- No request changed the inventory or the cursor. This was compared in the
  inventory the server reported, in a second request, and in the server's own
  view (`clear` counts, and `data get` from 1.13) where the server has one.
- 50 requests in 10 seconds were all answered, and no server warned or
  kicked. Of 20 requests within one second all were answered, except where a
  request overtook the acknowledgement of the one before on a 1.12.2 server.
  That is the rule about acknowledgements, not a rate limit.
- A state ID of −1 always works from 1.17.1, so Rift does not follow the
  backend's state IDs.
- A click outside the window (slot −999) dropped the stack on the cursor on
  every server. A click between the slots (slot −1) kept it.

### Client and server code, by reading

Mojang's client and server were read for 1.12.2, 1.13.2, 1.14.4, 1.16,
1.16.5, 1.17, 1.17.1, 1.18.2, 1.19.4, 1.20.4, 1.21.1, 1.21.2, 1.21.4, 1.21.5
and 26.3, and Pumpkin at the commit `tests/servers.json` pins. 1.12.2 and
1.13.2 have no official names for their code, so those answers rest on
community names.

| Question | Answer |
| --- | --- |
| A menu opens over another screen | The client replaces it and sends no close. The mouse stays where it is. What the player held on the cursor is dropped on the client's side only; up to 1.17 the client also swings its arm |
| A menu that got no contents | Its own slots are empty. The inventory rows are the player's real inventory |
| `open_screen` with the ID of the open menu | The client builds the menu anew: empty slots, empty cursor, a new anvil text field. No packet changes a title in place |
| Contents for container 0 while a menu is open | `container_set_content` is always applied. `container_set_slot` is applied only for the hotbar up to 1.21.1, and for every slot from 1.21.2 |
| Emptying the cursor | `container_set_slot` for container −1 up to 1.21.1; `set_cursor_item` from 1.21.2, where the older form is ignored |
| `container_close` from the server | Closes whatever screen is open, whatever ID it names. The client sends nothing back |
| Container IDs | Vanilla servers use 1 to 100. 101 to 127 fit every layout; 128 and above would be negative in the click and `container_set_slot` up to 1.21.1 |
| Clicks | Always sent, after the client applied them to its own copy. The packet names an action number and the clicked stack up to 1.16.5, the changed slots and the cursor from 1.17, a state ID from 1.17.1, and hashes of stacks from 1.21.5 |
| The state ID in a click | The one of the last slot or contents packet the client got for that container; 0 for a menu that got none |
| Transactions up to 1.16.5 | The client never waits for one. It acknowledges a rejection for container 0 or the open container by itself |
| Middle click | Sent in every game mode up to 1.17.1, only in creative mode from 1.18 |
| F over a slot | Sent from 1.16 |
| The anvil | As [described](#the-anvil). The client allows 35 characters up to 1.16.5 and 50 from 1.17 |

### Not checked

- The event Paper and Spigot fire for the request. It would need a plugin on
  the backend.
- Shift-clicks in a hopper and a dispenser, which are assumed to move items
  as in a chest.
- The request in game modes other than survival.
- Real clients. Step 8 runs them.

## Later

Not planned, but the design leaves room:

- **The inventory rows as part of the menu.** Rift could draw its own items
  over the player's 36 slots while a menu is open and ask the backend for the
  real ones when it closes. It would have to hold back the backend's
  inventory updates meanwhile.
- **Click sounds** per button.
- **Raw data components** on a button, in the newest version's format, for
  what the typed fields do not cover.
- **More menu types.**
