# Rust plugins

A Rust plugin is an ordinary `cdylib` crate built for `wasm32-wasip2` with the
SDK in [`sdk/rust`](../sdk/rust) (crate `rift-plugin`). The SDK ships the
contract's WIT and generates the bindings, and Rust's linker for
`wasm32-wasip2` emits a WebAssembly component directly: no `cargo component`
and no `wasm-tools component new`. Read the [plugin overview](plugins.md) for
how plugins are enabled, dispatched and restarted.

## Build

```sh
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/my_plugin.wasm plugins/my-plugin/
```

`Cargo.toml`:

```toml
[package]
name = "my-plugin"
version = "0.1.0"
edition = "2024"

# Keep the plugin out of any surrounding workspace.
[workspace]

[lib]
crate-type = ["cdylib"]

[dependencies]
rift-plugin = { path = "../rift/sdk/rust" } # sdk/rust in a Rift checkout
serde = { version = "1", features = ["derive"] }

[profile.release]
opt-level = "s"
lto = true
codegen-units = 1
strip = true
```

The release profile above keeps the component small. Check the result with
`wasm-tools component wit my_plugin.wasm`: it exports
`rift:plugin/handler@0.1.x` and imports the `rift:plugin/*` interfaces it uses.

## A minimal plugin

```rust
use rift_plugin::{Action, CommandSpec, Context, Hook, Plugin, Registration, SuggestKind};
use serde::Deserialize;

#[derive(Default, Deserialize)]
struct Config {
    #[serde(default)]
    deny: Option<String>,
}

#[derive(Default)]
struct Hello {
    deny: Option<String>,
}

impl Plugin for Hello {
    fn init(&mut self, config: &str) -> Result<Registration, String> {
        let config: Config = rift_plugin::config(config)?;
        self.deny = config.deny;
        Ok(Registration::new()
            .command(CommandSpec::new("hello").permission("hello.use").suggest(SuggestKind::Players))
            .hooks([Hook::Login, Hook::Command]))
    }

    fn login(&mut self, ctx: Context) -> Action {
        match &self.deny {
            Some(name) if *name == ctx.name => Action::deny("You are not welcome here."),
            _ => Action::Continue,
        }
    }

    fn command(&mut self, ctx: Context) -> Action {
        Action::message(format!("Hello, {}!", ctx.name))
    }
}

rift_plugin::export!(Hello);
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
[`examples/plugins/hello-rs`](../examples/plugins/hello-rs) is the complete
example and the host's conformance fixture.

## Mapping the contract

| Contract | Rust |
| --- | --- |
| `handler` exports | Methods of the `Plugin` trait; each has a default. `rift_plugin::export!(T)` exports a `T: Plugin + Default`. |
| plugin state | Fields of your type. The SDK creates one value with `Default` right before `init` and passes it to every hook as `&mut self`; `shutdown` runs last. |
| `registration` | `Registration::new().command(...).hooks([...])`; `CommandSpec::new(name).permission(node).suggest(kind)`; without `permission`, or with an empty node, every player may see and run the command (since 0.1.5). |
| `action` | `Action::Continue`, `Action::deny(..)`, `Action::deny_json(..)`, `Action::message(..)`, `Action::message_json(..)`, `Action::server(..)`, `Action::Queue(..)`, `Action::LeaveQueue`. The `_json` forms take a `serde_json::Value` or JSON text ([rich text](extensions.md#rich-text)). |
| configuration JSON | `rift_plugin::config::<T>(json)` decodes with serde. |
| `players`, `store`, `permissions`, `bus`, `servers`, `timers`, `meta` | Modules of the same names with the generated functions. `store::get_json`/`set_json` and `bus::reply(&message, payload)` are helpers. |
| `log` | `log::info!` and friends: the SDK installs a logger before `init`. Also re-exported as `rift_plugin::log::info!`. |
| `result<T, string>` | `Result<T, String>` with the host's message. |
| WASI | `std::fs` on the granted directories (`/data`, `/host`), clocks and random through the standard library. |
| `dialogs` export, `players.dialog` | [Dialogs](dialogs.md): `players::show_dialog(target, &serde_json::Value)` (or `players::dialog` with JSON text) and `players::clear_dialog`; clicks reach `Plugin::dialog(DialogResponse)`, whose `values::<T>()` and `payload::<T>()` decode the JSON. No registration needed. |
| `players.title-json` | `players::title_json(target, &TitleJsonSpec::new(title).subtitle(sub).times(10, 70, 20))` (since 0.1.4); title and subtitle take anything `ToString`, such as a `text` builder or a `serde_json::Value`. |
| `players.locale` | `players::locale(target) -> Result<Option<String>, String>` (since 0.1.5): the client's language (`en_us`), or `None` while Rift does not know it, which is always the case in `login` and, for clients before 1.20.2, in `join` ([timing](plugins.md#host-api)). |
| `styled` export | `Plugin::suggest_rich(ctx) -> Vec<RichSuggestion>` and `Plugin::status_rich(ping) -> Option<RichStatusOverride>` (since 0.1.4). Their defaults call `suggest` and `status`; hosts before 0.1.4 get the rich text's words. |
| rich text | The `text` module: `text::mini`, `text::legacy`, `text::Component` (below). |

## Rich text builders

The `-json` functions and answers take a JSON text component. The `text`
module writes them, with every string escaped:

```rust
use rift_plugin::{Action, RichSuggestion, TitleJsonSpec, players, text};

// Rift markup, written as {"mini": "...", "args": {...}}; see docs/text.md.
let welcome = text::mini("<gold>Welcome, <player>!</gold> <gray><click:run_command:/spawn>[Spawn]")
    .arg("player", &ctx.name)                                // shown as written, never parsed
    .arg_component("rank", text::legacy("&c[Admin]"));       // a component argument
players::message_json(&ctx.uuid, &welcome.to_string())?;

// Legacy codes, written as {"legacy": "...", "marker": "&"}; `.marker('§')` for another marker.
players::title_json(
    &ctx.uuid,
    &TitleJsonSpec::new(text::mini("<green>Lobby")).subtitle(text::legacy("&7Have fun")),
)?;

// A component field by field (1.21.5 spellings; Rift converts for older clients).
let accept = text::Component::text("[Accept]")
    .color("green")
    .bold()
    .run_command("/party accept Alex")
    .hover_text("Join Alex's party")
    .extra(" or ignore it");
let reply = Action::message_json(accept);

// Rich tooltips through the `styled` export.
let tip = RichSuggestion::new("lobby").tooltip(text::mini("<gray>The <aqua>main</aqua> lobby"));
```

`text::Component` has `color`, `bold`, `italic`, `underlined`,
`strikethrough`, `obfuscated`, `run_command`, `suggest_command`, `open_url`,
`copy_to_clipboard`, `hover_text` (plain), `hover` (a component), `insertion`,
`extra` (a child) and `field` (any other key). Every builder implements
`Display` (its JSON) and converts into `serde_json::Value`, so builders nest.

Markup is only read from an explicit `mini` object: a bare string, as in
`Action::message_json("<red>hi")`, is shown as written. Pass player input as
`arg`, never by formatting it into the markup. The tags are listed in
[Text and markup](text.md).

## Gotchas

- **Rust's standard library imports WASI `@0.2.9`.** The component imports a
  few WASI 0.2 interfaces (stdio, environment, exit, clocks) at the patch
  version Rust records, and Rift links them semver-compatibly against its WASI
  0.2 implementation, so this needs no action. A host that matched WASI
  versions exactly would refuse the component.
- **Panics trap.** A panic ends the call and marks the instance failed; Rift
  applies `on_failure` to that call and restarts the instance according to
  `failure.restart`. Return errors from `init` and `reload` instead of
  panicking.
- **Deadlines interrupt.** Guest code still running at its deadline is
  interrupted and the instance is restarted, because it may have left its
  state half-changed. Keep hooks short and do slow work from timers.
- **Single-threaded.** Do not spawn threads; the host never calls one instance
  concurrently, so `&mut self` needs no locking.
- **List your hooks.** `Registration::hooks` should name exactly the hooks you
  implement; an empty list delivers every hook, which costs a call per event.
- **No `wasi:http` helper.** Use `rift.servers`/`bus` for Rift work; outbound
  HTTP needs `network = true` and a `wasi:http` client crate.
