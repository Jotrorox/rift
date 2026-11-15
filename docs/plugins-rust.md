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
rift-plugin = "0.2.0"
serde = { version = "1", features = ["derive"] }

[profile.release]
opt-level = "s"
lto = true
codegen-units = 1
strip = true
```

[`rift-plugin`](https://crates.io/crates/rift-plugin) is versioned after the
plugin contract (`rift:plugin@0.2.0`), not the proxy. Each published version
has a `sdk/rust/v…` git tag, and in a Rift checkout
`{ path = "…/sdk/rust" }` works as well. The release profile above keeps the component small. Check the result with
`wasm-tools component wit my_plugin.wasm`: it exports
`rift:plugin/handler@0.2.0` and imports the `rift:plugin/*` interfaces it uses.

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
api = "0.2"
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
| `registration` | `Registration::new().command(...).hooks([...])`. An empty hook list delivers every hook except `Hook::Chat` and `Hook::Kick`. |
| `command-spec` | `CommandSpec::new(name).alias(..).aliases([..]).permission(node).suggest(kind).console()`; see [commands](#commands). |
| `text` | `Text::Plain(String)` or `Text::Json(String)`; everything that shows text takes `impl Into<Text>`; see [text](#text). |
| `decision` | `Action`: `Action::Continue`, `Action::deny(text)`, `Action::message(text)`, `Action::server(name)`, `Action::queue(name)`, `Action::LeaveQueue`. |
| `suggestion`, `status-override` | `Suggestion::new(text).tooltip(text)`; `StatusOverride { motd: Some(..), ..Default::default() }`. |
| `title-spec`, `boss-bar-spec` | `TitleSpec::new(title).subtitle(sub).times(10, 70, 20)`, `TitleSpec::subtitle_only(sub)`; `BossBarSpec` is the plain record, its `text` a `Text`. |
| configuration JSON | `rift_plugin::config::<T>(json)` decodes with serde. |
| `players`, `store`, `permissions`, `bus`, `servers`, `timers`, `background`, `meta` | Modules of the same names. `store::get_json`/`set_json` and `bus::reply(&message, payload)` are helpers; `meta::api()` is the host's `rift:plugin` version. |
| `log` | `log::info!` and friends: the SDK installs a logger before `init` (and before `work_init` on a worker). Also re-exported as `rift_plugin::log::info!`. |
| `result<T, string>` | `Result<T, String>` with the host's message. |
| WASI | `std::fs` on the granted directories (`/data`, `/host`), clocks and random through the standard library. |
| `players.dialog`, `dialog` hook | [Dialogs](dialogs.md): `players::show_dialog(target, &serde_json::Value)` (or `players::dialog` with JSON text) and `players::clear_dialog`; clicks reach `Plugin::dialog(DialogResponse)`, whose `values::<T>()` and `payload::<T>()` decode the JSON. No hook entry needed. |
| anything else | `rift_plugin::bindings` holds the generated bindings. |

## Text

Everything a player reads is a `Text`: `Text::Plain`, 1 to 1024 bytes shown
exactly as written, or `Text::Json`, a JSON text component of at most 32 KiB.
Functions, constructors and builders take `impl Into<Text>`:

| Value | Becomes |
| --- | --- |
| `&str`, `String` | plain text |
| `text::mini(..)`, `text::legacy(..)`, `text::Component` | JSON text |
| `serde_json::Value` | JSON text |
| `Text::json(string)` | JSON text you already hold as a string |

```rust
use rift_plugin::{Action, Suggestion, TitleSpec, players, text};

players::message(&ctx.uuid, "Shown <exactly> as written")?;

// Rift markup, written as {"mini": "...", "args": {...}}; see docs/text.md.
let welcome = text::mini("<gold>Welcome, <player>!</gold> <gray><click:run_command:/spawn>[Spawn]")
    .arg("player", &ctx.name)                                // shown as written, never parsed
    .arg_component("rank", text::legacy("&c[Admin]"));       // a component argument
players::message(&ctx.uuid, welcome)?;

// Legacy codes, written as {"legacy": "...", "marker": "&"}; `.marker('§')` for another marker.
players::title(
    &ctx.uuid,
    &TitleSpec::new(text::mini("<green>Lobby")).subtitle(text::legacy("&7Have fun")),
)?;
players::tab_list(&ctx.uuid, Some(text::mini("<gold>Rift").into()), Some("Have fun".into()))?;

// A component field by field (1.21.5 spellings; Rift converts for older clients).
let accept = text::Component::text("[Accept]")
    .color("green")
    .bold()
    .run_command("/party accept Alex")
    .hover_text("Join Alex's party")
    .extra(" or ignore it");
let reply = Action::message(accept);

let tip = Suggestion::new("lobby").tooltip(text::mini("<gray>The <aqua>main</aqua> lobby"));
```

`players::message`, `action_bar`, `kick` and `broadcast` take the text
directly; `players::tab_list` takes `Option<Text>` for header and footer (both
`None` restores the configured tab list), and `players::title` and
`players::boss_bar` take the records.

`text::Component` has `color`, `bold`, `italic`, `underlined`,
`strikethrough`, `obfuscated`, `run_command`, `suggest_command`, `open_url`,
`copy_to_clipboard`, `hover_text` (plain), `hover` (a component), `insertion`,
`extra` (a child) and `field` (any other key). Every builder implements
`Display` (its JSON) and converts into `serde_json::Value`, so builders nest.

Markup is only read from an explicit `mini` object: a string is always shown
as written, also as JSON text (`Action::message(serde_json::json!("<red>hi"))`).
Pass player input as plain text or as `arg`, never by formatting it into the
markup. The tags are listed in [Text and markup](text.md).

## Commands

```rust
Registration::new()
    .command(
        CommandSpec::new("whois")
            .aliases(["who", "seen"])
            .permission("staff.whois")
            .suggest(SuggestKind::Players)
            .console(),
    )
    .hooks([Hook::Command])
```

- Aliases are written like the name (1 to 64 bytes of `a-z0-9_-`), at most 16.
  `ctx.command` is always the name, whichever was typed.
- Without `permission`, or with an empty node, every player may see and run
  the command.
- `console()` lets operators run the command with `rift admin plugins run`.
  `ctx.console` is then true, `ctx.connection_id` is 0, `ctx.uuid` is empty
  and `ctx.name` is `CONSOLE`; a `message` answer goes to the operator.
  Commands without it never see the console.
- `ctx.language` is the language Rift's own messages use for the player.

## Chat and kick

`Plugin::chat` sees chat messages, and the commands that reach the backend,
before the backend does ([the hook](plugins.md#chat)). `Plugin::kick` decides
what happens to a player a backend kicked. Both are delivered only to a
registration that lists them; an empty hook list does not include them.

```rust
use rift_plugin::{Action, ChatAttempt, ChatKind, Context, Hook, Plugin, Registration};

#[derive(Default)]
struct Mute {
    muted: Vec<String>,
}

impl Plugin for Mute {
    fn init(&mut self, _config: &str) -> Result<Registration, String> {
        Ok(Registration::new().hooks([Hook::Chat, Hook::Kick]))
    }

    fn chat(&mut self, attempt: ChatAttempt) -> Action {
        if attempt.kind == ChatKind::Message && self.muted.contains(&attempt.ctx.uuid) {
            return Action::deny("You are muted.");
        }
        Action::Continue
    }

    fn kick(&mut self, ctx: Context) -> Action {
        match ctx.reason.as_deref() {
            Some(reason) if reason.contains("restarting") => Action::server("lobby"),
            _ => Action::Continue,
        }
    }
}

rift_plugin::export!(Mute);
```

`attempt.text` is the message, or the command line without its slash; for a
command, `attempt.ctx.command` is the root name and `attempt.ctx.args` the
rest. `chat` answers `Action::Continue` or `Action::deny`. Rift asks a plugin
that lists `Hook::Chat` about every message of every player and starts it
only when the proxy sets `authentication.signed_chat = false`.

In `kick`, `ctx.server` is the backend and `ctx.reason` its reason without
formatting. `Action::Continue` leaves the kick to `network.on_kick`,
`Action::deny` disconnects the player with the text and `Action::server`
moves them there.

## Background work

A plugin runs one call at a time, so a hook that blocks holds up every other
hook. A decision hook can defer its answer instead, hand the slow part to a
background worker and return. Workers are further instances of the same
component, enabled in the manifest:

```toml
[background]
workers = 2
```

A worker has its own memory, and its state is its own `Default` value. It
never gets `init` or any hook: only `Plugin::work_init` once, with the plugin
configuration, and then `Plugin::work` once per job. `work` may block (30 s
unless the manifest sets `deadlines.work`); its result returns to the main
instance through `Plugin::job_done`.

```rust
use rift_plugin::{Action, Context, Hook, Plugin, Registration, background};
use std::collections::HashMap;

#[derive(Default)]
struct Bans {
    /// Main instance: job handle to the ticket of the login waiting for it.
    waiting: HashMap<u64, u64>,
}

impl Plugin for Bans {
    fn init(&mut self, _config: &str) -> Result<Registration, String> {
        Ok(Registration::new().hooks([Hook::Login]))
    }

    fn login(&mut self, ctx: Context) -> Action {
        let Ok(ticket) = background::defer() else {
            return Action::deny("Try again in a moment.");
        };
        match background::run(ctx.uuid.as_bytes()) {
            Ok(job) => {
                self.waiting.insert(job, ticket);
            }
            Err(_) => {
                let _ = background::resolve(ticket, Action::deny("Try again in a moment."));
            }
        }
        Action::Continue // ignored: the answer is the one given to `resolve`
    }

    fn job_done(&mut self, job: u64, outcome: Result<Vec<u8>, String>) {
        let Some(ticket) = self.waiting.remove(&job) else { return };
        let action = match outcome {
            Ok(reason) if reason.is_empty() => Action::Continue,
            Ok(reason) => Action::deny(String::from_utf8_lossy(&reason).into_owned()),
            Err(_) => Action::deny("Try again in a moment."),
        };
        let _ = background::resolve(ticket, action);
    }

    // On a worker: the slow lookup. An empty answer means "not banned".
    fn work(&mut self, uuid: Vec<u8>) -> Result<Vec<u8>, String> {
        let uuid = String::from_utf8(uuid).map_err(|error| error.to_string())?;
        Ok(std::fs::read(format!("/data/bans/{uuid}")).unwrap_or_default())
    }
}

rift_plugin::export!(Bans);
```

- `background::defer` works in `login`, `initial_server`, `before_transfer`,
  `command`, `chat` and `kick`, once per call. A hook that does not defer
  answers with its return value as usual.
- The hook's deadline keeps running while the job does. An answer after it
  counts as a missed deadline (`on_timeout`), and `background::resolve` then
  returns `Ok(false)`.
- `background::run` fails without workers in the manifest and when 1024 jobs
  wait already. `job_done` gets `Err` with the error `work` returned, or the
  host's reason. Without a `work` implementation every job fails with `this
  plugin has no background work`.
- `background::is_worker()` tells the two kinds of instance apart in code they
  share. `job_done` needs no entry in the hook list.

## Players, languages and groups

`players::get` and `players::list` return `PlayerInfo` with the player's
`ip`, profile `properties`, `visible_to`, the client's `locale` and the
`language` Rift's own messages use for them. `locale` is `None` while Rift
does not know it: the client sends it after login, so it is unset in `login`
and, for clients before 1.20.2, in `join`.

`players::lookup` and `players::search` read what Rift remembers about every
player who joined at least once, online or not:

```rust
// A UUID, or the last name, case-insensitive.
if let Some(record) = players::lookup("Alex")? {
    if !record.online {
        log::info!("{} was last seen on {}", record.name, record.last_server);
    }
}
let recent = players::search("al", 10)?; // by name prefix, most recently seen first, 1..=100
```

A `PlayerRecord` has `uuid`, `name`, `first_seen` and `last_seen` (Unix
seconds), `last_ip` (empty when `players.record_ip` is off), `last_server`,
`online` and `language`.

`players::set_language(target, Some("de_de"))` sets the language of Rift's own
messages for a player, online or not (a tag of 2..=16 bytes of `a-z0-9_`);
`None` goes back to the client's locale. It returns whether the player is
known, and is remembered across sessions, so `ctx.language` has it from
`login` on.

`permissions::add_group` takes an optional expiry in Unix seconds; a new
expiry replaces an earlier one:

```rust
let month = meta::time() as u64 + 30 * 24 * 3600;
permissions::add_group(&ctx.uuid, "vip", Some(month))?; // for 30 days
permissions::add_group(&ctx.uuid, "builder", None)?;    // until removed
```

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
  state half-changed. Keep hooks short and move slow work to
  [background workers](#background-work).
- **Single-threaded.** Do not spawn threads; the host never calls one instance
  concurrently, so `&mut self` needs no locking.
- **List your hooks.** `Registration::hooks` should name exactly the hooks you
  implement; an empty list delivers every hook except `chat` and `kick`,
  which costs a call per event.
- **No `wasi:http` helper.** Use `rift.servers`/`bus` for Rift work; outbound
  HTTP needs `network = true` and a `wasi:http` client crate.
