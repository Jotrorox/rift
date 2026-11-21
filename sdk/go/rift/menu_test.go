package rift

import "testing"

func TestMenuJSON(t *testing.T) {
	glass := Button{Item: "gray_stained_glass_pane", Name: Plain(" ")}
	menu := Menu{
		ID:     "servers",
		Title:  Mini("<dark_gray>Choose a server").Text(),
		Layout: []string{"#########", "# S C M #", "####X####"},
		Keys: map[rune]Button{
			'X': {Item: "barrier", Name: Mini("<red>Close").Text(), Close: true},
			'#': glass,
			'S': {Item: "grass_block", Lore: []Text{Plain("Click to join")}, ID: "join", PayloadJSON: JSONString("survival")},
		},
	}
	want := `{"id":"servers","type":"chest","title":{"mini":"<dark_gray>Choose a server"},
		"layout":["#########","# S C M #","####X####"],
		"keys":{"#":{"item":"gray_stained_glass_pane","name":" "},
			"S":{"item":"grass_block","lore":["Click to join"],"id":"join","payload":"survival"},
			"X":{"item":"barrier","name":{"mini":"<red>Close"},"close":true}}}`
	same(t, menu.JSON(), want)
	if menu.JSON() != menu.JSON() {
		t.Error("the same menu gave different JSON")
	}
}

func TestMenuUnsetFieldsStayOut(t *testing.T) {
	same(t, Menu{ID: "m"}.JSON(), `{"id":"m","type":"chest"}`)
	same(t, Menu{ID: "m", Type: MenuHopper}.JSON(), `{"id":"m","type":"hopper"}`)
	same(t, Button{Item: "stone"}.JSON(), `{"item":"stone"}`)
	// A false that means something is not the same as unset.
	same(t, Button{Item: "stone", Glint: Ptr(false), Details: Ptr(true), Tooltip: Ptr(false)}.JSON(),
		`{"item":"stone","glint":false,"details":true,"tooltip":false}`)
	same(t, Button{Item: "stone", Count: 64}.JSON(), `{"item":"stone","count":64}`)
	same(t, MenuPatch{}.JSON(), `{}`)
}

func TestMenuAllFields(t *testing.T) {
	menu := Menu{
		ID: "shop", Type: MenuChest, Rows: 6, Title: Plain("Shop"),
		Slots: []Slot{
			At(Button{Item: "arrow", Name: Plain("Back"), ID: "page", PayloadJSON: `{"step":-1}`}, 45),
			At(Button{Item: "paper", Clicks: []Click{ClickLeft, ClickShiftRight}}, 1, 2, 3),
			At(Button{Item: "stone", AllClicks: true, ID: "any"}, 0),
			Empty(7, 8),
		},
		StateJSON: `{"page":1}`, Text: "Alex", Typing: true,
	}
	same(t, menu.JSON(), `{"id":"shop","type":"chest","rows":6,"title":"Shop",
		"slots":[{"slot":45,"item":"arrow","name":"Back","id":"page","payload":{"step":-1}},
			{"slots":[1,2,3],"item":"paper","clicks":["left","shift_right"]},
			{"slot":0,"item":"stone","id":"any","clicks":"all"},
			{"slot":7,"empty":true},{"slot":8,"empty":true}],
		"state":{"page":1},"text":"Alex","typing":true}`)
}

func TestButtonLook(t *testing.T) {
	button := Button{
		Item:      "player_head",
		Head:      &HeadSkin{Texture: "abc", Signature: "sig"},
		Color:     "#ff8800",
		Model:     &Model{Number: Ptr(7.0)},
		ItemModel: "minecraft:stone",
	}
	same(t, button.JSON(), `{"item":"player_head","model":7,"item_model":"minecraft:stone",
		"head":{"texture":"abc","signature":"sig"},"color":"#ff8800"}`)
	same(t, Button{Item: "paper", Model: &Model{Floats: []float64{1.5}, Strings: []string{"red"}, Colors: []uint32{0xff0000}}}.JSON(),
		`{"item":"paper","model":{"floats":[1.5],"flags":[],"strings":["red"],"colors":[16711680]}}`)
	same(t, Button{Item: "player_head", Head: &HeadSkin{Player: "Notch"}}.JSON(),
		`{"item":"player_head","head":{"player":"Notch"}}`)
}

func TestMenuPatchJSON(t *testing.T) {
	patch := MenuPatch{
		Slots:     []Slot{At(Button{Item: "clock", Name: Plain("12")}, 4), Empty(0)},
		Keys:      map[rune]Button{'S': {Item: "stone"}},
		Title:     Plain("Now"),
		StateJSON: JSONString("x"),
		Text:      Ptr(""),
	}
	same(t, patch.JSON(), `{"slots":[{"slot":4,"item":"clock","name":"12"},{"slot":0,"empty":true}],
		"keys":{"S":{"item":"stone"}},"title":"Now","state":"x","text":""}`)
	same(t, MenuPatch{Text: Ptr("hi")}.JSON(), `{"text":"hi"}`)
}

func TestMenuStringsAreEscaped(t *testing.T) {
	same(t, Menu{ID: "m", Text: "a\"b\n\x01"}.JSON(), `{"id":"m","type":"chest","text":"a\"b\n\u0001"}`)
	same(t, Menu{ID: "m", Keys: map[rune]Button{'"': {Item: "stone"}}}.JSON(),
		`{"id":"m","type":"chest","keys":{"\"":{"item":"stone"}}}`)
}

func TestMenuEnums(t *testing.T) {
	for c := ClickLeft; c <= ClickOffhand; c++ {
		if got := parseClick(c.String()); got != c || c.String() == "" {
			t.Errorf("click %d: name %q parses to %d", c, c.String(), got)
		}
	}
	for r := CloseReasonPlayer; r <= CloseReasonDisconnect; r++ {
		if got := parseCloseReason(r.String()); got != r || r.String() == "" {
			t.Errorf("reason %d: name %q parses to %d", r, r.String(), got)
		}
	}
	if parseClick("triple") != ClickOther || parseCloseReason("lava") != CloseReasonOther || ClickOther.String() != "" {
		t.Error("an unknown name is not the Other case")
	}
	if ClickShiftLeft.String() != "shift_left" || CloseReasonDisconnect.String() != "disconnect" {
		t.Error("wrong host spelling")
	}
}

func TestMenuEventPayload(t *testing.T) {
	str := func(s string) *string { return &s }
	cases := []struct {
		json  *string
		value string
		ok    bool
	}{
		{nil, "", false},
		{str(`"survival"`), "survival", true},
		{str(` "a\"bé" `), "a\"bé", true},
		{str(`""`), "", true},
		{str(`7`), "", false},
		{str(`{"server":"x"}`), "", false},
		{str(`"x" 1`), "", false},
		{str(`"unterminated`), "", false},
	}
	for _, c := range cases {
		event := MenuEvent{PayloadJSON: c.json, StateJSON: c.json}
		for _, read := range []func() (string, bool){event.PayloadString, event.StateString} {
			if value, ok := read(); value != c.value || ok != c.ok {
				t.Errorf("%v: got %q, %v; want %q, %v", c.json, value, ok, c.value, c.ok)
			}
		}
	}
	var payload struct{ Server string }
	event := MenuEvent{PayloadJSON: str(`{"Server":"creative"}`)}
	if found, err := event.DecodePayload(&payload); !found || err != nil || payload.Server != "creative" {
		t.Errorf("DecodePayload: %v %v %+v", found, err, payload)
	}
	if found, err := event.DecodeState(&payload); found || err != nil {
		t.Errorf("DecodeState without a state: %v %v", found, err)
	}
	bad := MenuEvent{StateJSON: str(`[`)}
	if found, err := bad.DecodeState(&payload); !found || err == nil {
		t.Errorf("DecodeState of bad JSON: %v %v", found, err)
	}
}
