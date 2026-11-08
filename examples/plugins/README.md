# WebAssembly plugin examples

Each example is a standalone project that builds a WebAssembly component
against the `plugin` world of `rift:plugin@0.1.0` (`wit/rift-plugin.wit`).
A plugin directory holds a `plugin.toml` manifest and the built `.wasm` it
names in `entry`.

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

`hello-rs` doubles as the host's conformance fixture, and the C and Go
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
