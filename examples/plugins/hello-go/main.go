// Command hello-go is the Go example plugin and a host conformance fixture.
//
// It behaves exactly like examples/plugins/hello-rs and hello-c, so the host
// runs one conformance suite against all three:
//
//   - init(config) decodes the JSON configuration ({"deny": "<name>"}, deny
//     optional; invalid JSON or a non-string deny fails init), starts one
//     repeating 1000 ms timer with timers.every(1000), subscribes to the bus
//     subject hello.ping without a queue group (a failed subscription fails
//     init) and returns this registration:
//   - command hello, no aliases, permission hello.use, suggest players, not
//     for the console;
//   - command servers, open to every player, no suggestions;
//   - hooks login, command, join, message, timer, so the host skips every
//     other hook.
//   - login(ctx) returns deny(plain("You are not welcome here.")) when ctx.name
//     equals the configured deny name exactly, otherwise continue.
//   - command(ctx) for hello returns the message greeting with the argument
//     name set to ctx.name, which Rift renders from messages.toml in the
//     player's language ("Hello, <ctx.name>!" by default). For servers it
//     shows the player the server selector (see selector below) with
//     players.menu and returns continue. Host errors are ignored.
//   - menu(event): a click on a button with the action join transfers the
//     player to the server named by the button's payload. Host errors are
//     ignored; every other event is ignored.
//   - join(ctx) calls players.message(ctx.uuid, plain("Welcome, <ctx.name>!")),
//     then store.increment("hello", "joins", 1). Host errors are ignored.
//   - message(msg): when msg.reply is set, publishes msg.payload unchanged to
//     that subject (no reply subject of its own); otherwise does nothing.
//     Publish errors are ignored.
//   - timer(handle) logs "tick" at debug level.
//
// No other hook is implemented and nothing else is logged.
//
// Build: make (see Makefile and sdk/go/README.md).
package main

import "github.com/Jotrorox/rift/sdk/go/rift"

// selector is the server selector: a chest of three rows framed in gray glass,
// with a button per server (action join, the server's name as payload) and one
// that closes the menu.
func selector() rift.Menu {
	server := func(item, name, server string) rift.Button {
		return rift.Button{
			Item:        item,
			Name:        rift.Mini(name).Text(),
			Lore:        []rift.Text{rift.Plain("Click to join")},
			ID:          "join",
			PayloadJSON: rift.JSONString(server),
		}
	}
	return rift.Menu{
		ID:     "servers",
		Title:  rift.Mini("<dark_gray>Choose a server").Text(),
		Layout: []string{"#########", "# S C M #", "####X####"},
		Keys: map[rune]rift.Button{
			'#': {Item: "gray_stained_glass_pane", Name: rift.Plain(" ")},
			'S': server("grass_block", "<green>Survival", "survival"),
			'C': server("bricks", "<gold>Creative", "creative"),
			'M': server("bow", "<red>Minigames", "minigames"),
			'X': {Item: "barrier", Name: rift.Mini("<red>Close").Text(), Close: true},
		},
	}
}

var (
	deny    string
	denySet bool
)

func init() {
	rift.OnInit(func(configJSON string) (rift.Registration, error) {
		// rift.DecodeConfig with a struct works too, but encoding/json adds
		// about 850 KB to a TinyGo build.
		var err error
		if deny, denySet, err = rift.ConfigString(configJSON, "deny"); err != nil {
			return rift.Registration{}, err
		}
		rift.Every(1000)
		if _, err := rift.Subscribe("hello.ping", ""); err != nil {
			return rift.Registration{}, err
		}
		return rift.Registration{
			Commands: []rift.CommandSpec{
				{Name: "hello", Permission: "hello.use", Suggest: rift.SuggestPlayers()},
				{Name: "servers"},
			},
			Hooks: []rift.Hook{rift.HookLogin, rift.HookCommand, rift.HookJoin, rift.HookMessage, rift.HookTimer},
		}, nil
	})

	rift.OnLogin(func(ctx rift.Context) rift.Decision {
		if denySet && ctx.Name == deny {
			return rift.Deny(rift.Plain("You are not welcome here."))
		}
		return rift.Continue()
	})

	rift.OnCommand(func(ctx rift.Context) rift.Decision {
		if ctx.Command != nil && *ctx.Command == "servers" {
			_, _ = rift.ShowMenu(ctx.UUID, selector())
			return rift.Continue()
		}
		// The text lives in messages.toml, translated per player.
		return rift.Message(rift.Msg("greeting").Arg("name", ctx.Name).Text())
	})

	rift.OnMenu(func(event rift.MenuEvent) {
		if event.Kind != rift.MenuKindClick || event.Action != "join" {
			return
		}
		// PayloadString reads a string without encoding/json.
		if server, ok := event.PayloadString(); ok {
			_, _ = rift.Transfer(event.Ctx.UUID, server)
		}
	})

	rift.OnJoin(func(ctx rift.Context) {
		_, _ = rift.SendMessage(ctx.UUID, rift.Plain("Welcome, "+ctx.Name+"!"))
		_, _ = rift.StoreIncrement("hello", "joins", 1)
	})

	rift.OnMessage(func(msg rift.BusMessage) {
		_, _ = rift.Reply(msg, msg.Payload)
	})

	rift.OnTimer(func(uint64) {
		rift.Log(rift.LevelDebug, "tick")
	})
}

func main() {}
