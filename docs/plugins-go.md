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
| `action` | `rift.Continue()`, `rift.Deny`, `rift.Server`, `rift.Message`, `rift.Queue`, `rift.LeaveQueue()`; `route` answers are `rift.RouteDefault()`, `rift.RouteBackend`, `rift.RouteReject`. |
| `players` | `rift.Players`, `rift.Player`, `rift.PlayerNames`, `rift.SendMessage`, `rift.SendActionBar`, `rift.SendTitle`, `rift.SetTabList`, `rift.SetBossBar`, `rift.Kick`, `rift.Transfer`, `rift.Broadcast`. |
| `store` | `rift.StoreGet`, `StoreSet`, `StoreDelete`, `StoreIncrement`, `StoreList`. |
| `permissions` | `rift.HasPermission`, `SetPermission`, `AddGroup`, `RemoveGroup`, `Groups`, `ListGroupsJSON`. |
| `bus` | `rift.Publish`, `Reply`, `Request`, `Subscribe`, `Unsubscribe`, `StreamPublish`. |
| `servers` | `rift.ServerCreate`, `ServerRemove`, `ServerStart`, `ServerStop`, `TemplateDownload`, `Control`. |
| `timers` | `rift.Every`, `After`, `CancelTimer`. |
| `log`, `meta` | `rift.Log(level, msg)` and `Tracef` ... `Errorf`; `rift.Name`, `Version`, `DataDir`, `ConfigDir`, `Clock`, `Time`. |
| `result<T, string>` | `(T, error)` with a `rift.HostError`. |
| configuration JSON | `rift.ConfigString(json, "key")` returns `(value, ok, err)` with a built-in parser; `rift.DecodeConfig(json, &cfg)` uses `encoding/json`, which adds about 850 KB. |

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
