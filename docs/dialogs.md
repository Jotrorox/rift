# Dialogs

Minecraft 1.21.6 added dialogs: screens with text, input fields and buttons
that a server can open on the client. Rift can show them to any player on
1.21.6 or newer (protocols 771 to 777, through 26.3), whatever software the
backend runs, and sends the button clicks back to the plugin that showed the
dialog instead of to the backend. A plugin can ask for a nickname on first
join, confirm a transfer, file a report, or pick a server from a list, without
a backend plugin.

Every plugin mode can do it:

| Mode | Show | Close | Receive clicks |
| --- | --- | --- | --- |
| `rift.lua` (API v2+) | `rift.players.dialog(player, dialog)` | `rift.players.clear_dialog(player)` | `rift.on("dialog", fn)` or `extensions.dialog` |
| Persistent Lua | `rift.players.dialog(player, dialog)` | `rift.players.clear_dialog(player)` | `rift.on("dialog", fn)` |
| WebAssembly | `players.dialog(target, dialog-json)` | `players.clear-dialog(target)` | the `dialogs` export ([Rust](plugins-rust.md), [C](plugins-c.md), [Go](plugins-go.md)) |

`dialog` is a table (Lua) or JSON text in the vanilla dialog format, the one
data packs use in `data/<namespace>/dialog/*.json` (see the
[Minecraft Wiki](https://minecraft.wiki/w/Dialog)). The call returns false
when the player is offline, older than 1.21.6 or has 32 undelivered actions,
and fails when the dialog is invalid.

## An example

```lua
-- rift.lua
rift.on("join", function(ctx)
    if rift.store.get("rules", ctx.uuid) then return end
    rift.players.dialog(ctx.uuid, {
        type = "confirmation",
        title = { text = "Server rules", bold = true },
        body = { "Be kind. No griefing. No cheats.", "Breaking a rule gets you banned." },
        can_close_with_escape = false,
        yes = { label = "I accept", action = { type = "custom", id = "accept" } },
        no = { label = "Leave", action = { type = "custom", id = "decline" } },
    })
end)

rift.on("dialog", function(ctx)
    if ctx.action == "accept" then
        rift.store.set("rules", ctx.uuid, "accepted")
    elseif ctx.action == "decline" then
        rift.players.kick(ctx.uuid, "You need to accept the rules to play here.")
    end
end)
```

## Which clicks come back

Rift looks at every `custom` and `dynamic/custom` action in the dialog:

- An `id` **without a namespace** (`"accept"`, `"send_report"`) is the
  plugin's own action. Rift sends it to the client as
  `rift:dialog/<serial>/<n>`, and a click comes back to the plugin with:
  - `action`: the `id` as the plugin wrote it;
  - `values`: the player's input values by key, for `dynamic/custom` (text
    and single-option inputs as strings, booleans as booleans, number ranges
    as numbers); empty for `custom`;
  - `payload`: the action's `payload` (`custom`) or `additions`
    (`dynamic/custom`), exactly as the plugin wrote it.
- An `id` **with a namespace** (`"myserver:vote"`) is sent unchanged, with
  its payload, and its clicks reach the backend as if the backend had shown
  the dialog. The `rift` namespace is reserved.

Rift keeps `payload` and `additions` instead of sending them to the client,
so a modified client can neither read nor change them: put the IDs and state
a click needs there. `values` come from the client like anything a player
types: check them. Values of the wrong type are dropped, and keys the dialog
does not declare never arrive.

Other actions (`open_url`, `run_command`, `suggest_command`, `change_page`,
`copy_to_clipboard`, `show_dialog`, `dynamic/run_command`) work as in vanilla.
A `run_command` or `dynamic/run_command` is sent like a command the player
typed, so one naming a Rift command (`/server`, `/hub`, a plugin command)
reaches Rift. The client may ask the player to confirm a command first.

The clicks of a dialog go only to whoever showed it: the configuration script
or one persistent plugin, by name. A WebAssembly plugin needs no entry in
`registration.hooks` for them.

## How long a dialog lives

The client shows one dialog at a time. Rift accepts clicks for the dialog it
showed last, until:

- a click on a button of a dialog with `after_action = "close"` (the
  default) closes it;
- the plugin shows another dialog or calls `clear_dialog`;
- the backend shows a dialog of its own or clears the screen;
- the player switches servers (configuration closes every screen) or leaves.

With `after_action = "none"` the dialog stays open and every click is
reported. With `"wait_for_response"` the client shows a waiting screen until
the plugin answers with a new dialog or `clear_dialog`. A dialog that pauses
the game (`pause`, default true) cannot use `"none"`. A click that arrives
while the player is switching servers is dropped, like a command.

`clear_dialog` closes only Rift's dialog: if the backend has shown one since,
it stays.

Pressing Escape runs the dialog's cancel action: the `action` button of a
`notice`, the `no` button of a `confirmation`, the `exit_action` of the other
types. When that is a plugin action, the plugin hears about it; otherwise the
dialog closes without anyone being told. Set `can_close_with_escape = false`
when the player must choose.

## What Rift accepts

A dialog the client cannot decode disconnects the player, so Rift checks the
definition first and fails the plugin's call instead. It accepts the subset
that every client from 1.21.6 to 26.3 decodes the same way, with the limits
those clients enforce. Unknown fields are errors, so a typo is caught rather
than ignored. `minecraft:` is optional in every `type`.

| Part | Accepted |
| --- | --- |
| Dialog types | `notice` (`action`), `confirmation` (`yes`, `no`), `multi_action` (`actions`, `columns`, `exit_action`), `server_links` (`columns`, `button_width`, `exit_action`), `dialog_list` (`dialogs`, `columns`, `button_width`, `exit_action`) |
| Common fields | `title` (required), `external_title`, `body`, `inputs`, `can_close_with_escape`, `pause`, `after_action` |
| Body | `plain_message` (`contents`, `width`), one or a list; a string or a text component table (one without `type`) is a plain message |
| Inputs | `text` (`max_length`, `initial`, `multiline`, `width`, `label_visible`), `boolean` (`initial`, `on_true`, `on_false`), `single_option` (`options`, `width`, `label_visible`), `number_range` (`start`, `end`, `step`, `initial`, `label_format`, `width`); every input has a `key` (letters, digits, `_`) and a `label` |
| Buttons | `label` (required), `tooltip`, `width` (1–1024), `action` |
| Actions | `open_url` (http or https), `run_command`, `suggest_command`, `change_page`, `copy_to_clipboard`, `show_dialog` (inline), `custom`, `dynamic/run_command` (every `$(key)` must name an input), `dynamic/custom` |
| Text | Every label, title and message is a text component: a string, a list or a component table |

Not accepted, because one client version may lack what they refer to:

- registry references (`"dialogs": "#minecraft:quick_actions"`,
  `show_dialog` with a dialog ID): give dialogs inline instead;
- `item` bodies: an item a player's version lacks would disconnect them.

Limits: 64 KiB of JSON per dialog, 32 inputs per dialog, 64 buttons per list,
64 plugin actions (including nested dialogs) and 4 levels of nested dialogs.

## On the wire

For protocol details see [network protocol](network-protocol.md#dialogs).
