package rift

import (
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

func (m *MiniText) set(name, value string) *MiniText {
	for i := range m.args {
		if m.args[i].key == name {
			m.args[i].value = value
			return m
		}
	}
	m.args = append(m.args, jsonField{name, value})
	return m
}

// Arg makes <name> show text exactly as written: it is never parsed, so a
// player name cannot add tags.
func (m *MiniText) Arg(name, text string) *MiniText { return m.set(name, jsonQuote(text)) }

// ArgComponent makes <name> show a component with its own formatting, such as
// a Component, a Legacy object or a JSON Text.
func (m *MiniText) ArgComponent(name string, component TextJSON) *MiniText {
	return m.set(name, component.JSON())
}

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
	extra  []string
}

// Component is a JSON text component showing text. Chain the setters:
//
//	rift.Component("[Accept]").Color("green").Bold().RunCommand("/party accept Alex")
func Component(text string) *ComponentText {
	return &ComponentText{fields: []jsonField{{"text", jsonQuote(text)}}}
}

// Field sets any other field to raw JSON, such as Field("font",
// `"minecraft:uniform"`) or Field("italic", "false").
func (c *ComponentText) Field(key, rawJSON string) *ComponentText {
	for i := range c.fields {
		if c.fields[i].key == key {
			c.fields[i].value = rawJSON
			return c
		}
	}
	c.fields = append(c.fields, jsonField{key, rawJSON})
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

// HoverText shows text, as written, when the pointer rests on the component.
func (c *ComponentText) HoverText(text string) *ComponentText { return c.Hover(Plain(text)) }

// Hover shows a component, such as another Component or a Mini object, when
// the pointer rests on the component.
func (c *ComponentText) Hover(component TextJSON) *ComponentText {
	return c.Field("hover_event", `{"action":"show_text","value":`+component.JSON()+`}`)
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
	fields := c.fields
	if len(c.extra) > 0 {
		fields = append(fields[:len(fields):len(fields)], jsonField{"extra", "[" + strings.Join(c.extra, ",") + "]"})
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
