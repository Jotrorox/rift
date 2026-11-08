// Package rift is the Go SDK for Rift WebAssembly plugins.
//
// It wraps the bindings that wit-bindgen-go generates for the
// rift:plugin@0.1.0 contract (see the gen directory) with plain Go types,
// registers a no-op default for every hook, and exposes setters for the hooks
// a plugin implements:
//
//	func init() {
//		rift.OnInit(func(configJSON string) (rift.Registration, error) {
//			return rift.Registration{
//				Commands: []rift.CommandSpec{{Name: "hello", Permission: "hello.use", Suggest: rift.SuggestPlayers()}},
//				Hooks:    []rift.Hook{rift.HookCommand},
//			}, nil
//		})
//		rift.OnCommand(func(ctx rift.Context) rift.Action {
//			return rift.Message("Hello, " + ctx.Name + "!")
//		})
//	}
//
//	func main() {}
//
// Register handlers from an init function: the host calls exports right after
// instantiation and never runs main. The host delivers only the hooks listed
// in the registration that OnInit returns (an empty list delivers every
// hook); leave Registration.Hooks nil and the SDK lists exactly the hooks
// that have a handler.
//
// Defaults: init returns an empty registration, decision hooks continue,
// route keeps the configured policy, suggest returns nothing, status and http
// return no override, and every notification hook does nothing.
//
// Build with TinyGo 0.34 or newer:
//
//	tinygo build -target=wasip2 --wit-package <rift>/wit --wit-world plugin -no-debug -o plugin.wasm .
package rift
