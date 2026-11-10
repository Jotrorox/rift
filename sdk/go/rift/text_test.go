package rift

import (
	"encoding/json"
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
	same(t, Plain("a\xffb \x1f").JSON(), `"a�b \u001f"`)
	if got := RawJSON(`{"text":"x"}`).JSON(); got != `{"text":"x"}` {
		t.Errorf("RawJSON = %s", got)
	}
}

func TestWordsOfRichText(t *testing.T) {
	mini := Mini(`<gold>Hi <player>!</gold><br>a < b \<c> <hover:show_text:'<red>x y'>z</hover><Rank>`).
		Arg("player", "Steve").ArgComponent("rank", Component(" [A]"))
	if got := componentWords(mini.JSON()); got != "Hi Steve!\na < b <c> z [A]" {
		t.Errorf("mini words = %q", got)
	}
	if got := componentWords(Legacy("&cRed &#ff8800hex &zkeep").JSON()); got != "Red hex &zkeep" {
		t.Errorf("legacy words = %q", got)
	}
	plain := RichSuggestion{Text: "a", TooltipJSON: Ptr(`{"mini":"<red>tip"}`)}.plain()
	if plain.Tooltip == nil || *plain.Tooltip != "tip" {
		t.Errorf("plain tooltip = %v", plain.Tooltip)
	}
	rich := Suggestion{Text: "a", Tooltip: Ptr(`{"not":"json"}`)}.rich()
	if *rich.TooltipJSON != `"{\"not\":\"json\"}"` || *rich.plain().Tooltip != `{"not":"json"}` {
		t.Errorf("round trip = %s", *rich.TooltipJSON)
	}
	status := StatusOverride{Version: Ptr("Rift"), Sample: []string{"one"}}.rich()
	if *status.VersionJSON != `"Rift"` || status.SampleJSON[0] != `"one"` {
		t.Errorf("rich status = %+v", status)
	}
	back := RichStatusOverride{VersionJSON: Ptr(`{"text":"Ri","extra":["ft"]}`), SampleJSON: []string{}}.plain()
	if *back.Version != "Rift" || back.Sample == nil || len(back.Sample) != 0 {
		t.Errorf("plain status = %+v", back)
	}
}
