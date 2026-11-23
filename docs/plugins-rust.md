# Rust plugins

A Rust plugin is an ordinary `cdylib` crate built for `wasm32-wasip2` with the
SDK in [`sdk/rust`](../sdk/rust) (crate `rift-plugin`). This guide shows how
the plugin contract looks in Rust. [Plugins](plugins.md) covers enabling and
trust; [Plugin events and commands](plugin-events.md) says what each event and
answer means.

## Build

```sh
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/my_plugin.wasm plugins/my-plugin/
```

The SDK ships the contract's WIT and generates the bindings, and Rust's linker
for `wasm32-wasip2` emits a WebAssembly component directly: no
`cargo component` and no `wasm-tools component new`.

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
rift-plugin = "0.3.0"
serde = { version = "1", features = ["derive"] }

[profile.release]
opt-level = "s"
lto = true
codegen-units = 1
strip = true
```

- [`rift-plugin`](https://crates.io/crates/rift-plugin) is versioned after the
  plugin contract (`rift:plugin@0.3.0`), not the proxy. Each published version
  has a `sdk/rust/v…` git tag. In a Rift checkout `{ path = "…/sdk/rust" }`
  works as well.
- The release profile keeps the component small.
- Check the result with `wasm-tools component wit my_plugin.wasm`: it exports
  `rift:plugin/handler@0.3.0` and imports the `rift:plugin/*` interfaces it
  uses.

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
api = "0.3"
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
| `players`, `store`, `permissions`, `bus`, `servers`, `timers`, `background`, `meta`, `text` | Modules of the same names. `store::get_json`/`set_json` and `bus::reply(&message, payload)` are helpers; `meta::api()` is the host's `rift:plugin` version. |
| `log` | `log::info!` and friends: the SDK installs a logger before `init` (and before `work_init` on a worker). Also re-exported as `rift_plugin::log::info!`. |
| `result<T, string>` | `Result<T, String>` with the host's message. |
| WASI | `std::fs` on the granted directories (`/data`, `/host`), clocks and random through the standard library. |
| `players.dialog`, `dialog` hook | [Dialogs](dialogs.md): `players::show_dialog(target, &serde_json::Value)` (or `players::dialog` with JSON text) and `players::clear_dialog`; clicks reach `Plugin::dialog(DialogResponse)`, whose `values::<T>()` and `payload::<T>()` decode the JSON. No hook entry needed. |
| `players.menu`, `menu` hook | [Menus](menus.md#from-rust-go-and-c): `players::show_menu(target, &Menu)` (or `players::menu` with JSON text), `players::update_menu(target, id, &MenuPatch)` (or `update_menu_json`) and `players::close_menu(target, Some(id))`, where `None` closes whatever menu is open. The `menu` module builds menus, buttons and patches. Events reach `Plugin::menu(MenuEvent)`: `kind()` is a `MenuKind` (`Click { action, slot, click, key }`, `Text { text }`, `Close { reason }` or `Other`), and `payload::<T>()` and `state::<T>()` decode the JSON. No hook entry needed. |
| anything else | `rift_plugin::bindings` holds the generated bindings. |

## Text

Everything a player reads is a [`Text`](plugin-events.md#text): `Text::Plain`,
shown exactly as written, or `Text::Json`, a JSON text component. Functions,
constructors and builders take `impl Into<Text>`:

| Value | Becomes |
| --- | --- |
| `&str`, `String` | plain text |
| `text::mini(..)`, `text::legacy(..)`, `text::message(..)`, `text::Component` | JSON text |
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

// A style argument wraps what the template puts inside <team>...</team>.
let tag = text::mini("<team>[<tag>]</team> <name>")
    .arg_style("team", "<#55ffff><bold>")
    .arg("tag", "RED")
    .arg("name", &ctx.name);

// A message from the plugin's messages.toml, written as {"message": "...", "args": {...}}.
players::message(&ctx.uuid, text::message("invited").arg("from", "Alex"))?;

// Text you splice into markup yourself: `<` becomes `\<` and `\` becomes `\\`.
let safe = text::mini(format!("<gray>{}", text::escape(&motd_line)));

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

// Other contents, styles and events.
let joined = text::Component::translate("multiplayer.player.joined")
    .with(text::Component::text("Alex").color("yellow"))
    .fallback("Alex joined");
let head = text::Component::head("Alex").hat(false).hover_entity("minecraft:player", &uuid);
let diamond = text::Component::sprite("item/diamond")
    .shadow("#000000", 0.5)
    .hover_item("minecraft:diamond", 3);
```

- `players::message`, `action_bar`, `kick` and `broadcast` take the text
  directly. `players::tab_list` takes `Option<Text>` for header and footer
  (both `None` restores the configured tab list). `players::title` and
  `players::boss_bar` take the records.
- `text::mini` takes `arg` (shown as written), `arg_component` and
  `arg_style` (a [style argument](text.md#style-arguments): markup made only of formatting
  tags). `text::message(key)` takes the same three. A message comes from the
  plugin's `messages.toml`; see [plugin messages](text.md#plugin-messages).
- `text::escape(&str)` escapes text for splicing into markup. An argument
  needs no escaping.
- `text::render(text, language)`, `text::plain`, `text::to_legacy` and
  `text::to_mini` (the contract's `text` interface) ask Rift to convert text, with
  markup, legacy codes and the plugin's messages resolved in `language`
  (`Some("de")`) or the default. They take `impl Into<Text>` and return
  `Result<String, String>`: compact JSON, the words, `§` codes or markup.
  Use them for text that leaves through a bus payload, an HTTP response or
  a log line, and in `init` to check a template from the configuration:

  ```rust
  // In init: a bad template in the configuration fails the start.
  text::render(text::mini(&config.greeting), None)?;
  // The plugin's own message `created` without formatting, in German.
  let line = text::plain(text::message("created"), Some("de"))?;
  ```
- `text::Component` builds a component:

  | Kind | Methods |
  | --- | --- |
  | Content | `text(s)`, `translate(key)` with `with(arg)` and `fallback(s)`, `keybind(key)`, `sprite(sprite)` with `atlas(atlas)`, `head(name)` with `hat(false)` |
  | Style | `color`, `bold`, `italic`, `underlined`, `strikethrough`, `obfuscated`, `decoration(name, on)` (`decoration("italic", false)` turns it off), `font(id)`, `shadow("#rrggbb", opacity)`, `insertion` |
  | Click | `run_command`, `suggest_command`, `open_url`, `copy_to_clipboard`, `change_page(page)` |
  | Hover | `hover_text` (plain), `hover` (a component), `hover_item(id, count)`, `hover_entity(type, uuid)`, `hover_entity_named(type, uuid, name)` |
  | Other | `extra` (a child), `field(key, value)` (any other key) |

  Sprites and heads need 1.21.9 or newer; older clients drop them.
- Every builder implements `Display` (its JSON) and converts into
  `serde_json::Value`, so builders nest.

Rift reads markup only from an explicit `mini` object. A string is always
shown as written, also as JSON text
(`Action::message(serde_json::json!("<red>hi"))`). Pass player input as plain
text or as `arg`, never by formatting it into the markup. The tags are listed
in [Styled text](text.md).

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

[Commands](plugin-events.md#commands) has the rules for names, aliases and
console runs. In Rust:

- `aliases` takes at most 16 names. `ctx.command` is always the command's own
  name, whichever was typed.
- Without `permission`, or with an empty node, every player may see and run
  the command.
- `console()` lets operators run the command with `rift admin plugins run`.
  `ctx.console` is then true, `ctx.connection_id` is 0, `ctx.uuid` is empty
  and `ctx.name` is `CONSOLE`; a `message` answer goes to the operator.
- `ctx.language` is the language Rift's own messages use for the player.

## Chat and kick

`Plugin::chat` handles the [`chat`](plugin-events.md#chat) event and
`Plugin::kick` the `kick` event. Rift delivers both only to a registration that
lists them; an empty hook list does not include them. With signed chat, the
first deny costs that player their
[chat signing](plugin-events.md#denying-costs-the-player-chat-signing).

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

| Hook | Input | Answers |
| --- | --- | --- |
| `chat` | `attempt.text` is the message, or the command line without its slash. For a command, `attempt.ctx.command` is the root name and `attempt.ctx.args` the rest | `Action::Continue` or `Action::deny` |
| `kick` | `ctx.server` is the backend and `ctx.reason` its reason without formatting | `Action::Continue` leaves the kick to `network.on_kick`, `Action::deny` disconnects the player with the text, `Action::server` moves them there |

## Background work

[Background work](plugin-events.md#background-work) explains the model and
its rules. Workers need `[background]` in the manifest.

In Rust a worker is another instance of the component whose state is its own
`Default` value. It runs `Plugin::work_init` once, with the plugin
configuration, and then `Plugin::work` once per job. The result returns to the
main instance through `Plugin::job_done`.

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

- `background::resolve` returns `Ok(false)` when the answer came after the
  hook's deadline.
- `Plugin::job_done` gets `Err` with the error `work` returned, or the host's
  reason. Without a `work` implementation every job fails with
  `this plugin has no background work`.
- `background::is_worker()` tells the two kinds of instance apart in code they
  share.

## Players, languages and groups

[The player directory and languages](plugin-events.md#the-player-directory-and-languages)
describes the records and the language rules. In Rust:

- `players::get` and `players::list` return `PlayerInfo` with the player's
  `ip`, profile `properties`, `visible_to`, `locale` and `language`. `locale`
  is `None` while Rift does not know it: always in `login`, and in `join` for
  clients before 1.20.2.
- `players::lookup` and `players::search` return `PlayerRecord` with `uuid`,
  `name`, `first_seen`, `last_seen`, `last_ip`, `last_server`, `online` and
  `language`.
- `players::set_language(target, Some("de_de"))` returns whether the player
  is known; `None` goes back to the client's locale.

```rust
// A UUID, or the last name, case-insensitive.
if let Some(record) = players::lookup("Alex")? {
    if !record.online {
        log::info!("{} was last seen on {}", record.name, record.last_server);
    }
}
let recent = players::search("al", 10)?; // by name prefix, most recently seen first, 1..=100
```

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
  version Rust records. Rift links them semver-compatibly, so this needs no
  action. A host that matched WASI versions exactly would refuse the
  component.
- **Panics trap.** A panic ends the call and marks the instance failed; Rift
  applies `on_failure` to that call and restarts the instance according to
  `failure.restart`. Return errors from `init` and `reload` instead.
- **Deadlines interrupt.** Guest code still running at its deadline is
  interrupted and the instance is restarted, because it may have left its
  state half-changed. Move slow work to
  [background workers](#background-work).
- **Single-threaded.** Do not spawn threads; the host never calls one instance
  concurrently, so `&mut self` needs no locking.
- **List your hooks.** `Registration::hooks` should name exactly the hooks you
  implement. An empty list costs a call per event.
- **No `wasi:http` helper.** Use `rift.servers`/`bus` for Rift work; outbound
  HTTP needs `network = true` and a `wasi:http` client crate.
