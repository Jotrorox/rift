package rift

import (
	"encoding/json"
	"testing"
)

func TestConfigString(t *testing.T) {
	cases := []struct {
		json      string
		value     string
		ok, isErr bool
	}{
		{json: "", ok: false},
		{json: "{}", ok: false},
		{json: ` { "deny" : "Mallory" } `, value: "Mallory", ok: true},
		{json: `{"a":[1,2,{"deny":"x"}],"deny":"Mäl\n"}`, value: "Mäl\n", ok: true},
		{json: `{"x":-0.5e+3,"deny":"😀"}`, value: "😀", ok: true},
		{json: `{"deny":""}`, value: "", ok: true},
		{json: `{"deny":null}`, ok: false},
		{json: `{"deny":"a","deny":null}`, ok: false},
		{json: `{"deny":5}`, isErr: true},
		{json: `{"deny":{"name":"x"}}`, isErr: true},
		{json: `{"deny":"a",}`, isErr: true},
		{json: `{"deny":"a"} x`, isErr: true},
		{json: `[]`, isErr: true},
		{json: `not json`, isErr: true},
		{json: `{"x":tru}`, isErr: true},
		{json: `{"x":01}`, isErr: true},
		{json: `{"x":"\ud800"}`, isErr: true},
		{json: "{\"x\":\"a\x01\"}", isErr: true},
	}
	for _, c := range cases {
		value, ok, err := ConfigString(c.json, "deny")
		if (err != nil) != c.isErr || ok != c.ok || value != c.value {
			t.Errorf("ConfigString(%q) = %q, %v, %v; want %q, %v, err=%v", c.json, value, ok, err, c.value, c.ok, c.isErr)
		}
		// Agree with encoding/json on validity.
		if c.json != "" {
			var anyValue map[string]any
			validObject := json.Unmarshal([]byte(c.json), &anyValue) == nil
			if !c.isErr && !validObject {
				t.Errorf("%q accepted but encoding/json rejects it", c.json)
			}
		}
	}
}

func TestDecodeConfig(t *testing.T) {
	var cfg struct {
		Deny *string `json:"deny"`
	}
	if err := DecodeConfig("", &cfg); err != nil || cfg.Deny != nil {
		t.Fatalf("empty config: %v %v", err, cfg.Deny)
	}
	if err := DecodeConfig(`{"deny":"Mallory"}`, &cfg); err != nil || cfg.Deny == nil || *cfg.Deny != "Mallory" {
		t.Fatalf("deny: %v %v", err, cfg.Deny)
	}
	if err := DecodeConfig(`{"deny":5}`, &cfg); err == nil {
		t.Fatal("accepted a non-string deny")
	}
}

func TestComponentWords(t *testing.T) {
	for _, c := range []struct{ json, words string }{
		{`{"extra":[{"text":": spam"}],"text":"Banned","color":"red"}`, "Banned: spam"},
		{`["a",{"text":"b","bold":true},{"translate":"k","fallback":"f"}]`, "abf"},
		{`"plain"`, "plain"},
		{`not json`, "not json"},
		{`{"text":"x"} trailing`, `{"text":"x"} trailing`},
	} {
		if got := componentWords(c.json); got != c.words {
			t.Errorf("componentWords(%q) = %q, want %q", c.json, got, c.words)
		}
	}
	if a := DenyJSON(`{"text":"No"}`).lower(); a.Deny() == nil || *a.Deny() != "No" {
		t.Errorf("DenyJSON lowers to %v for older hosts", a)
	}
}
