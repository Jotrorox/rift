# Plugin examples

Four persistent plugins that behave identically, one per supported language:

| Example | Engine | Build | Guide |
| --- | --- | --- | --- |
| [`hello-rs`](hello-rs) | WebAssembly | `cargo build --release --target wasm32-wasip2` | [Rust plugins](../../docs/plugins-rust.md) |
| [`hello-c`](hello-c) | WebAssembly | `make WASI_SDK_PATH=/path/to/wasi-sdk` | [C plugins](../../docs/plugins-c.md) |
| [`hello-go`](hello-go) | WebAssembly | `make TINYGO=/path/to/tinygo` (TinyGo 0.42.0) | [Go plugins](../../docs/plugins-go.md) |
| [`hello-lua`](hello-lua) | Lua | nothing to build | [Lua plugins](../../docs/plugins-lua.md) |

The WebAssembly examples are standalone projects that build a component
against the `plugin` world of `rift:plugin@0.1.1` (`wit/rift-plugin.wit`). A
plugin directory holds a `plugin.toml` manifest and the entry it names: the
built `.wasm`, or `init.lua`.

To try one, copy its folder (with the built `.wasm`) to `plugins/<name>/` next
to `rift.lua` and enable it:

```lua
rift.plugin("hello-rs", { deny = "Mallory" })
```

`rift check` then lists it with its capabilities; `rift admin plugins list`
and the dashboard's Plugins page show it running. See the
[plugin overview](../../docs/plugins.md) for the manifest, lifecycle and trust
model.

## hello-rs (Rust)

Built with the Rust SDK in [`sdk/rust`](../../sdk/rust) (crate `rift-plugin`).
Plugin authors need only `rustup target add wasm32-wasip2`; the SDK ships the
WIT and generates the bindings, and Rust's linker for `wasm32-wasip2` emits a
component directly (no `cargo component`, no `wasm-tools component new`).

```sh
rustup target add wasm32-wasip2
cd examples/plugins/hello-rs
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/hello_rs.wasm .
```

The directory then holds `plugin.toml` and `hello_rs.wasm`, ready to load.

### Behaviour

`hello-rs` doubles as the host's conformance fixture, and the C, Go and Lua
examples behave identically. Every observable effect is deterministic:

| Export | Behaviour |
| --- | --- |
| `init(config)` | Decodes the JSON configuration `{"deny": "<name>"}` (`deny` optional; invalid JSON fails `init`). Calls `timers.every(1000)` once, then `bus.subscribe("hello.ping", none)` (a failed subscription fails `init`). Returns command `hello` with permission `hello.use` and suggest `players`, and hooks `login`, `command`, `join`, `message`, `timer`, so the host skips every other hook. |
| `login(ctx)` | `deny("You are not welcome here.")` when `ctx.name` equals the configured `deny` name exactly, otherwise `continue`. |
| `command(ctx)` | `message("Hello, <ctx.name>!")`. |
| `join(ctx)` | `players.message(ctx.uuid, "Welcome, <ctx.name>!")`, then `store.increment("hello", "joins", 1)`. Host errors are ignored. |
| `message(msg)` | When `msg.reply` is set, `bus.publish(msg.reply, msg.payload, none)`; otherwise nothing. Errors are ignored. |
| `timer(handle)` | Logs `tick` at `debug`. |

Every other export keeps the SDK default (`continue`, `default`, `none` or an
empty list), and the plugin logs nothing else.

Example configuration: `{"deny":"Mallory"}`.

### Manifest

```toml
name = "hello-rs"
version = "0.1.0"
api = "0.1"
mode = "persistent"
engine = "wasm"
entry = "hello_rs.wasm"
hot_reload = false

[failure]
on_timeout = "deny"
on_failure = "deny"
restart = "on-failure"

[capabilities]
filesystem = ["data"]
network = false
```

## hello-c (C)

Built with [wasi-sdk](https://github.com/WebAssembly/wasi-sdk/releases) 27 or
newer and the C SDK in [`sdk/c`](../../sdk/c):

```sh
cd examples/plugins/hello-c
make WASI_SDK_PATH=/path/to/wasi-sdk
```

or with CMake:

```sh
cmake -B build -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_TOOLCHAIN_FILE=$WASI_SDK_PATH/share/cmake/wasi-sdk-p2.cmake
cmake --build build && cp build/hello_c.wasm .
```

The directory then holds `plugin.toml` and `hello_c.wasm`. The manifest is
`hello-rs`'s with `name = "hello-c"` and `entry = "hello_c.wasm"`.

## hello-go (Go)

Built with [TinyGo](https://tinygo.org) **0.42.0** (newer releases leak memory
on every call; see [Go plugins](../../docs/plugins-go.md)), `wasm-tools` and
`wasm-opt` on `PATH`, and the Go SDK in [`sdk/go`](../../sdk/go):

```sh
cd examples/plugins/hello-go
make TINYGO=/path/to/tinygo
# which runs:
tinygo build -target=wasip2 --wit-package ../../../wit --wit-world plugin -no-debug -o hello_go.wasm .
```

The directory then holds `plugin.toml` and `hello_go.wasm`. The manifest is
`hello-rs`'s with `name = "hello-go"` and `entry = "hello_go.wasm"`.

## hello-lua (Lua)

A persistent Lua plugin: `plugin.toml` with `engine = "lua"` and the entry
script `init.lua`, which Rift runs in its own LuaJIT state. There is nothing to
build; copy the folder. Each handler body in `init.lua` would also work
unchanged in `rift.lua`.

## Writing a Rust plugin

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
rift-plugin = "0.1"
```

```rust
use rift_plugin::{Action, Context, Hook, Plugin, Registration};

#[derive(Default)]
struct MyPlugin {
    // State lives here: the SDK keeps one instance, created with Default
    // right before init, and passes it to every hook as &mut self.
}

impl Plugin for MyPlugin {
    fn init(&mut self, _config: &str) -> Result<Registration, String> {
        Ok(Registration::new().hooks([Hook::Login]))
    }

    fn login(&mut self, _ctx: Context) -> Action {
        Action::Continue
    }
}

rift_plugin::export!(MyPlugin);
```

Every hook has a default; implement only the ones you list in
`Registration::hooks`. The host API lives in `rift_plugin::{players, store,
permissions, bus, servers, timers, log, meta}`, `rift_plugin::config::<T>`
decodes the JSON configuration with serde, and `log::info!` (or
`rift_plugin::log::info!`) writes to Rift's log.

The built component also imports a few WASI 0.2 interfaces that Rust's
standard library uses (stdio, environment, exit, clocks). Rust 1.98's
standard library records them as `@0.2.9`; hosts link them through semver
matching against any WASI 0.2 implementation.

## How Rift runs a WebAssembly plugin

Rift runs components on Wasmtime with Cranelift, one instance per plugin on
the plugin's own thread, one call at a time. The host links Rift's
interfaces, the whole WASI 0.2 command import set and `wasi:http`, so Rust,
C and TinyGo components load alike.

**Capabilities.** `[capabilities]` decides what the instance sees; `rift
check` prints the effective set. The default is `filesystem = ["data"]`,
`network = true` and the whole environment.

| Grant | Inside the plugin |
| --- | --- |
| `filesystem = ["data"]` | The plugin's `data/` folder as `/data` (created if missing; `meta.data-dir`). |
| `filesystem = ["host"]` | The configuration directory as `/host` (`meta.config-dir`), read-only. |
| `filesystem = ["/srv/x"]` | That absolute path at the same path, read-write. It may not contain or lie inside `local/plugincache` or `plugins/`, so no plugin can change code Rift runs. |
| `network = true` | TCP, UDP and DNS through `wasi:sockets`; outbound HTTP through `wasi:http`. Without it every socket is refused and `wasi:http` requests fail with `HTTP-request-denied`. |
| `env = ["NAME"]` | Only the listed variables that are set; omit `env` to inherit Rift's environment. |

All directories are read-write and nothing else of the host is visible.
Stdin is empty; stdout and stderr go to Rift's log line by line as
`rift: plugin <name>: stdout: ...` (stderr at `warn`).

**Limits.** An optional `[wasm]` section caps each instance's memory:

```toml
[wasm]
memory_limit_mb = 64   # 1..=4096; 0 or absent keeps WebAssembly's 4 GiB bound
```

`[deadlines]` bound the awaited hooks, `init`, `reload` and `shutdown`. A
guest still running at its deadline is interrupted (within about 10 ms) and
the instance counts as failed: Rift discards it and restarts it according to
`[failure] restart`, because the interrupted code may have left its state
half-changed. A trap, such as a Rust panic, does the same. `init` or `reload`
returning an error is an ordinary failure that interrupts nothing. Observers
(`join`, `after_transfer`, `disconnect`, `message`, `timer`, `event`) are not
awaited, but a guest still running one after 60 s is interrupted the same
way, as in Lua; stopping or reloading Rift also interrupts a guest that does
not return within the shutdown grace period.

**Compilation.** The first start compiles the component and stores the result
in `local/plugincache/<name>-<key>.cwasm` under the configuration directory;
later starts with the same component, Rift version and engine settings load
it instead. `rift check --strict` compiles every WebAssembly plugin ahead of
time, checks that its imports and exports fit this Rift, and fills the same
cache. Each file carries an HMAC-SHA256 under a per-install secret,
`local/plugincache/key` (32 random bytes, mode 0600, created on first use);
Rift loads only files with a valid MAC and compiles again on any mismatch.
A key file that is not a private regular file of Rift's user disables the
cache (remove it to start over), so run `rift check --strict` as the same
user as Rift.

Rift built without its default `wasm` Cargo feature has no WebAssembly
support: `rift check` warns about such plugins and startup refuses them.
