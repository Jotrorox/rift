# Go plugins

A Go plugin is a WebAssembly component built with [TinyGo](https://tinygo.org)
and the SDK in [`sdk/go`](../sdk/go) (module
`github.com/Jotrorox/rift/sdk/go`): plain Go types, a no-op default for every
hook and host-call wrappers. Read the [plugin overview](plugins.md) for how
plugins are enabled, dispatched and restarted, and
[`sdk/go/README.md`](../sdk/go/README.md) for the SDK's details.

## Build

Upstream Go cannot emit WebAssembly components yet
([golang/go#65333](https://github.com/golang/go/issues/65333)), so TinyGo is
the supported compiler. It needs
[`wasm-tools`](https://github.com/bytecodealliance/wasm-tools) on `PATH` and
`wasm-opt` from [Binaryen](https://github.com/WebAssembly/binaryen) (bundled in
TinyGo's Linux release; `brew install binaryen` on macOS).

**Use TinyGo 0.42.0** (see [gotchas](#gotchas)):

```sh
go get github.com/Jotrorox/rift/sdk/go
tinygo build -target=wasip2 --wit-package path/to/rift/wit --wit-world plugin -no-debug -o my_plugin.wasm .
```

`--wit-package` takes the `wit/` directory of a Rift checkout (or a packaged
WIT from `wasm-tools component wit --wasm -o rift-plugin.wasm wit`).
`-no-debug` strips DWARF; `-scheduler=none` shrinks the output further for a
plugin that starts no goroutines. Check the result with
`wasm-tools component wit my_plugin.wasm`.

## A minimal plugin

```go
package main

import "github.com/Jotrorox/rift/sdk/go/rift"

var deny string

func init() {
	rift.OnInit(func(configJSON string) (rift.Registration, error) {
		name, ok, err := rift.ConfigString(configJSON, "deny")
		if err != nil {
			return rift.Registration{}, err
		}
		if ok {
			deny = name
		}
		return rift.Registration{
			Commands: []rift.CommandSpec{{Name: "hello", Permission: "hello.use", Suggest: rift.SuggestPlayers()}},
		}, nil
	})
	rift.OnLogin(func(ctx rift.Context) rift.Action {
		if deny != "" && ctx.Name == deny {
			return rift.Deny("You are not welcome here.")
		}
		return rift.Continue()
	})
	rift.OnCommand(func(ctx rift.Context) rift.Action {
		return rift.Message("Hello, " + ctx.Name + "!")
	})
}

func main() {}
```

```toml
# plugins/my-plugin/plugin.toml
name = "my-plugin"
version = "0.1.0"
api = "0.1"
engine = "wasm"
entry = "my_plugin.wasm"

[capabilities]
filesystem = ["data"]
network = false
env = []
```

Then `rift.plugin("my-plugin", { deny = "Mallory" })` in `rift.lua`.
[`examples/plugins/hello-go`](../examples/plugins/hello-go) is the complete
example (`make TINYGO=/path/to/tinygo`), behaving exactly like the Rust, C and
Lua examples.

## Mapping the contract

| Contract | Go |
| --- | --- |
| `handler` exports | `rift.OnInit`, `rift.OnLogin`, `rift.OnCommand`, ... set from a package `init` function. The host calls exports right after instantiation and never runs `main`, which must exist but stays empty. |
| `registration` | `rift.Registration{Commands, Hooks}`. Leave `Hooks` nil and the SDK lists exactly the hooks you set a handler for. |
| `action` | `rift.Continue()`, `rift.Deny`, `rift.DenyJSON`, `rift.Server`, `rift.Message`, `rift.MessageJSON`, `rift.Queue`, `rift.LeaveQueue()` (the `JSON` forms take a JSON text component, see [rich text](extensions.md#rich-text)); `route` answers are `rift.RouteDefault()`, `rift.RouteBackend`, `rift.RouteReject`. |
| `players` | `rift.Players`, `rift.Player`, `rift.PlayerNames`, `rift.Locale` (since 0.1.5; the client's language, and false while Rift does not know it, which is always the case in `OnLogin` and, for clients before 1.20.2, in `OnJoin`; see [timing](plugins.md#host-api)), `rift.SendMessage`, `rift.SendMessageJSON`, `rift.SendActionBar`, `rift.SendActionBarJSON`, `rift.SendTitle`, `rift.SendTitleJSON` (since 0.1.4), `rift.SetTabList`, `rift.SetBossBar`, `rift.ShowDialog`, `rift.ClearDialog`, `rift.Kick`, `rift.KickJSON`, `rift.Transfer`, `rift.Broadcast`, `rift.BroadcastJSON`. |
| `dialogs` export | `rift.OnDialog(func(rift.DialogResponse))` receives clicks on the plugin's [dialogs](dialogs.md); it needs no hook in the registration. |
| `styled` export | `rift.OnRichSuggest(func(rift.Context) []rift.RichSuggestion)` and `rift.OnRichStatus(func(rift.Ping) *rift.RichStatusOverride)` (since 0.1.4) replace `OnSuggest` and `OnStatus` with JSON text tooltips, version and sample lines. Without them the SDK answers `styled` with the plain handlers; hosts before 0.1.4 get the rich text's words. |
| `chat` export | `chat.OnChat(func(rift.ChatAttempt) rift.Action)` in package `sdk/go/rift/chat` (since 0.1.5), for a plugin built with `--wit-world plugin-chat`; see [chat](#chat). |
| rich text | `rift.Mini`, `rift.Legacy`, `rift.Component`, `rift.Plain`, `rift.RawJSON` (below); `rift.Ptr(v)` for optional fields. |
| `store` | `rift.StoreGet`, `StoreSet`, `StoreDelete`, `StoreIncrement`, `StoreList`. |
| `permissions` | `rift.HasPermission`, `SetPermission`, `AddGroup`, `RemoveGroup`, `Groups`, `ListGroupsJSON`. |
| `bus` | `rift.Publish`, `Reply`, `Request`, `Subscribe`, `Unsubscribe`, `StreamPublish`. |
| `servers` | `rift.ServerCreate`, `ServerRemove`, `ServerStart`, `ServerStop`, `TemplateDownload`, `Control`. |
| `timers` | `rift.Every`, `After`, `CancelTimer`. |
| `log`, `meta` | `rift.Log(level, msg)` and `Tracef` ... `Errorf`; `rift.Name`, `Version`, `DataDir`, `ConfigDir`, `Clock`, `Time`. |
| `result<T, string>` | `(T, error)` with a `rift.HostError`. |
| configuration JSON | `rift.ConfigString(json, "key")` returns `(value, ok, err)` with a built-in parser; `rift.DecodeConfig(json, &cfg)` uses `encoding/json`, which adds about 850 KB. |


## Chat

The chat hook sees chat messages, and the commands that reach the backend,
before the backend does ([the hook](plugins.md#chat)). Exporting it is the
registration, so it lives in a package of its own: a plugin that imports
`sdk/go/rift/chat` exports `rift:plugin/chat`, and one that does not is never
asked.

```go
import (
	"github.com/Jotrorox/rift/sdk/go/rift"
	"github.com/Jotrorox/rift/sdk/go/rift/chat"
)

func init() {
	chat.OnChat(func(attempt rift.ChatAttempt) rift.Action {
		if attempt.Kind == rift.ChatMessage && muted[attempt.Ctx.UUID] {
			return rift.Deny("You are muted.")
		}
		return rift.Continue()
	})
}
```

Build such a plugin against world `plugin-chat`, which has the export:

```sh
tinygo build -target=wasip2 --wit-package path/to/rift/wit --wit-world plugin-chat -no-debug -o mute.wasm .
```

`attempt.Text` is the message, or the command line without its slash; for a
command, `attempt.Ctx.Command` is the root name and `attempt.Ctx.Args` the
rest. Answer `rift.Continue()`, `rift.Deny` or `rift.DenyJSON`. Rift asks the
plugin about every message and starts it only when the proxy sets
`authentication.signed_chat = false`.

## Rich text builders

The `JSON` functions and answers take a JSON text component. The builders
write one, escaping every string, without `encoding/json`:

```go
// Rift markup, written as {"mini": "...", "args": {...}}; see docs/text.md.
welcome := rift.Mini("<gold>Welcome, <player>!</gold> <gray><click:run_command:/spawn>[Spawn]").
	Arg("player", ctx.Name).                      // shown as written, never parsed
	ArgComponent("rank", rift.Legacy("&c[Admin]")) // a component argument
rift.SendMessageJSON(ctx.UUID, welcome.JSON())

// Legacy codes, written as {"legacy": "...", "marker": "&"}; .Marker('§') for another marker.
rift.SendTitleJSON(ctx.UUID, rift.TitleJSON{
	TitleJSON:    rift.Mini("<green>Lobby").JSON(),
	SubtitleJSON: rift.Ptr(rift.Legacy("&7Have fun").JSON()),
	FadeIn: 10, Stay: 70, FadeOut: 20,
})

// A component field by field (1.21.5 spellings; Rift converts for older clients).
accept := rift.Component("[Accept]").Color("green").Bold().
	RunCommand("/party accept Alex").HoverText("Join Alex's party").
	Extra(rift.Plain(" or ignore it"))
return rift.MessageJSON(accept.JSON())
```

`rift.Component` has `Color`, `Bold`, `Italic`, `Underlined`, `Strikethrough`,
`Obfuscated`, `RunCommand`, `SuggestCommand`, `OpenURL`, `CopyToClipboard`,
`HoverText` (plain), `Hover` (a component), `Insertion`, `Extra` (children)
and `Field` (any other key, raw JSON). Builders nest through the `TextJSON`
interface (`JSON() string`), which `rift.Plain` (text as written) and
`rift.RawJSON` (hand-written JSON) also implement.

Rich tooltips and server-list text use the same builders:

```go
rift.OnRichSuggest(func(ctx rift.Context) []rift.RichSuggestion {
	return []rift.RichSuggestion{{Text: "lobby", TooltipJSON: rift.Ptr(rift.Mini("<gray>The main lobby").JSON())}}
})
```

Markup is only read from an explicit `mini` object: a bare string, as in
`rift.MessageJSON("<red>hi")`, is shown as written. Pass player input with
`Arg`, never by concatenating it into the markup. The tags are listed in
[Text and markup](text.md).

## Gotchas

- **Pin TinyGo 0.42.0.** TinyGo 0.43.0 and 0.44.0 allocate the memory the host
  writes arguments and results into (`cabi_realloc`) as manually managed memory
  that the garbage collector never reclaims, and the generated bindings never
  free it. Every hook call and every host call that passes strings or lists
  then leaks: about 3 KB per round of five hooks in the hello-go conformance
  run, against a flat 2 MB heap with 0.42.0. CI builds with 0.42.0.
- **The WASI CLI imports are always there.** TinyGo's `wasip2` target expects
  the WASI CLI import set even when a plugin uses none of it, which is why
  guests build against world `plugin`. Rift links all of it; capabilities
  decide what it can reach.
- **Panics trap.** A panic ends the call and marks the instance failed; Rift
  applies `on_failure` and restarts it per `failure.restart`. A hook still
  running at its deadline is interrupted the same way.
- **Single-threaded.** The host never calls one instance concurrently, so
  handlers need no locking. Goroutines run only while a hook runs.
- **Size.** `encoding/json` and `fmt` are large under TinyGo; prefer
  `rift.ConfigString` and plain string concatenation in small plugins.
