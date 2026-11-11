// Package chat adds the chat hook of rift:plugin 0.1.5 to a Rift plugin: it
// sees every chat message, and every command that reaches the backend,
// before the backend does, and may drop it.
//
//	import (
//		"github.com/Jotrorox/rift/sdk/go/rift"
//		"github.com/Jotrorox/rift/sdk/go/rift/chat"
//	)
//
//	func init() {
//		chat.OnChat(func(attempt rift.ChatAttempt) rift.Action {
//			if attempt.Kind == rift.ChatMessage && muted[attempt.Ctx.UUID] {
//				return rift.Deny("You are muted.")
//			}
//			return rift.Continue()
//		})
//	}
//
// The handler answers rift.Continue, which lets the packet through
// unchanged, or rift.Deny or rift.DenyJSON, which drop it and show the
// player the text; any other answer is a failure of the plugin. For a
// command, attempt.Ctx.Command is the root name and attempt.Ctx.Args the
// rest. Commands that Rift or a plugin owns never come here.
//
// Importing this package is what registers the hook: the component then
// exports rift:plugin/chat, and Rift asks every component with that export
// about every message of every player, whatever Registration.Hooks lists. A
// plugin that does not import it has no such export and is never asked.
//
// Two things follow. Rift starts a plugin with the export only when the
// proxy sets authentication.signed_chat = false, because dropping one signed
// message would break the player's signature chain. And the component must
// be built against the world that has the export:
//
//	tinygo build -target=wasip2 --wit-package <rift>/wit --wit-world plugin-chat -no-debug -o plugin.wasm .
//
// Hosts before 0.1.5 load such a component and never call the hook.
package chat

import (
	gen "github.com/Jotrorox/rift/sdk/go/gen/rift/plugin/chat"
	"github.com/Jotrorox/rift/sdk/go/gen/rift/plugin/types"
	"github.com/Jotrorox/rift/sdk/go/rift"
)

// The registered handler; it lets everything through until OnChat replaces
// it.
var onChat = func(rift.ChatAttempt) rift.Action { return rift.Continue() }

// OnChat sets the handler for chat messages and backend commands. The
// deadline is 50 ms unless the manifest sets deadlines.chat; a missed
// deadline or a failure applies the manifest's policy, where deny drops the
// message.
func OnChat(f func(rift.ChatAttempt) rift.Action) { onChat = f }

func init() {
	gen.Exports.Chat = func(a types.ChatAttempt) types.Decision {
		return onChat(rift.LiftChatAttempt(a)).Decision()
	}
}
