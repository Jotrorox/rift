// Package rift is the Go SDK for Rift WebAssembly plugins.
//
// It wraps the bindings that wit-bindgen-go generates for the
// rift:plugin@0.3.0 contract (see the gen directory) with plain Go types,
// registers a default for every hook, and exposes setters for the hooks a
// plugin implements:
//
//	func init() {
//		rift.OnInit(func(configJSON string) (rift.Registration, error) {
//			return rift.Registration{
//				Commands: []rift.CommandSpec{{Name: "hello", Aliases: []string{"hi"}, Permission: "hello.use", Suggest: rift.SuggestPlayers()}},
//				Hooks:    []rift.Hook{rift.HookCommand},
//			}, nil
//		})
//		rift.OnCommand(func(ctx rift.Context) rift.Decision {
//			return rift.Message(rift.Plain("Hello, " + ctx.Name + "!"))
//		})
//	}
//
//	func main() {}
//
// Register handlers from an init function: the host calls exports right after
// instantiation and never runs main. The host delivers only the hooks listed
// in the registration that OnInit returns. An empty list delivers every hook
// except chat and kick, which a plugin gets only by listing HookChat and
// HookKick. Leave Registration.Hooks nil and the SDK lists exactly the hooks
// that have a handler, chat and kick included.
//
// Defaults: init returns an empty registration, decision hooks continue,
// route keeps the configured policy, suggest returns nothing, status and http
// return no override, every notification hook does nothing, and work fails
// with ErrNoWork.
//
// Text: everything shown to a player is a Text, either Plain (shown exactly
// as written) or a JSON text component: JSON for one written by hand, or the
// Text method of the builders Mini, Legacy and Component. A plain string is
// never read as markup (see docs/text.md). The zero Text means "none" where
// text is optional.
//
// Menus: ShowMenu opens an inventory menu from the plain structs Menu, Button
// and MenuPatch (see docs/menus.md); clicks, typing and the end of the menu
// reach the OnMenu handler as a MenuEvent with typed Kind, Click and Reason.
//
// Background work: a plugin runs one call at a time, so a hook that needs a
// slow lookup calls Defer and Run and returns. A background worker, a
// separate instance of the same component, runs the OnWork handler; the
// result comes back through OnJobDone, which answers with Resolve.
//
// Build with TinyGo 0.34 or newer (0.42.0 recommended):
//
//	tinygo build -target=wasip2 --wit-package <rift>/wit --wit-world plugin -no-debug -o plugin.wasm .
package rift
