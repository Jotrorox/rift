package rift

import (
	"encoding/json"
	"errors"
	"strings"
	"unicode/utf8"
)

// The configuration passed to OnInit and OnReload is a JSON object ("{}"
// without one). Two helpers read it:
//
//   - ConfigString reads one top-level string member with a small built-in
//     parser and adds only a few kilobytes to the plugin;
//   - DecodeConfig decodes the whole object with encoding/json, which is
//     convenient for structured configuration but adds roughly 850 KB to a
//     TinyGo build.

// ErrInvalidConfig reports configuration that is not a JSON object.
var ErrInvalidConfig = errors.New("invalid configuration: expected a JSON object")

// DecodeConfig decodes the configuration into v (usually a pointer to a
// struct with json tags). An empty string counts as "{}".
func DecodeConfig(configJSON string, v any) error {
	if strings.TrimSpace(configJSON) == "" {
		configJSON = "{}"
	}
	if err := json.Unmarshal([]byte(configJSON), v); err != nil {
		return errors.New("invalid configuration: " + err.Error())
	}
	return nil
}

// ConfigString validates the configuration and reads one top-level string
// member. ok is false when the key is missing or null; a value of another
// type, or input that is not a JSON object, is an error. An empty string
// counts as "{}". Nested values are validated and skipped, not searched.
func ConfigString(configJSON, key string) (value string, ok bool, err error) {
	if strings.TrimSpace(configJSON) == "" {
		return "", false, nil
	}
	p := &jsonScanner{s: configJSON}
	p.ws()
	if !p.consume('{') {
		return "", false, ErrInvalidConfig
	}
	wrongType := false
	p.ws()
	if !p.consume('}') {
		for {
			p.ws()
			name, valid := p.str()
			if !valid {
				return "", false, ErrInvalidConfig
			}
			p.ws()
			if !p.consume(':') {
				return "", false, ErrInvalidConfig
			}
			p.ws()
			if name == key {
				// A later duplicate wins.
				value, ok, wrongType = "", false, false
				switch {
				case p.peek('"'):
					if value, valid = p.str(); !valid {
						return "", false, ErrInvalidConfig
					}
					ok = true
				case p.literal("null"):
				default:
					if !p.skip(0) {
						return "", false, ErrInvalidConfig
					}
					wrongType = true
				}
			} else if !p.skip(0) {
				return "", false, ErrInvalidConfig
			}
			p.ws()
			if p.consume(',') {
				continue
			}
			if p.consume('}') {
				break
			}
			return "", false, ErrInvalidConfig
		}
	}
	p.ws()
	if p.i != len(p.s) {
		return "", false, ErrInvalidConfig
	}
	if wrongType {
		return "", false, errors.New("invalid configuration: `" + key + "` must be a string")
	}
	return value, ok, nil
}

type jsonScanner struct {
	s string
	i int
}

func (p *jsonScanner) ws() {
	for p.i < len(p.s) {
		switch p.s[p.i] {
		case ' ', '\t', '\n', '\r':
			p.i++
		default:
			return
		}
	}
}

func (p *jsonScanner) peek(c byte) bool { return p.i < len(p.s) && p.s[p.i] == c }

func (p *jsonScanner) consume(c byte) bool {
	if p.peek(c) {
		p.i++
		return true
	}
	return false
}

func (p *jsonScanner) literal(word string) bool {
	if strings.HasPrefix(p.s[p.i:], word) {
		p.i += len(word)
		return true
	}
	return false
}

func (p *jsonScanner) hex4() (rune, bool) {
	if len(p.s)-p.i < 4 {
		return 0, false
	}
	var v rune
	for _, c := range []byte(p.s[p.i : p.i+4]) {
		v <<= 4
		switch {
		case c >= '0' && c <= '9':
			v |= rune(c - '0')
		case c >= 'a' && c <= 'f':
			v |= rune(c-'a') + 10
		case c >= 'A' && c <= 'F':
			v |= rune(c-'A') + 10
		default:
			return 0, false
		}
	}
	p.i += 4
	return v, true
}

// str parses a string at the opening quote and returns it unescaped.
func (p *jsonScanner) str() (string, bool) {
	if !p.consume('"') {
		return "", false
	}
	var out []byte
	start := p.i
	for p.i < len(p.s) {
		c := p.s[p.i]
		switch {
		case c == '"':
			out = append(out, p.s[start:p.i]...)
			p.i++
			return string(out), true
		case c < 0x20:
			return "", false
		case c != '\\':
			p.i++
			continue
		}
		out = append(out, p.s[start:p.i]...)
		p.i++
		if p.i >= len(p.s) {
			return "", false
		}
		e := p.s[p.i]
		p.i++
		switch e {
		case '"', '\\', '/':
			out = append(out, e)
		case 'b':
			out = append(out, '\b')
		case 'f':
			out = append(out, '\f')
		case 'n':
			out = append(out, '\n')
		case 'r':
			out = append(out, '\r')
		case 't':
			out = append(out, '\t')
		case 'u':
			r, ok := p.hex4()
			if !ok {
				return "", false
			}
			if r >= 0xD800 && r <= 0xDBFF {
				if !p.literal(`\u`) {
					return "", false
				}
				lo, ok := p.hex4()
				if !ok || lo < 0xDC00 || lo > 0xDFFF {
					return "", false
				}
				r = 0x10000 + (r-0xD800)<<10 + (lo - 0xDC00)
			} else if r >= 0xDC00 && r <= 0xDFFF {
				return "", false
			}
			out = utf8.AppendRune(out, r)
		default:
			return "", false
		}
		start = p.i
	}
	return "", false
}

func (p *jsonScanner) digits() bool {
	start := p.i
	for p.i < len(p.s) && p.s[p.i] >= '0' && p.s[p.i] <= '9' {
		p.i++
	}
	return p.i > start
}

func (p *jsonScanner) number() bool {
	p.consume('-')
	if !p.consume('0') && !p.digits() {
		return false
	}
	if p.consume('.') && !p.digits() {
		return false
	}
	if p.consume('e') || p.consume('E') {
		if !p.consume('+') {
			p.consume('-')
		}
		if !p.digits() {
			return false
		}
	}
	return true
}

// skip validates and skips one value.
func (p *jsonScanner) skip(depth int) bool {
	if depth > 64 {
		return false
	}
	p.ws()
	if p.i >= len(p.s) {
		return false
	}
	switch c := p.s[p.i]; c {
	case '"':
		_, ok := p.str()
		return ok
	case 't':
		return p.literal("true")
	case 'f':
		return p.literal("false")
	case 'n':
		return p.literal("null")
	case '{', '[':
		closing := byte('}')
		if c == '[' {
			closing = ']'
		}
		p.i++
		p.ws()
		if p.consume(closing) {
			return true
		}
		for {
			if c == '{' {
				p.ws()
				if _, ok := p.str(); !ok {
					return false
				}
				p.ws()
				if !p.consume(':') {
					return false
				}
			}
			if !p.skip(depth + 1) {
				return false
			}
			p.ws()
			if p.consume(',') {
				continue
			}
			return p.consume(closing)
		}
	default:
		return p.number()
	}
}

// words appends the visible text of one JSON text component value: strings,
// list items, and an object's text (or translation fallback or key)
// followed by its extra components. Everything else is validated and
// skipped.
func (p *jsonScanner) words(out *strings.Builder, depth int) bool {
	if depth > 64 {
		return false
	}
	p.ws()
	switch {
	case p.peek('"'):
		text, ok := p.str()
		out.WriteString(text)
		return ok
	case p.consume('['):
		p.ws()
		if p.consume(']') {
			return true
		}
		for {
			if !p.words(out, depth+1) {
				return false
			}
			p.ws()
			if p.consume(']') {
				return true
			}
			if !p.consume(',') {
				return false
			}
		}
	case p.consume('{'):
		var text, fallback, translate string
		var extra strings.Builder
		// The mini and legacy authoring objects lose their tags and codes.
		var mini, legacy, marker *string
		args := map[string]string{}
		p.ws()
		if !p.consume('}') {
			for {
				p.ws()
				key, ok := p.str()
				if !ok {
					return false
				}
				p.ws()
				if !p.consume(':') {
					return false
				}
				p.ws()
				switch {
				case key == "extra":
					if !p.words(&extra, depth+1) {
						return false
					}
				case (key == "mini" || key == "legacy" || key == "marker") && p.peek('"'):
					value, ok := p.str()
					if !ok {
						return false
					}
					switch key {
					case "mini":
						mini = &value
					case "legacy":
						legacy = &value
					default:
						marker = &value
					}
				case key == "args" && p.peek('{'):
					if !p.argWords(args, depth+1) {
						return false
					}
				case (key == "text" || key == "fallback" || key == "translate") && p.peek('"'):
					value, ok := p.str()
					if !ok {
						return false
					}
					switch key {
					case "text":
						text = value
					case "fallback":
						fallback = value
					default:
						translate = value
					}
				default:
					if !p.skip(depth + 1) {
						return false
					}
				}
				p.ws()
				if p.consume('}') {
					break
				}
				if !p.consume(',') {
					return false
				}
			}
		}
		switch {
		case mini != nil:
			stripMarkup(*mini, out, func(name string, out *strings.Builder) { out.WriteString(args[name]) })
			return true
		case legacy != nil:
			m := '&'
			if marker != nil {
				if r, _ := utf8.DecodeRuneInString(*marker); r != utf8.RuneError {
					m = r
				}
			}
			stripLegacy(*legacy, m, out)
			return true
		case text != "":
			out.WriteString(text)
		case fallback != "":
			out.WriteString(fallback)
		default:
			out.WriteString(translate)
		}
		out.WriteString(extra.String())
		return true
	default:
		return p.skip(depth)
	}
}

// argWords reads the args object of a mini object: each argument's words by
// lowercase name.
func (p *jsonScanner) argWords(args map[string]string, depth int) bool {
	if depth > 64 || !p.consume('{') {
		return false
	}
	p.ws()
	if p.consume('}') {
		return true
	}
	for {
		p.ws()
		name, ok := p.str()
		if !ok {
			return false
		}
		p.ws()
		if !p.consume(':') {
			return false
		}
		p.ws()
		var value strings.Builder
		if p.peek('"') || p.peek('{') || p.peek('[') {
			if !p.words(&value, depth+1) {
				return false
			}
		} else {
			start := p.i
			if !p.skip(depth + 1) {
				return false
			}
			value.WriteString(p.s[start:p.i])
		}
		args[strings.ToLower(name)] = value.String()
		p.ws()
		if p.consume('}') {
			return true
		}
		if !p.consume(',') {
			return false
		}
	}
}
