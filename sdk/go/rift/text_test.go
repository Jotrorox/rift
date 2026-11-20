package rift

import (
	"encoding/json"
	"math"
	"reflect"
	"testing"
)

func decode(t *testing.T, text string) any {
	t.Helper()
	var value any
	if err := json.Unmarshal([]byte(text), &value); err != nil {
		t.Fatalf("invalid JSON %s: %v", text, err)
	}
	return value
}

func same(t *testing.T, got, want string) {
	t.Helper()
	if g, w := decode(t, got), decode(t, want); !reflect.DeepEqual(g, w) {
		t.Errorf("got %s\nwant %s", got, want)
	}
}

func TestTextBuilders(t *testing.T) {
	mini := Mini(`<gold>Hi "<player>"</gold>`).
		Arg("player", "St<b>eve\n\x01").
		ArgComponent("rank", Legacy("&cAdmin")).
		Arg("player", "Alex\\")
	same(t, mini.JSON(), `{"mini":"<gold>Hi \"<player>\"</gold>","args":{"player":"Alex\\","rank":{"legacy":"&cAdmin","marker":"&"}}}`)
	if got := Mini("<red>x").String(); got != `{"mini":"<red>x"}` {
		t.Errorf("Mini without args = %s", got)
	}
	if got := Legacy("§aHi").Marker('§').JSON(); got != `{"legacy":"§aHi","marker":"§"}` {
		t.Errorf("Legacy = %s", got)
	}
	component := Component(`[a"b]`).Color("#ff8800").Bold().Italic().Underlined().Strikethrough().Obfuscated().
		SuggestCommand("/msg ").Hover(Mini("<green>Hover")).Insertion("ins").
		Extra(Plain("plain"), Component("child").OpenURL("https://example.org")).
		Field("font", `"minecraft:uniform"`)
	same(t, component.JSON(), `{
		"text": "[a\"b]", "color": "#ff8800", "bold": true, "italic": true,
		"underlined": true, "strikethrough": true, "obfuscated": true,
		"click_event": {"action": "suggest_command", "command": "/msg "},
		"hover_event": {"action": "show_text", "value": {"mini": "<green>Hover"}},
		"insertion": "ins",
		"extra": ["plain", {"text": "child", "click_event": {"action": "open_url", "url": "https://example.org"}}],
		"font": "minecraft:uniform"
	}`)
	same(t, Component("c").CopyToClipboard("v").RunCommand("/x").HoverText("t").JSON(),
		`{"text":"c","click_event":{"action":"run_command","command":"/x"},"hover_event":{"action":"show_text","value":"t"}}`)
	// Invalid UTF-8 and control characters still make valid JSON.
	same(t, Plain("a\xffb \x1f").JSON(), `"a\ufffdb \u001f"`)
}

func TestEscape(t *testing.T) {
	for _, v := range []struct{ in, want string }{
		{"plain", "plain"},
		{"a < b", `a \< b`},
		{"<red>", `\<red>`},
		{`\`, `\\`},
		{`\<`, `\\\<`},
		{"", ""},
	} {
		if got := Escape(v.in); got != v.want {
			t.Errorf("Escape(%q) = %q, want %q", v.in, got, v.want)
		}
	}
}

func TestMessagesAndStyleArguments(t *testing.T) {
	invited := Msg("party:invited").
		Arg("from", "Alex").
		ArgComponent("server", Component("lobby").Color("aqua")).
		ArgStyle("team", "<#55ffff><bold>")
	same(t, invited.JSON(), `{
		"message": "party:invited",
		"args": {
			"from": "Alex",
			"server": {"text": "lobby", "color": "aqua"},
			"team": {"style": "<#55ffff><bold>"}
		}
	}`)
	if got := Msg("created").Text().JSON(); got != `{"message":"created"}` {
		t.Errorf("Msg without args = %s", got)
	}
	if got := Msg("created").String(); got != `{"message":"created"}` {
		t.Errorf("Msg.String = %s", got)
	}
	same(t, Mini("<team>x</team>").ArgStyle("team", "<red>").JSON(),
		`{"mini":"<team>x</team>","args":{"team":{"style":"<red>"}}}`)
	same(t, Mini("<m>").ArgComponent("m", Msg("hi")).JSON(), `{"mini":"<m>","args":{"m":{"message":"hi"}}}`)
}

func TestComponentContentsStylesAndEvents(t *testing.T) {
	same(t, Translate("chat.type.text").With(Plain("Alex"), Component("hi").Italic()).Fallback("<Alex> hi").JSON(), `{
		"translate": "chat.type.text",
		"with": ["Alex", {"text": "hi", "italic": true}],
		"fallback": "<Alex> hi"
	}`)
	same(t, Keybind("key.jump").JSON(), `{"keybind":"key.jump"}`)
	same(t, Sprite("item/diamond").JSON(), `{"object":"atlas","sprite":"item/diamond"}`)
	same(t, Sprite("block/stone").Atlas("minecraft:blocks").JSON(),
		`{"object":"atlas","sprite":"block/stone","atlas":"minecraft:blocks"}`)
	same(t, Head("Alex").JSON(), `{"object":"player","player":{"name":"Alex"}}`)
	same(t, Head("Alex").Hat(false).JSON(), `{"object":"player","player":{"name":"Alex"},"hat":false}`)
	same(t, Component("x").Font("minecraft:uniform").Decoration("italic", false).Bold().Decoration("bold", false).
		Field("insertion", `"i"`).JSON(),
		`{"text":"x","font":"minecraft:uniform","italic":false,"bold":false,"insertion":"i"}`)
	// The host's <shadow> arithmetic: opacity * 255, rounded, as signed ARGB.
	for _, v := range []struct {
		color   string
		opacity float64
		want    string
	}{
		{"#ff0000", 0.25, "1090453504"},
		{"#000000", 1, "-16777216"},
		{"#FFAA00", 0.5, "-2130728448"},
		{"#123456", 0, "1193046"},
		{"#ffffff", 2, "-1"},
		{"#ffffff", math.NaN(), "16777215"},
		{"red", 0.5, `"red"`},
		{"#fffff", 0.5, `"#fffff"`},
		{"#+fffff", 0.5, `"#+fffff"`},
	} {
		same(t, Component("x").Shadow(v.color, v.opacity).JSON(), `{"text":"x","shadow_color":`+v.want+`}`)
	}
	same(t, Component("x").ChangePage(3).JSON(), `{"text":"x","click_event":{"action":"change_page","page":3}}`)
	same(t, Component("x").HoverItem("minecraft:diamond", 2).JSON(),
		`{"text":"x","hover_event":{"action":"show_item","id":"minecraft:diamond","count":2}}`)
	const uuid = "00000000-0000-0000-0000-000000000001"
	pig := `{"text":"x","hover_event":{"action":"show_entity","id":"minecraft:pig","uuid":"` + uuid + `"}}`
	same(t, Component("x").HoverEntity("minecraft:pig", uuid, nil).JSON(), pig)
	same(t, Component("x").HoverEntity("minecraft:pig", uuid, Text{}).JSON(), pig)
	same(t, Component("x").HoverEntity("minecraft:pig", uuid, Mini("<red>Bob")).JSON(),
		`{"text":"x","hover_event":{"action":"show_entity","id":"minecraft:pig","uuid":"`+uuid+`","name":{"mini":"<red>Bob"}}}`)
	// JSON does not change the builder, so it can be called twice.
	c := Translate("k").With(Plain("a")).Extra(Plain("b"))
	if first, second := c.JSON(), c.JSON(); first != second || first != `{"translate":"k","with":["a"],"extra":["b"]}` {
		t.Errorf("Translate JSON = %s then %s", first, second)
	}
}

func TestText(t *testing.T) {
	plain := Plain(`<red>"hi"`)
	if plain.IsJSON() || plain.IsZero() || plain.String() != `<red>"hi"` || plain.JSON() != `"<red>\"hi\""` {
		t.Errorf("Plain = %+v", plain)
	}
	if l := plain.lower(); l.Plain() == nil || *l.Plain() != `<red>"hi"` || l.JSON() != nil {
		t.Errorf("Plain lowers to %v", l)
	}
	raw := JSON(`{"text":"x"}`)
	if !raw.IsJSON() || raw.JSON() != `{"text":"x"}` || raw.String() != `{"text":"x"}` {
		t.Errorf("JSON = %+v", raw)
	}
	for _, built := range []Text{Mini("<red>x").Text(), Legacy("&cx").Text(), Component("x").Text()} {
		l := built.lower()
		if !built.IsJSON() || l.JSON() == nil || *l.JSON() != built.JSON() {
			t.Errorf("builder Text = %+v", built)
		}
	}
	if got := Mini("<red>x").Text().JSON(); got != `{"mini":"<red>x"}` {
		t.Errorf("Mini.Text = %s", got)
	}
	// The zero Text is "none" in optional places and nests as a Text.
	var none Text
	if opt := none.lowerOpt(); !none.IsZero() || opt.Some() != nil {
		t.Errorf("zero Text is not none")
	}
	if opt := Plain("x").lowerOpt(); opt.Some() == nil || *opt.Some().Plain() != "x" {
		t.Errorf("Plain is none")
	}
	same(t, Component("a").Extra(Plain("b"), JSON(`{"text":"c"}`)).Hover(Plain("h")).JSON(),
		`{"text":"a","hover_event":{"action":"show_text","value":"h"},"extra":["b",{"text":"c"}]}`)
}

func TestLowering(t *testing.T) {
	if d := Deny(Plain("No")).lower(); d.Deny() == nil || *d.Deny().Plain() != "No" {
		t.Errorf("Deny lowers to %v", d)
	}
	if d := Message(Mini("<red>hi").Text()).lower(); d.Message() == nil || *d.Message().JSON() != `{"mini":"<red>hi"}` {
		t.Errorf("Message lowers to %v", d)
	}
	if d := Continue().lower(); !d.Continue() {
		t.Errorf("Continue lowers to %v", d)
	}
	if d := Server("lobby").lower(); d.Server() == nil || *d.Server() != "lobby" {
		t.Errorf("Server lowers to %v", d)
	}
	if d := Queue("pvp").lower(); d.Queue() == nil || *d.Queue() != "pvp" {
		t.Errorf("Queue lowers to %v", d)
	}
	if d := LeaveQueue().lower(); !d.LeaveQueue() {
		t.Errorf("LeaveQueue lowers to %v", d)
	}
	reg := Registration{
		Commands: []CommandSpec{{Name: "hello", Aliases: []string{"hi", "hey"}, Permission: "hello.use", Suggest: SuggestPlayers(), Console: true}},
		Hooks:    []Hook{HookCommand, HookChat, HookKick},
	}.lower()
	spec := reg.Commands.Slice()[0]
	if spec.Name != "hello" || spec.Aliases.Len() != 2 || spec.Aliases.Slice()[1] != "hey" || !spec.Console || !spec.Suggest.Players() {
		t.Errorf("command spec = %+v", spec)
	}
	if hooks := reg.Hooks.Slice(); len(hooks) != 3 || hooks[1].String() != "chat" || hooks[2].String() != "kick" {
		t.Errorf("hooks = %v", hooks)
	}
	status := StatusOverride{Version: Plain("Rift"), Sample: []Text{Plain("one"), Legacy("&ctwo").Text()}, Online: Ptr(uint32(3))}.lower()
	if status.Motd.Some() != nil || *status.Version.Some().Plain() != "Rift" || *status.Online.Some() != 3 {
		t.Errorf("status = %+v", status)
	}
	if sample := status.Sample.Some().Slice(); len(sample) != 2 || *sample[0].Plain() != "one" || sample[1].JSON() == nil {
		t.Errorf("sample = %v", sample)
	}
	if kept := (StatusOverride{}).lower(); kept.Sample.Some() != nil {
		t.Errorf("nil sample is not kept")
	}
	if cleared := (StatusOverride{Sample: []Text{}}).lower(); cleared.Sample.Some() == nil || cleared.Sample.Some().Len() != 0 {
		t.Errorf("empty sample does not clear")
	}
}

func TestDefaultHandlers(t *testing.T) {
	if _, err := onWork([]byte("x")); err != ErrNoWork || err.Error() != "this plugin has no background work" {
		t.Errorf("default work = %v", err)
	}
	if err := onWorkInit("{}"); err != nil {
		t.Errorf("default work-init = %v", err)
	}
	if d := onChat(ChatAttempt{}).lower(); !d.Continue() {
		t.Errorf("default chat = %v", d)
	}
	if d := onKick(Context{}).lower(); !d.Continue() {
		t.Errorf("default kick = %v", d)
	}
	onJobDone(1, nil, JobError("x"))
	if handled[HookChat] || handled[HookKick] {
		t.Errorf("chat or kick handled without a handler")
	}
}
