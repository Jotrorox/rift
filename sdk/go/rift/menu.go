package rift

import (
	"encoding/json"
	"strconv"
	"strings"

	"github.com/Jotrorox/rift/sdk/go/gen/rift/plugin/types"
)

// Inventory menus: chest, hopper, dispenser and anvil screens whose slots hold
// items that act as buttons (Minecraft 1.12 and newer). See docs/menus.md for
// the format and the rules.
//
// Menu, Button and MenuPatch are plain structs; ShowMenu and UpdateMenu write
// the JSON the host parses, with the same builders as the text types and
// without encoding/json, which would add hundreds of kilobytes to a TinyGo
// build. Fields that are left out stay out of the JSON, so the host's defaults
// apply. A field whose zero value is a real choice is a pointer
// (Button.Glint, Button.Details, Button.Tooltip, MenuPatch.Text); use Ptr to
// set one. Nothing is checked here: Rift validates the menu and fails the
// call with a reason, as for a menu written as JSON text.

// MenuType is the kind of screen a menu opens.
type MenuType string

const (
	// MenuChest is a chest: 9 slots per row, 1 to 6 rows. It is the default.
	MenuChest MenuType = "chest"
	// MenuHopper is a hopper: 5 slots in one row.
	MenuHopper MenuType = "hopper"
	// MenuDispenser is a dispenser: 3 rows of 3 slots.
	MenuDispenser MenuType = "dispenser"
	// MenuAnvil is an anvil, a one-line text prompt: slot 0 is the left
	// input, 1 the right input and 2 the result.
	MenuAnvil MenuType = "anvil"
)

// Menu is a menu definition. ID is the only required field. Slots are
// numbered from 0, left to right and top to bottom.
type Menu struct {
	// ID names the menu in every event: 1 to 64 characters of a-z, 0-9, _,
	// -, . and /. Showing a menu with the ID of the one the player has open
	// redraws it in place.
	ID string
	// Type is the screen; empty means a chest.
	Type MenuType
	// Rows is the number of rows of a chest, 1 to 6; 0 means as many as the
	// layout has lines, else 3.
	Rows int
	// Title is the menu's title; the zero Text is none.
	Title Text
	// Layout is one line of characters per row, one character per slot. A
	// space is an empty slot; every other character needs an entry in Keys.
	Layout []string
	// Keys is the button each layout character stands for.
	Keys map[rune]Button
	// Slots places buttons by number after the layout, replacing what it put
	// there. Make entries with At and Empty.
	Slots []Slot
	// StateJSON is any JSON value of at most 16 KiB that Rift keeps with the
	// menu and hands back in every event; "" for none. JSONString writes a
	// string value.
	StateJSON string
	// Text is what the text field of an anvil starts with.
	Text string
	// Typing makes an anvil report a text event while the player types.
	Typing bool
}

// MenuPatch changes part of an open menu with UpdateMenu. Fields left out
// change nothing.
type MenuPatch struct {
	// Slots replace those slots. Make entries with At and Empty.
	Slots []Slot
	// Keys are new buttons for layout characters, applied to every slot the
	// layout gave that character.
	Keys map[rune]Button
	// Title is a new title; the zero Text leaves it.
	Title Text
	// StateJSON is a new state as JSON; "" leaves it.
	StateJSON string
	// Text replaces what the text field of an anvil holds. A pointer to ""
	// clears it.
	Text *string
}

// Slot is a button placed by slot number, or an emptied slot. Make one with
// At or Empty.
type Slot struct {
	Slots  []int
	Button Button
	Empty  bool
}

// At places button in the given slots.
func At(button Button, slots ...int) Slot { return Slot{Slots: slots, Button: button} }

// Empty empties the given slots.
func Empty(slots ...int) Slot { return Slot{Slots: slots, Empty: true} }

// Button is an item with an optional action. Item is the only required field.
type Button struct {
	// Item is an item name of the newest supported version, such as
	// "diamond_sword"; "minecraft:" is optional.
	Item string
	// Count is 1 to 99; 0 means 1. Clients before 1.20.5 get at most 64.
	Count int
	// Name is the item's name; the zero Text keeps the item's own.
	Name Text
	// Lore is up to 32 lines, one text each.
	Lore []Text
	// Glint adds the enchantment glint when true and removes an item's own
	// on 1.20.5 and newer when false; nil keeps the item's own.
	Glint *bool
	// Model is custom model data for resource packs.
	Model *Model
	// ItemModel is a model ID for 1.21.2 and newer.
	ItemModel string
	// Head is the skin of a player_head.
	Head *HeadSkin
	// Color is "#RRGGBB" for leather armor, potions and other dyeable items.
	Color string
	// Details shows the lines the game adds on its own (attack damage,
	// enchantments, potion effects) when true; nil hides them.
	Details *bool
	// Tooltip hides the tooltip altogether on 1.20.5 and newer when false
	// (older clients get an empty name); nil shows it.
	Tooltip *bool
	// ID is the action name reported when the button is clicked: the same
	// characters as a menu ID. Without one the button is decoration.
	ID string
	// PayloadJSON is any JSON value of at most 4 KiB, handed back with the
	// click; "" for none. Rift keeps it; the client never sees it.
	PayloadJSON string
	// Clicks are the click kinds that report. Empty means the four mouse
	// clicks. Needs an ID.
	Clicks []Click
	// AllClicks reports every click kind, in place of Clicks.
	AllClicks bool
	// Close closes the menu when the button is clicked with one of the four
	// mouse clicks, without waiting for the plugin. It needs no ID.
	Close bool
}

// Model is the custom model data of a button: Number alone is the single
// number every version reads; otherwise the lists are the 1.21.4 format.
type Model struct {
	Number  *float64
	Floats  []float64
	Flags   []bool
	Strings []string
	Colors  []uint32 // RGB numbers such as 0xff8800
}

// HeadSkin is the skin of a player_head button: Player alone is an online
// player's name or UUID; Texture (with an optional Signature) is a textures
// property value as head databases publish it.
type HeadSkin struct {
	Player    string
	Texture   string
	Signature string
}

// JSONString returns s as a JSON string, for StateJSON and PayloadJSON:
//
//	rift.Button{Item: "grass_block", ID: "join", PayloadJSON: rift.JSONString("survival")}
func JSONString(s string) string { return jsonQuote(s) }

// JSON returns the menu definition as the host parses it.
func (m Menu) JSON() string {
	fields := []jsonField{{"id", jsonQuote(m.ID)}}
	menuType := m.Type
	if menuType == "" {
		menuType = MenuChest
	}
	fields = append(fields, jsonField{"type", jsonQuote(string(menuType))})
	if m.Rows != 0 {
		fields = append(fields, jsonField{"rows", strconv.Itoa(m.Rows)})
	}
	if !m.Title.IsZero() {
		fields = append(fields, jsonField{"title", m.Title.JSON()})
	}
	if len(m.Layout) > 0 {
		lines := make([]string, len(m.Layout))
		for i, line := range m.Layout {
			lines[i] = jsonQuote(line)
		}
		fields = append(fields, jsonField{"layout", "[" + strings.Join(lines, ",") + "]"})
	}
	fields = appendKeys(fields, m.Keys)
	fields = appendSlots(fields, m.Slots)
	if m.StateJSON != "" {
		fields = append(fields, jsonField{"state", m.StateJSON})
	}
	if m.Text != "" {
		fields = append(fields, jsonField{"text", jsonQuote(m.Text)})
	}
	if m.Typing {
		fields = append(fields, jsonField{"typing", "true"})
	}
	return objectJSON(fields)
}

// JSON returns the patch as the host parses it.
func (p MenuPatch) JSON() string {
	var fields []jsonField
	fields = appendSlots(fields, p.Slots)
	fields = appendKeys(fields, p.Keys)
	if !p.Title.IsZero() {
		fields = append(fields, jsonField{"title", p.Title.JSON()})
	}
	if p.StateJSON != "" {
		fields = append(fields, jsonField{"state", p.StateJSON})
	}
	if p.Text != nil {
		fields = append(fields, jsonField{"text", jsonQuote(*p.Text)})
	}
	return objectJSON(fields)
}

// JSON returns the button as the host parses it.
func (b Button) JSON() string { return objectJSON(b.fields(nil)) }

func objectJSON(fields []jsonField) string {
	var b strings.Builder
	writeObject(&b, fields)
	return b.String()
}

func appendKeys(fields []jsonField, keys map[rune]Button) []jsonField {
	if len(keys) == 0 {
		return fields
	}
	// Sorted, so the same menu always gives the same JSON.
	characters := make([]rune, 0, len(keys))
	for character := range keys {
		characters = append(characters, character)
	}
	for i := 1; i < len(characters); i++ {
		for j := i; j > 0 && characters[j] < characters[j-1]; j-- {
			characters[j], characters[j-1] = characters[j-1], characters[j]
		}
	}
	entries := make([]jsonField, len(characters))
	for i, character := range characters {
		entries[i] = jsonField{string(character), keys[character].JSON()}
	}
	return append(fields, jsonField{"keys", objectJSON(entries)})
}

func appendSlots(fields []jsonField, slots []Slot) []jsonField {
	if len(slots) == 0 {
		return fields
	}
	var entries []string
	for _, slot := range slots {
		if slot.Empty {
			for _, n := range slot.Slots {
				entries = append(entries, `{"slot":`+strconv.Itoa(n)+`,"empty":true}`)
			}
			continue
		}
		var place jsonField
		if len(slot.Slots) == 1 {
			place = jsonField{"slot", strconv.Itoa(slot.Slots[0])}
		} else {
			numbers := make([]string, len(slot.Slots))
			for i, n := range slot.Slots {
				numbers[i] = strconv.Itoa(n)
			}
			place = jsonField{"slots", "[" + strings.Join(numbers, ",") + "]"}
		}
		entries = append(entries, objectJSON(slot.Button.fields([]jsonField{place})))
	}
	return append(fields, jsonField{"slots", "[" + strings.Join(entries, ",") + "]"})
}

// fields appends the button's fields to leading.
func (b Button) fields(leading []jsonField) []jsonField {
	fields := append(leading, jsonField{"item", jsonQuote(b.Item)})
	if b.Count != 0 {
		fields = append(fields, jsonField{"count", strconv.Itoa(b.Count)})
	}
	if !b.Name.IsZero() {
		fields = append(fields, jsonField{"name", b.Name.JSON()})
	}
	if len(b.Lore) > 0 {
		lines := make([]string, len(b.Lore))
		for i, line := range b.Lore {
			lines[i] = line.JSON()
		}
		fields = append(fields, jsonField{"lore", "[" + strings.Join(lines, ",") + "]"})
	}
	if b.Glint != nil {
		fields = append(fields, jsonField{"glint", strconv.FormatBool(*b.Glint)})
	}
	if b.Model != nil {
		fields = append(fields, jsonField{"model", b.Model.json()})
	}
	if b.ItemModel != "" {
		fields = append(fields, jsonField{"item_model", jsonQuote(b.ItemModel)})
	}
	if b.Head != nil {
		fields = append(fields, jsonField{"head", b.Head.json()})
	}
	if b.Color != "" {
		fields = append(fields, jsonField{"color", jsonQuote(b.Color)})
	}
	if b.Details != nil {
		fields = append(fields, jsonField{"details", strconv.FormatBool(*b.Details)})
	}
	if b.Tooltip != nil {
		fields = append(fields, jsonField{"tooltip", strconv.FormatBool(*b.Tooltip)})
	}
	if b.ID != "" {
		fields = append(fields, jsonField{"id", jsonQuote(b.ID)})
	}
	if b.PayloadJSON != "" {
		fields = append(fields, jsonField{"payload", b.PayloadJSON})
	}
	if b.AllClicks {
		fields = append(fields, jsonField{"clicks", `"all"`})
	} else if len(b.Clicks) > 0 {
		names := make([]string, 0, len(b.Clicks))
		for _, click := range b.Clicks {
			if click != ClickOther {
				names = append(names, jsonQuote(click.String()))
			}
		}
		fields = append(fields, jsonField{"clicks", "[" + strings.Join(names, ",") + "]"})
	}
	if b.Close {
		fields = append(fields, jsonField{"close", "true"})
	}
	return fields
}

func (m Model) json() string {
	if m.Number != nil {
		return jsonNumber(*m.Number)
	}
	floats := make([]string, len(m.Floats))
	for i, f := range m.Floats {
		floats[i] = jsonNumber(f)
	}
	flags := make([]string, len(m.Flags))
	for i, f := range m.Flags {
		flags[i] = strconv.FormatBool(f)
	}
	strs := make([]string, len(m.Strings))
	for i, s := range m.Strings {
		strs[i] = jsonQuote(s)
	}
	colors := make([]string, len(m.Colors))
	for i, c := range m.Colors {
		colors[i] = strconv.FormatUint(uint64(c), 10)
	}
	return objectJSON([]jsonField{
		{"floats", "[" + strings.Join(floats, ",") + "]"},
		{"flags", "[" + strings.Join(flags, ",") + "]"},
		{"strings", "[" + strings.Join(strs, ",") + "]"},
		{"colors", "[" + strings.Join(colors, ",") + "]"},
	})
}

func jsonNumber(f float64) string { return strconv.FormatFloat(f, 'g', -1, 64) }

func (h HeadSkin) json() string {
	var fields []jsonField
	if h.Player != "" {
		fields = append(fields, jsonField{"player", jsonQuote(h.Player)})
	}
	if h.Texture != "" {
		fields = append(fields, jsonField{"texture", jsonQuote(h.Texture)})
	}
	if h.Signature != "" {
		fields = append(fields, jsonField{"signature", jsonQuote(h.Signature)})
	}
	return objectJSON(fields)
}

// Click is how a player clicked a button. Its String is the name the host
// uses, such as "shift_left".
type Click uint8

const (
	// ClickOther is a kind this SDK does not know, from a newer Rift, and the
	// value of MenuEvent.Click for an event that is not a click.
	ClickOther Click = iota
	// ClickLeft is a left click.
	ClickLeft
	// ClickRight is a right click.
	ClickRight
	// ClickShiftLeft is a left click while holding Shift.
	ClickShiftLeft
	// ClickShiftRight is a right click while holding Shift.
	ClickShiftRight
	// ClickMiddle is the mouse wheel. Clients from 1.18 send it only in
	// creative mode.
	ClickMiddle
	// ClickDrop is Q over the slot.
	ClickDrop
	// ClickDropAll is Ctrl+Q over the slot.
	ClickDropAll
	// ClickNumberKey is 1 to 9 over the slot; MenuEvent.Key says which.
	ClickNumberKey
	// ClickOffhand is F over the slot (1.16 and newer clients).
	ClickOffhand
)

var clickNames = [...]string{
	ClickOther: "", ClickLeft: "left", ClickRight: "right", ClickShiftLeft: "shift_left",
	ClickShiftRight: "shift_right", ClickMiddle: "middle", ClickDrop: "drop",
	ClickDropAll: "drop_all", ClickNumberKey: "number_key", ClickOffhand: "offhand",
}

// String returns the name the host uses, "" for ClickOther.
func (c Click) String() string {
	if int(c) < len(clickNames) {
		return clickNames[c]
	}
	return ""
}

func parseClick(name string) Click {
	for c := ClickLeft; int(c) < len(clickNames); c++ {
		if clickNames[c] == name {
			return c
		}
	}
	return ClickOther
}

// MenuKind says what happened in a menu.
type MenuKind uint8

const (
	// MenuKindOther is a kind this SDK does not know, from a newer Rift:
	// ignore the event.
	MenuKindOther MenuKind = iota
	// MenuKindClick is a click on a button with an ID.
	MenuKindClick
	// MenuKindText is typing in an anvil menu shown with Typing.
	MenuKindText
	// MenuKindClose is the end of the menu; every menu ends with exactly one.
	MenuKindClose
)

// CloseReason is why a menu ended.
type CloseReason uint8

const (
	// CloseReasonOther is a reason this SDK does not know, from a newer
	// Rift, and the value of MenuEvent.Reason for an event that is not a
	// close.
	CloseReasonOther CloseReason = iota
	// CloseReasonPlayer is Escape or the inventory key.
	CloseReasonPlayer
	// CloseReasonPlugin is a button with Close, or CloseMenu.
	CloseReasonPlugin
	// CloseReasonReplaced is another menu (a different ID, or another
	// plugin's) or a dialog taking the screen.
	CloseReasonReplaced
	// CloseReasonServer is a backend opening a screen of its own, closing the
	// player's screens, or the player dying or changing worlds.
	CloseReasonServer
	// CloseReasonSwitch is the player switching servers.
	CloseReasonSwitch
	// CloseReasonDisconnect is the player leaving.
	CloseReasonDisconnect
)

var closeReasonNames = [...]string{
	CloseReasonOther: "", CloseReasonPlayer: "player", CloseReasonPlugin: "plugin",
	CloseReasonReplaced: "replaced", CloseReasonServer: "server",
	CloseReasonSwitch: "switch", CloseReasonDisconnect: "disconnect",
}

// String returns the name the host uses, "" for CloseReasonOther.
func (r CloseReason) String() string {
	if int(r) < len(closeReasonNames) {
		return closeReasonNames[r]
	}
	return ""
}

func parseCloseReason(name string) CloseReason {
	for r := CloseReasonPlayer; int(r) < len(closeReasonNames); r++ {
		if closeReasonNames[r] == name {
			return r
		}
	}
	return CloseReasonOther
}

// MenuEvent is something a player did in a menu this plugin showed. The
// handler set with OnMenu gets it; Kind says which fields are set.
type MenuEvent struct {
	Ctx  Context // the player; Server is their current backend
	Menu string  // the menu's ID
	Kind MenuKind
	// KindName, ClickName and ReasonName are the host's spellings, for a
	// value the typed fields do not know (they are then the Other cases).
	KindName   string
	ClickName  string
	ReasonName string
	// Click events: the button's ID, the slot number from 0, the click kind,
	// and with ClickNumberKey the key, 1 to 9.
	Action string
	Slot   int
	Click  Click
	Key    int
	// Text is what the text field of an anvil holds, in every event of one;
	// nil for other menus. It comes from the client: check it like chat.
	Text *string
	// Close events: why the menu ended.
	Reason CloseReason
	// PayloadJSON is the clicked button's payload as the plugin wrote it, or
	// nil. StateJSON is the menu's state as the plugin last set it, or nil.
	// PayloadString and StateString read a string; DecodePayload and
	// DecodeState anything else.
	PayloadJSON *string
	StateJSON   *string
}

// PayloadString reads the payload when it is a JSON string, the usual case
// for a name or an ID, with a small parser that adds only a few kilobytes to
// the plugin. ok is false for no payload or another type.
func (e MenuEvent) PayloadString() (value string, ok bool) { return jsonStringValue(e.PayloadJSON) }

// StateString reads the state when it is a JSON string; see PayloadString.
func (e MenuEvent) StateString() (value string, ok bool) { return jsonStringValue(e.StateJSON) }

// DecodePayload decodes the payload into v (usually a pointer to a struct
// with json tags) with encoding/json, which adds roughly 850 KB to a TinyGo
// build. found is false, and v untouched, when the button had no payload.
func (e MenuEvent) DecodePayload(v any) (found bool, err error) { return decodeJSON(e.PayloadJSON, v) }

// DecodeState decodes the state into v; see DecodePayload.
func (e MenuEvent) DecodeState(v any) (found bool, err error) { return decodeJSON(e.StateJSON, v) }

func decodeJSON(j *string, v any) (bool, error) {
	if j == nil {
		return false, nil
	}
	if err := json.Unmarshal([]byte(*j), v); err != nil {
		return true, err
	}
	return true, nil
}

func jsonStringValue(j *string) (string, bool) {
	if j == nil {
		return "", false
	}
	p := &jsonScanner{s: *j}
	p.ws()
	if !p.peek('"') {
		return "", false
	}
	value, ok := p.str()
	if !ok {
		return "", false
	}
	p.ws()
	if p.i != len(p.s) {
		return "", false
	}
	return value, true
}

func liftMenuEvent(e types.MenuEvent) MenuEvent {
	event := MenuEvent{
		Ctx:         liftContext(e.Ctx),
		Menu:        e.Menu,
		KindName:    e.Kind,
		Text:        liftOpt(e.Text),
		PayloadJSON: liftOpt(e.PayloadJSON),
		StateJSON:   liftOpt(e.StateJSON),
	}
	switch e.Kind {
	case "click":
		event.Kind = MenuKindClick
	case "text":
		event.Kind = MenuKindText
	case "close":
		event.Kind = MenuKindClose
	}
	if action := e.Action.Some(); action != nil {
		event.Action = *action
	}
	if slot := e.Slot.Some(); slot != nil {
		event.Slot = int(*slot)
	}
	if click := e.Click.Some(); click != nil {
		event.ClickName = *click
		event.Click = parseClick(*click)
	}
	if key := e.Key.Some(); key != nil {
		event.Key = int(*key)
	}
	if reason := e.Reason.Some(); reason != nil {
		event.ReasonName = *reason
		event.Reason = parseCloseReason(*reason)
	}
	return event
}
