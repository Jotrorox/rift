package rift

import (
	"strconv"
	"strings"
	"unicode/utf8"

	"go.bytecodealliance.org/cm"

	"github.com/Jotrorox/rift/sdk/go/gen/rift/plugin/types"
)

// Text is text shown to a player: plain text, shown exactly as written, or a
// JSON text component. Make one with Plain, JSON or a text builder's Text
// method. The zero Text is no text: optional fields and parameters (a
// subtitle, a tooltip, a tab list footer) take it for "none".
type Text struct {
	json  bool
	value string
}

// Plain is text of 1..=1024 bytes shown exactly as written: it is never read
// as markup, so player input cannot add tags or click events.
func Plain(text string) Text { return Text{value: text} }

// JSON is a JSON text component of at most 32 KiB written by hand, such as
// `{"text":"Hi","color":"gold"}` or a mini or legacy authoring object (see
// docs/text.md). Rift rejects it at the call when it is not a valid text
// component. The builders Mini, Legacy and Component write one for you.
func JSON(component string) Text { return Text{json: true, value: component} }

// IsZero reports whether t is the zero Text, which stands for no text.
func (t Text) IsZero() bool { return t == Text{} }

// IsJSON reports whether t is a JSON text component rather than plain text.
func (t Text) IsJSON() bool { return t.json }

// JSON returns the text as JSON: the component itself, or plain text as a
// JSON string. It makes a Text nest inside the builders.
func (t Text) JSON() string {
	if t.json {
		return t.value
	}
	return jsonQuote(t.value)
}

// String returns the plain text or the JSON component as it was given.
func (t Text) String() string { return t.value }

func (t Text) lower() types.Text {
	if t.json {
		return types.TextJSON(t.value)
	}
	return types.TextPlain(t.value)
}

func (t Text) lowerOpt() cm.Option[types.Text] {
	if t.IsZero() {
		return cm.None[types.Text]()
	}
	return cm.Some(t.lower())
}

// Text builders write JSON text components without encoding/json, which would
// add hundreds of kilobytes to every plugin, and escape everything they
// write:
//
//   - Mini writes Rift markup as {"mini": "...", "args": {...}}; Rift turns
//     it into a component (see docs/text.md for the tags).
//   - Legacy writes &c codes as {"legacy": "...", "marker": "&"}.
//   - Msg names a message from the plugin's messages.toml as
//     {"message": "...", "args": {...}}; Rift renders it in the player's
//     language.
//   - Component builds a JSON text component field by field, with the 1.21.5
//     spellings click_event and hover_event (Rift rewrites them for older
//     clients).
//
// Each builder's Text method returns the Text that host calls, decisions and
// records take. Markup is only read from a Mini object, so player input
// passed through Arg or Plain cannot add tags or click events.

// TextJSON is a piece of text the builders nest: a builder or a Text.
type TextJSON interface {
	// JSON returns the value as JSON text.
	JSON() string
}

type jsonField struct{ key, value string }

// MiniText is markup with named arguments; build one with Mini.
type MiniText struct {
	markup string
	args   []jsonField
}

// Mini is Rift markup such as "<gold>Hi <player>!". Add arguments with Arg
// and ArgComponent.
func Mini(markup string) *MiniText { return &MiniText{markup: markup} }

// setField sets key in fields, replacing an earlier value.
func setField(fields []jsonField, key, value string) []jsonField {
	for i := range fields {
		if fields[i].key == key {
			fields[i].value = value
			return fields
		}
	}
	return append(fields, jsonField{key, value})
}

func (m *MiniText) set(name, value string) *MiniText {
	m.args = setField(m.args, name, value)
	return m
}

// styleJSON is the value of a style argument: {"style": markup}.
func styleJSON(markup string) string { return `{"style":` + jsonQuote(markup) + `}` }

// Arg makes <name> show text exactly as written: it is never parsed, so a
// player name cannot add tags.
func (m *MiniText) Arg(name, text string) *MiniText { return m.set(name, jsonQuote(text)) }

// ArgComponent makes <name> show a component with its own formatting, such as
// a Component, a Legacy object or a JSON Text.
func (m *MiniText) ArgComponent(name string, component TextJSON) *MiniText {
	return m.set(name, component.JSON())
}

// ArgStyle makes <name> a style: markup made only of formatting tags, such
// as "<#55ffff><bold>". It wraps whatever the template puts inside
// <name>...</name>:
//
//	rift.Mini("<team>[<tag>]</team>").ArgStyle("team", "<#55ffff><bold>").Arg("tag", "RED")
func (m *MiniText) ArgStyle(name, markup string) *MiniText { return m.set(name, styleJSON(markup)) }

// JSON returns the authoring object.
func (m *MiniText) JSON() string {
	var b strings.Builder
	b.WriteString(`{"mini":`)
	b.WriteString(jsonQuote(m.markup))
	if len(m.args) > 0 {
		b.WriteString(`,"args":`)
		writeObject(&b, m.args)
	}
	b.WriteByte('}')
	return b.String()
}

// Text returns the builder's result as a JSON Text.
func (m *MiniText) Text() Text { return JSON(m.JSON()) }

// String returns the same as JSON.
func (m *MiniText) String() string { return m.JSON() }

// Escape returns text escaped so it can be spliced into markup: every \
// becomes \\ and every < becomes \<. Nothing else changes. An argument
// (Arg) is never parsed and needs no escaping.
func Escape(text string) string {
	if !strings.ContainsAny(text, `<\`) {
		return text
	}
	var b strings.Builder
	b.Grow(len(text) + 8)
	for i := 0; i < len(text); i++ {
		if c := text[i]; c == '<' || c == '\\' {
			b.WriteByte('\\')
		}
		b.WriteByte(text[i])
	}
	return b.String()
}

// MessageText is a message with named arguments; build one with Msg.
type MessageText struct {
	key  string
	args []jsonField
}

// Msg is a message from the plugin's messages.toml, by key. A key with ":"
// names another catalog, such as "party:invited" or "rift:queue.full". Rift
// renders it in the recipient's language and checks the key and the
// arguments at the call:
//
//	rift.SendMessage(target, rift.Msg("invited").Arg("from", ctx.Name).Text())
//
// (rift.Message is the decision that shows a player text.)
func Msg(key string) *MessageText { return &MessageText{key: key} }

// Arg makes name show text exactly as written: it is never parsed.
func (m *MessageText) Arg(name, text string) *MessageText {
	m.args = setField(m.args, name, jsonQuote(text))
	return m
}

// ArgComponent makes name show a component with its own formatting, such as
// a Component or a Mini object.
func (m *MessageText) ArgComponent(name string, component TextJSON) *MessageText {
	m.args = setField(m.args, name, component.JSON())
	return m
}

// ArgStyle makes name a style argument (see MiniText.ArgStyle); the catalog
// declares it under styles.
func (m *MessageText) ArgStyle(name, markup string) *MessageText {
	m.args = setField(m.args, name, styleJSON(markup))
	return m
}

// JSON returns the authoring object.
func (m *MessageText) JSON() string {
	var b strings.Builder
	b.WriteString(`{"message":`)
	b.WriteString(jsonQuote(m.key))
	if len(m.args) > 0 {
		b.WriteString(`,"args":`)
		writeObject(&b, m.args)
	}
	b.WriteByte('}')
	return b.String()
}

// Text returns the builder's result as a JSON Text.
func (m *MessageText) Text() Text { return JSON(m.JSON()) }

// String returns the same as JSON.
func (m *MessageText) String() string { return m.JSON() }

// LegacyText is text with legacy formatting codes; build one with Legacy.
type LegacyText struct {
	text   string
	marker rune
}

// Legacy is text with legacy formatting codes and the & marker, such as
// "&cRed &lbold"; &#rrggbb writes a hex colour. Use Marker for another
// marker.
func Legacy(text string) LegacyText { return LegacyText{text: text, marker: '&'} }

// Marker sets the character that starts a code, such as '§': one symbol, not
// a letter, digit or space.
func (l LegacyText) Marker(marker rune) LegacyText {
	l.marker = marker
	return l
}

// JSON returns the authoring object.
func (l LegacyText) JSON() string {
	return `{"legacy":` + jsonQuote(l.text) + `,"marker":` + jsonQuote(string(l.marker)) + `}`
}

// Text returns the builder's result as a JSON Text.
func (l LegacyText) Text() Text { return JSON(l.JSON()) }

// String returns the same as JSON.
func (l LegacyText) String() string { return l.JSON() }

// ComponentText is a JSON text component; build one with Component.
type ComponentText struct {
	fields []jsonField
	with   []string
	extra  []string
}

// Component is a JSON text component showing text. Chain the setters:
//
//	rift.Component("[Accept]").Color("green").Bold().RunCommand("/party accept Alex")
func Component(text string) *ComponentText {
	return &ComponentText{fields: []jsonField{{"text", jsonQuote(text)}}}
}

// Translate is a translation the client looks up by key, such as
// "multiplayer.player.joined". Add arguments with With and a fallback with
// Fallback:
//
//	rift.Translate("multiplayer.player.joined").With(rift.Component("Alex").Color("yellow")).Fallback("Alex joined")
func Translate(key string) *ComponentText {
	return &ComponentText{fields: []jsonField{{"translate", jsonQuote(key)}}}
}

// With appends arguments of a Translate component: Components, authoring
// objects or Plain text.
func (c *ComponentText) With(args ...TextJSON) *ComponentText {
	for _, arg := range args {
		c.with = append(c.with, arg.JSON())
	}
	return c
}

// Fallback sets the text a client shows when it has no translation for the
// key.
func (c *ComponentText) Fallback(text string) *ComponentText {
	return c.Field("fallback", jsonQuote(text))
}

// Keybind is the key the player bound to key, such as "key.jump".
func Keybind(key string) *ComponentText {
	return &ComponentText{fields: []jsonField{{"keybind", jsonQuote(key)}}}
}

// Sprite is a sprite from the blocks atlas, such as "item/diamond". Choose
// another atlas with Atlas. Clients before 1.21.9 do not show it.
func Sprite(sprite string) *ComponentText {
	return &ComponentText{fields: []jsonField{{"object", `"atlas"`}, {"sprite", jsonQuote(sprite)}}}
}

// Atlas sets the atlas of a Sprite, such as "minecraft:gui".
func (c *ComponentText) Atlas(atlas string) *ComponentText {
	return c.Field("atlas", jsonQuote(atlas))
}

// Head is the head of the player called name. Clients before 1.21.9 do not
// show it.
func Head(name string) *ComponentText {
	return &ComponentText{fields: []jsonField{{"object", `"player"`}, {"player", `{"name":` + jsonQuote(name) + `}`}}}
}

// Hat sets whether a Head shows the hat layer (it does unless set to false).
func (c *ComponentText) Hat(shown bool) *ComponentText {
	return c.Field("hat", strconv.FormatBool(shown))
}

// Field sets any other field to raw JSON, such as Field("insertion",
// `"text"`) or a field newer than this SDK.
func (c *ComponentText) Field(key, rawJSON string) *ComponentText {
	c.fields = setField(c.fields, key, rawJSON)
	return c
}

// Color sets a colour name such as "red" or "#ff8800".
func (c *ComponentText) Color(color string) *ComponentText {
	return c.Field("color", jsonQuote(color))
}

// Bold makes the text bold.
func (c *ComponentText) Bold() *ComponentText { return c.Field("bold", "true") }

// Italic makes the text italic.
func (c *ComponentText) Italic() *ComponentText { return c.Field("italic", "true") }

// Underlined underlines the text.
func (c *ComponentText) Underlined() *ComponentText { return c.Field("underlined", "true") }

// Strikethrough strikes the text through.
func (c *ComponentText) Strikethrough() *ComponentText { return c.Field("strikethrough", "true") }

// Obfuscated makes the text scramble.
func (c *ComponentText) Obfuscated() *ComponentText { return c.Field("obfuscated", "true") }

// Decoration turns a decoration ("bold", "italic", "underlined",
// "strikethrough" or "obfuscated") on or off. Off overrides what a parent
// component sets: Decoration("italic", false).
func (c *ComponentText) Decoration(name string, on bool) *ComponentText {
	return c.Field(name, strconv.FormatBool(on))
}

// Font sets the font, such as "minecraft:uniform".
func (c *ComponentText) Font(font string) *ComponentText { return c.Field("font", jsonQuote(font)) }

// Shadow sets a text shadow in color ("#rrggbb") with opacity from 0 to 1,
// as <shadow:#rrggbb:opacity> writes it. An opacity of 0 hides the shadow.
// A color that is not "#rrggbb" makes Rift reject the component at the call.
func (c *ComponentText) Shadow(color string, opacity float64) *ComponentText {
	argb, ok := shadowColor(color, opacity)
	if !ok {
		return c.Field("shadow_color", jsonQuote(color))
	}
	return c.Field("shadow_color", strconv.FormatInt(argb, 10))
}

// shadowColor is the ARGB integer vanilla stores for a shadow, with the same
// arithmetic as the host's <shadow> tag.
func shadowColor(color string, opacity float64) (int64, bool) {
	if len(color) != 7 || color[0] != '#' {
		return 0, false
	}
	var rgb uint32
	for i := 1; i < 7; i++ {
		d := color[i]
		switch {
		case d >= '0' && d <= '9':
			d -= '0'
		case d >= 'a' && d <= 'f':
			d -= 'a' - 10
		case d >= 'A' && d <= 'F':
			d -= 'A' - 10
		default:
			return 0, false
		}
		rgb = rgb<<4 | uint32(d)
	}
	switch {
	case !(opacity >= 0): // also NaN
		opacity = 0
	case opacity > 1:
		opacity = 1
	}
	alpha := uint32(opacity*255 + 0.5)
	// Vanilla stores ARGB as a signed int.
	return int64(int32(alpha<<24 | rgb)), true
}

func (c *ComponentText) click(action, key, value string) *ComponentText {
	return c.Field("click_event", `{"action":`+jsonQuote(action)+`,`+jsonQuote(key)+`:`+jsonQuote(value)+`}`)
}

// RunCommand makes a click run command (with its leading /) as the player.
func (c *ComponentText) RunCommand(command string) *ComponentText {
	return c.click("run_command", "command", command)
}

// SuggestCommand makes a click put command into the player's chat box.
func (c *ComponentText) SuggestCommand(command string) *ComponentText {
	return c.click("suggest_command", "command", command)
}

// OpenURL makes a click open url (http or https) after a confirmation.
func (c *ComponentText) OpenURL(url string) *ComponentText { return c.click("open_url", "url", url) }

// CopyToClipboard makes a click copy text to the clipboard.
func (c *ComponentText) CopyToClipboard(text string) *ComponentText {
	return c.click("copy_to_clipboard", "value", text)
}

// ChangePage makes a click turn a book to page, counting from 1.
func (c *ComponentText) ChangePage(page int) *ComponentText {
	return c.Field("click_event", `{"action":"change_page","page":`+strconv.Itoa(page)+`}`)
}

// HoverText shows text, as written, when the pointer rests on the component.
func (c *ComponentText) HoverText(text string) *ComponentText { return c.Hover(Plain(text)) }

// Hover shows a component, such as another Component or a Mini object, when
// the pointer rests on the component.
func (c *ComponentText) Hover(component TextJSON) *ComponentText {
	return c.Field("hover_event", `{"action":"show_text","value":`+component.JSON()+`}`)
}

// HoverItem shows count of the item id, such as "minecraft:diamond", when
// the pointer rests on the component.
func (c *ComponentText) HoverItem(id string, count int) *ComponentText {
	return c.Field("hover_event", `{"action":"show_item","id":`+jsonQuote(id)+`,"count":`+strconv.Itoa(count)+`}`)
}

// HoverEntity shows the entity of type kind, such as "minecraft:pig", with
// uuid in its hyphenated form, when the pointer rests on the component. name
// (a Component, an authoring object or Plain text) may be nil for none.
func (c *ComponentText) HoverEntity(kind, uuid string, name TextJSON) *ComponentText {
	event := `{"action":"show_entity","id":` + jsonQuote(kind) + `,"uuid":` + jsonQuote(uuid)
	if t, isText := name.(Text); name != nil && !(isText && t.IsZero()) {
		event += `,"name":` + name.JSON()
	}
	return c.Field("hover_event", event+`}`)
}

// Insertion sets text inserted into the chat box on shift-click.
func (c *ComponentText) Insertion(text string) *ComponentText {
	return c.Field("insertion", jsonQuote(text))
}

// Extra appends children: Components, authoring objects or Plain text. They
// inherit this component's style.
func (c *ComponentText) Extra(children ...TextJSON) *ComponentText {
	for _, child := range children {
		c.extra = append(c.extra, child.JSON())
	}
	return c
}

// JSON returns the component.
func (c *ComponentText) JSON() string {
	var b strings.Builder
	fields := c.fields[:len(c.fields):len(c.fields)]
	if len(c.with) > 0 {
		fields = append(fields, jsonField{"with", "[" + strings.Join(c.with, ",") + "]"})
	}
	if len(c.extra) > 0 {
		fields = append(fields, jsonField{"extra", "[" + strings.Join(c.extra, ",") + "]"})
	}
	writeObject(&b, fields)
	return b.String()
}

// Text returns the builder's result as a JSON Text.
func (c *ComponentText) Text() Text { return JSON(c.JSON()) }

// String returns the same as JSON.
func (c *ComponentText) String() string { return c.JSON() }

func writeObject(b *strings.Builder, fields []jsonField) {
	b.WriteByte('{')
	for i, field := range fields {
		if i > 0 {
			b.WriteByte(',')
		}
		b.WriteString(jsonQuote(field.key))
		b.WriteByte(':')
		b.WriteString(field.value)
	}
	b.WriteByte('}')
}

// jsonQuote writes s as a JSON string. Invalid UTF-8 becomes U+FFFD, as with
// encoding/json.
func jsonQuote(s string) string {
	const hex = "0123456789abcdef"
	var b strings.Builder
	b.Grow(len(s) + 2)
	b.WriteByte('"')
	for i := 0; i < len(s); {
		c := s[i]
		if c < utf8.RuneSelf {
			switch {
			case c == '"' || c == '\\':
				b.WriteByte('\\')
				b.WriteByte(c)
			case c == '\n':
				b.WriteString(`\n`)
			case c == '\r':
				b.WriteString(`\r`)
			case c == '\t':
				b.WriteString(`\t`)
			case c < 0x20:
				b.WriteString(`\u00`)
				b.WriteByte(hex[c>>4])
				b.WriteByte(hex[c&0xf])
			default:
				b.WriteByte(c)
			}
			i++
			continue
		}
		r, size := utf8.DecodeRuneInString(s[i:])
		if r == utf8.RuneError && size == 1 {
			b.WriteString(`�`)
		} else {
			b.WriteString(s[i : i+size])
		}
		i += size
	}
	b.WriteByte('"')
	return b.String()
}

// Ptr returns a pointer to v, for the optional fields of StatusOverride and
// the other records.
func Ptr[T any](v T) *T { return &v }
