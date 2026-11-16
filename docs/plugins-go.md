# Go plugins

A Go plugin is a WebAssembly component built with [TinyGo](https://tinygo.org)
and the SDK in [`sdk/go`](../sdk/go) (module
`github.com/Jotrorox/rift/sdk/go`): plain Go types, a no-op default for every
hook and host-call wrappers. This guide shows how the plugin contract looks in
Go. [Plugins](plugins.md) covers enabling and trust;
[Plugin events and commands](plugin-events.md) says what each event and answer
means.

## Build

Upstream Go cannot emit WebAssembly components yet
([golang/go#65333](https://github.com/golang/go/issues/65333)), so TinyGo is
the supported compiler. It needs
[`wasm-tools`](https://github.com/bytecodealliance/wasm-tools) on `PATH` and
`wasm-opt` from [Binaryen](https://github.com/WebAssembly/binaryen) (bundled in
TinyGo's Linux release; `brew install binaryen` on macOS).

**Use TinyGo 0.42.0** (see [gotchas](#gotchas)):

```sh
go get github.com/Jotrorox/rift/sdk/go@v0.2.0
tinygo build -target=wasip2 --wit-package path/to/rift/wit --wit-world plugin -no-debug -o my_plugin.wasm .
```

- `--wit-package` takes the `wit/` directory of a Rift checkout (or a packaged
  WIT from `wasm-tools component wit --wasm -o rift-plugin.wasm wit`).
- `-no-debug` strips DWARF. `-scheduler=none` shrinks the output further for a
  plugin that starts no goroutines.
- Check the result with `wasm-tools component wit my_plugin.wasm`.

[`sdk/go/README.md`](../sdk/go/README.md) has the SDK's details.

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
	rift.OnLogin(func(ctx rift.Context) rift.Decision {
		if deny != "" && ctx.Name == deny {
			return rift.Deny(rift.Plain("You are not welcome here."))
		}
		return rift.Continue()
	})
	rift.OnCommand(func(ctx rift.Context) rift.Decision {
		return rift.Message(rift.Plain("Hello, " + ctx.Name + "!"))
	})
}

func main() {}
```

```toml
# plugins/my-plugin/plugin.toml
name = "my-plugin"
version = "0.1.0"
api = "0.2"
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
| `handler` exports | `rift.OnInit`, `rift.OnLogin`, `rift.OnCommand`, ... set from a package `init` function. The host calls exports right after instantiation and never runs `main`, which must exist but stays empty. Every hook has a default. |
| `registration` | `rift.Registration{Commands, Hooks}`. Leave `Hooks` nil and the SDK lists exactly the hooks you set a handler for; see [commands](#commands) and [chat and kick](#chat-and-kick). |
| `text` | `rift.Plain`, `rift.JSON` and the builders `rift.Mini`, `rift.Legacy`, `rift.Component`; see [text](#text). |
| `decision` | `rift.Continue()`, `rift.Deny(text)`, `rift.Server(name)`, `rift.Message(text)`, `rift.Queue(server)`, `rift.LeaveQueue()`; `route` answers are `rift.RouteDefault()`, `rift.RouteBackend`, `rift.RouteReject`. |
| `suggest`, `status` | `rift.OnSuggest` returns `[]rift.Suggestion{{Text, Tooltip}}`; `rift.OnStatus` returns a `*rift.StatusOverride` whose `Motd`, `Version` and `Sample` are texts, or nil. |
| `chat`, `kick` | `rift.OnChat(func(rift.ChatAttempt) rift.Decision)` and `rift.OnKick(func(rift.Context) rift.Decision)`; see [chat and kick](#chat-and-kick). |
| `dialog` | `rift.ShowDialog`, `rift.ClearDialog`; clicks on the plugin's [dialogs](dialogs.md) reach `rift.OnDialog(func(rift.DialogResponse))`, which needs no hook in the registration. |
| `players` | `rift.Players`, `rift.Player`, `rift.PlayerNames`, `rift.SendMessage`, `rift.SendActionBar`, `rift.SendTitle`, `rift.SetTabList`, `rift.SetBossBar`, `rift.Kick`, `rift.Transfer`, `rift.Broadcast`, `rift.Lookup`, `rift.Search`, `rift.SetLanguage`; see [players, languages and groups](#players-languages-and-groups). |
| `background` | `rift.Defer`, `rift.Resolve`, `rift.Run`, `rift.IsWorker` with `rift.OnWork`, `rift.OnWorkInit` and `rift.OnJobDone`; see [background work](#background-work). |
| `store` | `rift.StoreGet`, `StoreSet`, `StoreDelete`, `StoreIncrement`, `StoreList`. |
| `permissions` | `rift.HasPermission`, `SetPermission`, `AddGroup`, `RemoveGroup`, `Groups`, `ListGroupsJSON`, `Meta`, `SetMeta`. |
| `bus` | `rift.Publish`, `Reply`, `Request`, `Subscribe`, `Unsubscribe`, `StreamPublish`. |
| `servers` | `rift.ServerCreate`, `ServerRemove`, `ServerStart`, `ServerStop`, `TemplateDownload`, `Control`. |
| `timers` | `rift.Every`, `After`, `CancelTimer`. |
| `log`, `meta` | `rift.Log(level, msg)` and `Tracef` ... `Errorf`; `rift.Name`, `Version`, `DataDir`, `ConfigDir`, `Clock`, `Time`, and `rift.API()`, the contract version the host implements (`0.2.0`). |
| `result<T, string>` | `(T, error)` with a `rift.HostError`. |
| `option<T>` | Parameters: `""` for an optional string, `0` for an optional Unix time, the zero `rift.Text{}` for an optional text. Record fields: pointers (`rift.Ptr(v)` makes one) and zero texts. |
| configuration JSON | `rift.ConfigString(json, "key")` returns `(value, ok, err)` with a built-in parser; `rift.DecodeConfig(json, &cfg)` uses `encoding/json`, which adds about 850 KB. |

## Text

Everything a player reads is a `rift.Text`, the contract's
[`text`](plugin-events.md#text): plain text shown exactly as written, or a JSON
text component.

```go
rift.SendMessage(ctx.UUID, rift.Plain("Welcome back."))
rift.SendActionBar(ctx.UUID, rift.JSON(`{"text":"Lobby","color":"green"}`))
return rift.Deny(rift.Plain("You are banned."))
```

The builders write a JSON component, escaping every string, without
`encoding/json`; their `Text()` method returns the `rift.Text`:

```go
// Rift markup, written as {"mini": "...", "args": {...}}; see docs/text.md.
welcome := rift.Mini("<gold>Welcome, <rank> <player>!</gold> <gray><click:run_command:/spawn>[Spawn]").
	Arg("player", ctx.Name).                       // shown as written, never parsed
	ArgComponent("rank", rift.Legacy("&c[Admin]")) // a component argument
rift.SendMessage(ctx.UUID, welcome.Text())

// Legacy codes, written as {"legacy": "...", "marker": "&"}; .Marker('§') for another marker.
rift.SendTitle(ctx.UUID, rift.Title{
	Title:    rift.Mini("<green>Lobby").Text(),
	Subtitle: rift.Legacy("&7Have fun").Text(),
	FadeIn:   10, Stay: 70, FadeOut: 20,
})

// A component field by field (1.21.5 spellings; Rift converts for older clients).
accept := rift.Component("[Accept]").Color("green").Bold().
	RunCommand("/party accept Alex").HoverText("Join Alex's party").
	Extra(rift.Plain(" or ignore it"))
return rift.Message(accept.Text())
```

- `rift.Component` has `Color`, `Bold`, `Italic`, `Underlined`,
  `Strikethrough`, `Obfuscated`, `RunCommand`, `SuggestCommand`, `OpenURL`,
  `CopyToClipboard`, `HoverText` (plain), `Hover` (a component), `Insertion`,
  `Extra` (children) and `Field` (any other key, raw JSON).
- Builders nest through the `TextJSON` interface (`JSON() string`), which
  `rift.Text` implements too.
- The zero `rift.Text{}` is "none" where text is optional: leave out a title's
  `Subtitle`, a suggestion's `Tooltip`, a tab list footer or the texts of a
  `rift.StatusOverride`.

```go
rift.OnSuggest(func(ctx rift.Context) []rift.Suggestion {
	return []rift.Suggestion{{Text: "lobby", Tooltip: rift.Mini("<gray>The main lobby").Text()}}
})
rift.OnStatus(func(ping rift.Ping) *rift.StatusOverride {
	return &rift.StatusOverride{
		Motd:   rift.Mini("<gold>My network").Text(),
		Sample: []rift.Text{rift.Plain("§7Hello")},
	}
})
```

Rift reads markup only from a `mini` object: `rift.Plain("<red>hi")` is shown
as written. Pass player input with `Arg` or `rift.Plain`, never by
concatenating it into markup. The tags are listed in [Styled text](text.md).

## Commands

A `rift.CommandSpec` has other names in `Aliases` (at most 16), and `Console`
lets operators run the command from the console:

```go
rift.CommandSpec{
	Name: "find", Aliases: []string{"whereis"}, Permission: "find.use",
	Suggest: rift.SuggestPlayers(), Console: true,
}
```

- The hooks always see `find` in `ctx.Command`, whichever name was typed.
- For a console run `ctx.Console` is true, `ctx.Name` is `CONSOLE`, `ctx.UUID`
  is empty and `ctx.ConnectionID` is 0; a `rift.Message` decision is printed
  for the operator ([commands](plugin-events.md#commands)).
- `Suggest` is one of `rift.SuggestNone()`, `SuggestFixed(...)`,
  `SuggestPlayers()`, `SuggestServers()` and `SuggestDynamic()`, which asks
  `rift.OnSuggest`.

## Chat and kick

`rift.OnChat` handles the [`chat`](plugin-events.md#chat) event and
`rift.OnKick` the `kick` event. Rift delivers both only when the hook list
names `rift.HookChat` and `rift.HookKick`: an empty list means every hook
except these two. With `Hooks` nil the SDK lists them when the handler is set.

```go
rift.OnChat(func(attempt rift.ChatAttempt) rift.Decision {
	if attempt.Kind == rift.ChatMessage && muted[attempt.Ctx.UUID] {
		return rift.Deny(rift.Plain("You are muted."))
	}
	return rift.Continue()
})

rift.OnKick(func(ctx rift.Context) rift.Decision {
	// ctx.Server is the backend that kicked, ctx.Reason its reason.
	return rift.Server("lobby")
})
```

| Hook | Input | Answers |
| --- | --- | --- |
| `chat` | `attempt.Text` is the message, or the command line without its slash. For a command (`rift.ChatCommand`), `attempt.Ctx.Command` is the root name and `attempt.Ctx.Args` the rest | `Continue` or `Deny` |
| `kick` | `ctx.Server` and `ctx.Reason` | `Continue` (leave it to `network.on_kick`), `Deny` (disconnect with the text) or `Server` |

With signed chat, the first deny costs that player their
[chat signing](plugin-events.md#denying-costs-the-player-chat-signing).

## Background work

[Background work](plugin-events.md#background-work) explains the model and
its rules. Workers need `[background]` in the manifest.

In Go a worker is another instance of the component with its own memory and
globals. Its package `init` functions run, so the handlers are set. It then
runs the `rift.OnWorkInit` handler once, with the plugin configuration, and the
`rift.OnWork` handler once per job. The result returns to the main instance
through `rift.OnJobDone`.

```go
// Main instance: the login tickets waiting for a job, by job handle.
var pending = map[uint64]uint64{}

func init() {
	// Main instance: defer the answer and queue the lookup.
	rift.OnLogin(func(ctx rift.Context) rift.Decision {
		ticket, err := rift.Defer()
		if err != nil {
			return rift.Continue()
		}
		handle, err := rift.Run([]byte(ctx.UUID))
		if err != nil {
			rift.Resolve(ticket, rift.Continue())
		} else {
			pending[handle] = ticket
		}
		return rift.Continue() // ignored once the hook is deferred
	})

	// Worker: the slow part. payload is the UUID queued above.
	rift.OnWork(func(payload []byte) ([]byte, error) {
		banned, err := banDatabaseHas(string(payload)) // may block
		if err != nil {
			return nil, err
		}
		if banned {
			return []byte("1"), nil
		}
		return []byte("0"), nil
	})

	// Main instance: answer the deferred login.
	rift.OnJobDone(func(handle uint64, result []byte, err error) {
		ticket, ok := pending[handle]
		if !ok {
			return
		}
		delete(pending, handle)
		decision := rift.Continue()
		if err == nil && string(result) == "1" {
			decision = rift.Deny(rift.Plain("You are banned."))
		}
		rift.Resolve(ticket, decision)
	})
}
```

- `rift.Defer` works in `OnLogin`, `OnInitialServer`, `OnBeforeTransfer`,
  `OnCommand`, `OnChat` and `OnKick`.
- `rift.Resolve` returns false when the answer came after the hook's
  deadline.
- `OnJobDone` reports a failed job with a `rift.JobError`: the error `OnWork`
  returned, or the host's reason. Without an `OnWork` handler every job fails
  with `rift.ErrNoWork`.
- `rift.IsWorker()` tells the two kinds of instance apart in code they share.

## Players, languages and groups

[The player directory and languages](plugin-events.md#the-player-directory-and-languages)
describes the records and the language rules. In Go:

- `rift.Player` and `rift.Players` return `rift.PlayerInfo` with the player's
  `IP`, profile `Properties`, `VisibleTo`, `Locale` and `Language`. `Locale`
  is nil while Rift does not know it: always in `OnLogin`, and in `OnJoin` for
  clients before 1.20.2.
- `rift.Lookup` and `rift.Search` return `rift.PlayerRecord` with `UUID`,
  `Name`, `FirstSeen`, `LastSeen`, `LastIP`, `LastServer`, `Online` and
  `Language`.
- `rift.SetLanguage(target, "de_de")` sets the language of Rift's own
  messages for a player; `""` goes back to the client's locale. `ctx.Language`
  has it from `OnLogin` on.

```go
record, err := rift.Lookup("Alex") // a UUID, or the last name, case-insensitive
if err == nil && record != nil && !record.Online {
	rift.Infof("%s was last seen on %s", record.Name, record.LastServer)
}
recent, _ := rift.Search("al", 10) // by name prefix, most recently seen first, 1..=100
```

`rift.AddGroup` takes an expiry in Unix seconds, `0` for none; a new expiry
replaces an earlier one:

```go
rift.AddGroup(ctx.UUID, "vip", uint64(rift.Time())+30*24*3600) // for 30 days
rift.AddGroup(ctx.UUID, "builder", 0)                          // until removed
```

## Gotchas

- **Pin TinyGo 0.42.0.** TinyGo 0.43.0 and 0.44.0 allocate the memory the host
  writes arguments and results into (`cabi_realloc`) as manually managed memory
  that the garbage collector never reclaims, and the generated bindings never
  free it. Every hook call and every host call that passes strings or lists
  then leaks: about 3 KB per round of five hooks in the hello-go conformance
  run, against a flat 2 MB heap with 0.42.0. CI builds with 0.42.0.
- **The WASI CLI imports are always there.** TinyGo's `wasip2` target expects
  the WASI CLI import set even when a plugin uses none of it, which is why
  guests build against world `plugin`. Capabilities decide what the plugin
  can reach.
- **Panics trap.** A panic ends the call and marks the instance failed; Rift
  applies `on_failure` and restarts it per `failure.restart`. A hook still
  running at its deadline is interrupted the same way.
- **Single-threaded.** The host never calls one instance concurrently, so
  handlers need no locking. Goroutines run only while a hook runs.
- **Size.** `encoding/json` and `fmt` are large under TinyGo; prefer
  `rift.ConfigString` and plain string concatenation in small plugins.
