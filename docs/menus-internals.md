# How menus work

For contributors: how Rift shows [inventory menus](menus.md), which packets
it writes and takes out of the stream for each protocol, how it asks a
backend to repair the player's inventory, and how all of this was checked.

The backend never learns that a menu is open. Rift writes the menu's packets
to the client itself, in the client's protocol, and takes the client's
packets for the menu out of the stream before the backend or the translator
sees them. [Dialogs](network-protocol.md#dialogs) work this way too, with
fewer packets.

| Where | What |
| --- | --- |
| `src/menu/mod.rs` | The definition: parsing and checking, limits, layouts, patches, the diff of two definitions, the anvil's text rules |
| `src/menu/event.rs` | The `menu` event, close reasons, the count of undelivered events, what a session receives (`Shown`, `Update`) |
| `src/menu/stack/mod.rs` | Buttons as item stacks for every client, and the stack cache |
| `src/protocol/menu.rs` | The packets: building them, reading clicks and renames, the backend packets that end a menu, the inventory request and its acknowledgement |
| `src/session/menus.rs` | The open menu in a session: opening, redraws and patches, undoing clicks, the inventory request, every ending |
| `src/extensions/players.rs` | `menu`, `update_menu` and `close_menu` as queued player actions, on this peer or another one |
| `src/extensions.rs`, `src/plugins/` | Delivering events to `rift.lua` and persistent plugins, and back to another peer |
| `src/lua/menus.lua` | `rift.menus.on` |

## Packets

| Step | To the client | From the client (never forwarded) |
| --- | --- | --- |
| Open | `open_screen` with Rift's container ID, the menu type and the title; then one `container_set_slot` per filled menu slot; for an anvil a cost of 0 | |
| Redraw or patch | `container_set_slot` for the changed slots. For a new title `open_screen` again with the same ID and then every filled slot, because the client builds the menu anew | |
| Click | The slots the click could have changed, again, and an empty cursor | `container_click` |
| Anvil text | The result slot and a cost of 0 (`container_set_data`, property 0) after each rename | `rename_item`; the `MC\|ItemName` plugin message on 1.12 |
| Close | `container_close` when Rift closes the menu | `container_close` when the player does |
| Other | | Every other packet that names Rift's container: `container_button_click`, `container_slot_state_changed`, the client's `container_ack` up to 1.16.5, and `place_recipe` (`crafting_recipe_placement` on 1.12). While a menu is open, the packets only a screen of the game's sends, which name no container: `rename_item` for a chest, hopper or dispenser, `select_trade`, `set_beacon` and `bundle_item_selected` (on 1.12 the `MC\|ItemName`, `MC\|TrSel` and `MC\|Beacon` plugin messages). `set_creative_mode_slot` while a menu is open and for a second after Rift closed it, on every version |

Packet IDs come from the translation crate's per-protocol tables, and the
bodies from its window and screen codecs (`codec/item/window.rs`,
`codec/misc/screen.rs`). These differences between protocols matter:

| | 1.12–1.13.2 | 1.14–1.16.5 | 1.17 | 1.17.1–1.21.1 | 1.21.2 and newer |
| --- | --- | --- | --- | --- | --- |
| `open_screen` | A byte container ID, a window name and a slot count (0 for an anvil) | A VarInt container ID and a `menu` registry ID | same | same | same |
| Container ID in clicks, closes and slots | A byte | A byte | A byte | A byte | A VarInt |
| A click names | An action number and the clicked stack | same | The slots it changed | The slots it changed and a state ID | same; stacks are hashes from 1.21.5 |
| Empty the cursor | `container_set_slot` with container −1 | same | same | same | `set_cursor_item` |

Titles are JSON text up to 1.20.2 and NBT from 1.20.3, written by the code
that writes every other text Rift sends. On 1.12 an anvil must be announced
with 0 slots: with any other number the client shows a chest.

### Container IDs

Vanilla servers number their containers 1 to 100. Rift takes its IDs from
101 to 127, the largest range that stays positive where a container ID is a
signed byte, and takes the next one for each menu it opens, skipping the ID
of the backend's open container. A late click on an earlier menu therefore
cannot be mistaken for a click on the current one.

Rift remembers which of its IDs it used and how many slots each menu had. A
client packet that names one of them is Rift's, even after the menu closed,
until the backend opens a container with that ID itself. A late click that
may have touched the inventory rows still makes Rift
[ask for the inventory](#the-inventory-request).

### State IDs and stale clicks

From 1.17.1 a click carries the state ID of the last slot or contents packet
the client got for that container, 0 for a menu that got none. Rift counts
its own state IDs for its container, from 1 to 32767 and then from 1 again,
and every update it sends (opening, a redraw, a patch, the answer to a click
or a rename) takes the next one.

For each slot Rift remembers the update in which the slot's `id`, `payload`
or `close` last changed. A click whose state ID names an older update than
its slot's is stale: Rift undoes it and neither reports it nor closes the
menu. A redraw sends a slot whose action changed even when it looks the same,
so the client's next click names the update that showed the new action.
Clients before 1.17.1 send no state ID, so their clicks are never stale.

### Opening over a backend screen

If the backend has a container open when Rift opens a menu, the client
replaces it without telling anyone and drops what the player held on the
cursor on its side only. Rift sends the backend the `container_close` the
client would have sent on Escape, so the backend's idea of the player stays
right. For this Rift follows the ID of the backend's open container: one
integer per session, read from `open_screen`, `mount_screen_open` and the
close packets of both sides.

## Undoing clicks

The client does not wait for the server. It applies every click to its own
copy of the screen first and then sends it, whatever the click is. Rift puts
the picture back, because nothing really moved.

| The click | What the client changed on its side |
| --- | --- |
| Left or right click | The clicked slot and the cursor |
| Shift-click on a menu slot | The clicked slot and any slots of the inventory rows |
| Shift-click on the inventory rows | The clicked slot and any menu slots |
| Number key, or F from 1.16 | The clicked slot and one hotbar slot or the off hand |
| Q or Ctrl+Q | The clicked slot |
| Middle click | The cursor, in creative mode |
| Double click | The cursor and every slot that holds the same item, inventory rows included |
| Drag | The slots dragged over and the cursor |

- **Menu slots.** Rift knows what every menu slot holds and sends again the
  ones the click could have changed. From 1.17 the click lists the slots the
  client changed. Before 1.17 Rift sends the clicked slot again, or every
  menu slot for a double click, a drag or a shift-click on the inventory
  rows. In an anvil it always sends all three slots.
- **The cursor.** Rift empties it with the packet the client's version
  applies while a menu is open: `container_set_slot` for container −1 up to
  1.21.1, `set_cursor_item` from 1.21.2, which ignores the older form.
- **The inventory rows.** Rift does not know what the player carries, so it
  cannot send those slots again; it asks the backend instead. From 1.17 the
  click's list says whether it touched them. Before 1.17 Rift assumes it for
  a click on the inventory rows, a shift-click, a number key, a double click
  and a drag. F always counts, because the off hand is not one of the
  window's slots and never in the list.
- A click on a decoration button, or of a kind the button does not report,
  is undone the same way and reported to nobody.
- **Creative mode.** A client in creative mode can ask the server to create
  the item it drops or picks (`set_creative_mode_slot`). Rift drops that
  packet while one of its menus is open, so a button never becomes a real
  item. When Rift ends the menu itself (a `close` button, `close_menu`,
  another screen, a switch), the client learns of it one round trip later
  and may have sent one for a button meanwhile, so Rift keeps dropping the
  packet for one second. A creative player who really drops or takes an
  item in that second loses that one action. After the player closed the
  menu themselves the packet passes at once.

## The inventory request

When a menu opens, the client keeps showing its own inventory in the lower
rows, so Rift sends nothing for them. They only become wrong when a click
changes them on the client. Rift then asks the backend to send the inventory
again. The protocol has a path for this, because a client and a server can
disagree about an inventory in normal play:

| Backend | Rift sends the backend | The backend answers |
| --- | --- | --- |
| 1.17.1 and newer | A left click between the slots of the player's own inventory (container 0, slot −1) with the state ID −1, which no server expects | One `container_set_content` with all 46 slots and the cursor |
| 1.12 to 1.16.5 | The same click with the action number −1, claiming that it picked up 99 stone. The server computes an empty result and disagrees | A rejection (`container_ack`), the whole inventory and the cursor. The backend then ignores every click of this player until the rejection is acknowledged |
| 1.17 | The same click, claiming 99 stone for every one of the 46 slots and the cursor | One `container_set_slot` for each slot, and the cursor |

The answer travels to the client like any backend packet, through the
translator when there is one. The client applies it, and the inventory rows
are right again.

- The click between the slots does nothing on any server, whatever the
  player holds. A click outside the window (slot −999) would drop what the
  player holds, so Rift never sends that.
- Rift writes the request in the backend's protocol, past the translator,
  which would repair the state ID that is wrong on purpose.
- **A rejection is Rift's to acknowledge.** Up to 1.16.5 Rift looks for the
  rejection of its own request (container 0, action −1, not accepted) before
  the translator, takes it out of the stream and sends the backend the
  acknowledgement at once. Until then the backend ignores clicks, the
  player's real ones included. This works the same for every client and
  translator. Rift counts the requests whose rejection may still come and
  takes out one rejection for each, so a request sent after an earlier one
  counted as lost has its rejection caught too, and a rejection that is not
  Rift's passes. It stops looking 30 seconds after its last request, so a
  backend that never rejects costs nothing per packet.
- **One at a time.** A request is on its way until a `container_set_content`
  for the player's inventory arrives, or for 5 seconds, after which it counts
  as lost. Two requests are at least 250 ms apart. A click that needs a
  request meanwhile makes one wait, and the waiting request always goes out:
  when the answer arrives and the interval is over, at the end of the
  interval, or when the request on its way counts as lost. Ten shift-clicks
  in half a second cost the backend two or three answers.
- **A 1.17 backend** answers slot by slot, and clients up to 1.21.1 apply
  single-slot updates of the main inventory only to the hotbar while another
  container is open. Rift asks a 1.17 backend once, when the menu closes.
- **Single-slot updates while a menu is open.** For the same reason, when a
  backend sends a `container_set_slot` for the player's inventory while a
  menu is open on a client up to 1.21.1 (a plugin gave an item), Rift asks
  for the inventory once more when the menu closes.
- No request goes out while the backend is not in play, and none when the
  menu ends with a switch or a disconnect: what the old backend had is gone
  with it.
- Rift keeps no copy of the player's inventory and parses no item for this.
  The repair takes one round trip to the backend; for that moment the player
  sees their own item where they dragged it.

## The anvil

- The client lets the player type only while the left slot holds an item. It
  starts the text field with that item's name and fills it again whenever
  that item changes. Rift names the left button after the text it puts in
  the field and sends that slot only when it has to: when it opens the menu,
  for a new title, for a patch with `text`, when the left button changed,
  and after a click. Each time it names the item after what the player typed
  so far, so the field keeps it, with the text cursor at the end.
- On 1.12 the left item carries the field's text exactly, without the color
  code that keeps other names upright, because that client copies the `§`
  string into its field and sends it back.
- The client sends the text when it changes and when the left slot is set:
  `rename_item` from 1.13, the `MC|ItemName` plugin message on 1.12, which
  sends it on every key. Clients from 1.13 to 1.19.4 send nothing for an
  empty text.
- Each rename makes the client compute a result (the left item, renamed) and
  a cost of at least 1 level by itself. Rift remembers a new text, sends the
  result slot and a cost of 0 again, and reports a `text` event if the menu
  asked for it, at most every 200 ms; a text that has to wait is reported
  when the interval ends. A cost of 0 hides the "Enchantment cost" line and
  stops the client from taking the result on its own.
- A rename longer than the client allows (35 UTF-16 units before 1.17, 50
  from 1.17), after the characters the game filters out of names, is ignored
  as the game ignores it. A text from the plugin is cut to the client's
  limit.
- The result slot is never empty. From 1.14 the client draws a red cross
  over the arrow when it is.
- A click on the result slot is always sent. Only a click that races a
  keystroke finds a cost above 0. The client then takes the cost off the
  shown levels and empties the input slots on its own, so after every click
  Rift sends the three slots and the cost again, and after a click on the
  result also the last `set_experience` it saw from the backend. That packet
  has three fixed fields, so remembering it costs nothing.
- A rename names no container. It is Rift's while Rift's anvil is open, and
  afterwards while Rift's anvil was the last screen and the backend opened no
  container since; a late one is dropped.

## What ends a menu

Rift ends its menu, with the [reason](menus.md#how-long-a-menu-lives) for the
plugin, when it sees:

| Rift sees | Rift does | Reason |
| --- | --- | --- |
| `container_close` from the client for Rift's ID | | `player` |
| A button with `close`, or `close_menu` | Sends `container_close` | `plugin` |
| Another menu of another `id` or owner, or a dialog of Rift's | A dialog: sends `container_close` first | `replaced` |
| `open_screen` or `mount_screen_open` from the backend, or its `container_close`, which closes whatever the client has open | | `server` |
| `open_sign_editor`, `open_book` or a dialog from the backend, which would leave the menu open beneath them | Sends `container_close` first | `server` |
| A respawn, or a new login from the backend | | `server` |
| The start of a server switch | Sends `container_close` | `switch` |
| A reconfiguration by the backend, which closes every screen | | `switch` |
| The end of the session | | `disconnect` |

A menu that never reached the player still ends once: with `switch` when the
action met the player between servers, and with `server` when its stacks
could not be built for the client, which Rift logs as a warning.

## Items and the stack cache

A plugin names an item once. Each client needs it in its own layout: numeric
IDs with a data value on 1.12, flattened IDs from 1.13, NBT tags until
1.20.4, data components from 1.20.5, with names and lore spelled differently
along the way.

`rift-translation` converts exactly this for backend items, one version step
at a time, with stand-ins for items a client lacks. Rift builds each button
as a stack of the newest protocol (26.3, data components) and runs it down
the same chain to the client's version
(`rift_translation::codec::item::newest`). There is no second item encoder.
What the conversion alone does not keep is fixed per client:

- Names and lore are upright: `italic = false` in the newest stack, and on
  clients that keep them as `§` strings a color code in front (`§r` for names
  before 1.13, `§5` for lore before 1.14).
- `details = false` gives the stack empty attribute modifiers and hides the
  lines the game adds; before 1.20.5 that is every `HideFlags` bit.
- `tooltip = false` cannot hide the tooltip before 1.20.5: the stack gets an
  empty name and no lore instead.
- `glint = true` on 1.12 is an enchantment list with one entry and its line
  hidden. Counts above 64 become 64 before 1.20.5.
- A head carries its texture property and an ID made from it, because
  clients before 1.20.2 drop a profile without a name or ID.
  `head = { player = ... }` uses the `textures` property of a player online
  on the calling peer or on the player's peer.

A converted stack depends only on the newest-protocol stack and the client's
protocol, so one cache shared by every session keeps the bytes by exactly
that, up to 4096 stacks before it starts over. A menu shown to 200 players
on the same version is encoded once. Texts are resolved (markup, legacy
codes, messages in the player's language) before the stack is built, so a
different language gives a different stack and never another player's bytes.

## Menus in a cluster

A menu for a player on another peer is parsed on the calling peer, with its
texts in the player's language, and sent to the player's peer as a player
action, as for dialogs. The receiving peer checks the menu again as it
reads it: each item name in its own build, the number of slots for the type,
the anvil's text and every limit of a definition. A peer of another build
cannot make it index past a menu's slots; it refuses such a menu or patch,
logs why, and the calling peer logs the refusal too. The owner is recorded as the calling
peer and plugin, so the session's events go back over the `player` stream to
the plugin that showed the menu. An event counts toward the player's 32
undelivered events until it was handed to that peer.

## What stays true

- The backend's inventory, cursor and containers are never changed by a menu.
- A client packet for a Rift container never reaches a backend, even after
  the menu closed.
- Rift never trusts a stack the client sends. It reads only the slot number,
  the button, the mode, the state ID and the changed slot numbers from a
  click, and the string from a rename.
- A malformed click or rename closes the session, as for dialogs and as
  vanilla does.
- A session that never shows a menu keeps only the backend's open container
  ID, the IDs Rift used, whether a request is on its way and the backend's
  last `set_experience`.

## Why it works this way

- **Ask the backend instead of keeping a copy of the inventory.** The client
  changes its inventory in many ways the server confirms without a packet
  (accepted clicks in backend containers, placed blocks, used items), so a
  copy in Rift drifts unless Rift simulates the game's inventory rules, and a
  drifted copy shows the player items they do not have. Every session would
  also pay for parsing its inventory packets. The request is exact, needs
  nothing on the backend, and [26 servers](#servers-by-experiment) answered
  it. Its costs are one round trip and a harmless click that backend plugins
  can see.
- **Slots from 0 in every language.** The same number in Lua, Rust, Go, C,
  JSON, events and the protocol, and in every interface library. Lua counts
  from 1, so an out-of-range slot is an error that names the range.
- **The four mouse clicks by default.** A player who presses Q or a number
  over a "buy" button buys nothing; the other kinds are one field away.
- **Stale clicks are dropped.** A click on an action the player had not seen
  is not reported, so no plugin has to guard against a double click on
  "next page". Only the action counts, so a slot that only looks different
  keeps its clicks.
- **Contract 0.3.0.** The `menu` hook is a function of `handler`, like every
  other hook, so one interface holds every hook in the WIT and in each SDK. A
  new hook is a breaking change, so the contract moved to 0.3.0, and Rift
  refuses 0.2 plugins with a pointer to
  [moving from 0.2](plugins.md#moving-from-02) instead of hosting two tracks.
  `kind`, `click` and `reason` are strings, so a later 0.3 release can add a
  value without another breaking version.
- **1.12 and newer.** That is the range `rift-translation` has item and
  packet tables for. Older clients would need hand-written packet IDs and an
  item table for seven more protocols; `menu` returns false for them, so
  adding them later changes no plugin.

## What was checked

### Servers, by experiment

[`tests/menus_probe.py`](../tests/menus_probe.py) joins a real server as a
scripted client, without Rift, and sends the
[inventory request](#the-inventory-request). It ran against 26 servers:

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

### Rift end to end, with a scripted client

[`tests/menus_minecraft.py`](../tests/menus_minecraft.py) starts real
servers and the Rift binary with a generated `rift.lua`, and joins through
Rift with a scripted client in the client's own protocol. It checks opening
(window type, container ID, title, every slot) for a chest, a hopper and a
dispenser; real clicks in each client's packet layout, which of them are
reported and with which fields; the undo packets of the client's version;
that clicks on the inventory rows make the backend send the inventory again
without the backend's own view changing; that nothing reaches the backend,
late clicks included; redraws, `update_menu`, the paged list, stale clicks
and a burst of 20 clicks a second on a button that redraws its menu; every
ending, each with exactly one `close` event; and that the client stays
connected and Rift and the server log no error. It passed on 24
server/client pairs: vanilla 1.12.2 to 26.3, Paper, Spigot and Pumpkin
backends, and translated pairs in both directions (26.3 backends with 1.12.2,
1.16.5 and 1.20.4 clients; 1.12.2, 1.16.5 and 1.17 backends with a 26.3
client).

### Real clients

[`tests/menus_real_client.py`](../tests/menus_real_client.py) puts Rift in
front of a vanilla server and joins the unmodified, headless client of each
version ([`tests/real_client.py`](../tests/real_client.py)). A persistent Lua
plugin shows, step by step, a 6-row chest with every button field, a redraw,
a replacing menu, a hopper, a dispenser, `close_menu`, a chest left open,
anvils (default, empty text, custom buttons, `typing`) with `update_menu`,
redraws of a chest with new slots, a new title and new rows, `update_menu` of
a chest, and an anvil left open while the client leaves. A relay between
client and Rift decodes every packet with an independent schema.

Clients of 18 versions accepted every step: 1.12.2, 1.13.2, 1.14.4, 1.15.2,
1.16.5, 1.17.1, 1.18.2, 1.19.4, 1.20.1, 1.20.4, 1.20.6, 1.21.1, 1.21.3,
1.21.4, 1.21.5, 1.21.8, 1.21.11 and 26.3. Each sent the rename a real client
sends when an anvil opens, which confirmed that 1.12 and 1.20 and newer send
an empty text and 1.13 to 1.19.4 do not.

### Unit and session tests

`src/menu/tests.rs` and `src/menu/stack/tests.rs` check parsing, every error
message, patches, diffs and the stacks for every protocol;
`src/protocol/menu/tests.rs` the packets; `src/session/menu_tests.rs` whole
sessions with a scripted client and backend, translated backends included;
and the plugin conformance suite shows, patches and closes menus from Lua and
WebAssembly plugins.

### Not checked

- Which event Paper and Spigot fire for the inventory request. It would need
  a plugin on the backend.
- Rendering. The headless clients draw nothing, so flicker, cursor position
  and how a button looks were not seen.
- The inventory request in game modes other than survival.
