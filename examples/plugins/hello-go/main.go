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
//   - command hello, permission hello.use, suggest players;
//   - hooks login, command, join, message, timer, so the host skips every
//     other hook.
//   - login(ctx) returns deny("You are not welcome here.") when ctx.name
//     equals the configured deny name exactly, otherwise continue.
//   - command(ctx) returns message("Hello, <ctx.name>!").
//   - join(ctx) calls players.message(ctx.uuid, "Welcome, <ctx.name>!"), then
//     store.increment("hello", "joins", 1). Host errors are ignored.
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
			},
			Hooks: []rift.Hook{rift.HookLogin, rift.HookCommand, rift.HookJoin, rift.HookMessage, rift.HookTimer},
		}, nil
	})

	rift.OnLogin(func(ctx rift.Context) rift.Action {
		if denySet && ctx.Name == deny {
			return rift.Deny("You are not welcome here.")
		}
		return rift.Continue()
	})

	rift.OnCommand(func(ctx rift.Context) rift.Action {
		return rift.Message("Hello, " + ctx.Name + "!")
	})

	rift.OnJoin(func(ctx rift.Context) {
		_, _ = rift.SendMessage(ctx.UUID, "Welcome, "+ctx.Name+"!")
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
