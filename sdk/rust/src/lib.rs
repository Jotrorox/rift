//! Guest SDK for Rift WebAssembly plugins.
//!
//! A Rift plugin is a WebAssembly component built for `wasm32-wasip2` against
//! the `plugin` world of `rift:plugin@0.2.1`. This crate ships that WIT,
//! generates the bindings and wraps them, so a plugin is an ordinary
//! `cdylib` crate:
//!
//! ```toml
//! [lib]
//! crate-type = ["cdylib"]
//!
//! [dependencies]
//! rift-plugin = "0.2"
//! ```
//!
//! ```no_run
//! use rift_plugin::{Action, CommandSpec, Context, Hook, Plugin, Registration, SuggestKind};
//!
//! #[derive(Default)]
//! struct Greeter;
//!
//! impl Plugin for Greeter {
//!     fn init(&mut self, _config: &str) -> Result<Registration, String> {
//!         Ok(Registration::new()
//!             .command(
//!                 CommandSpec::new("greet")
//!                     .alias("hi")
//!                     .permission("greeter.use")
//!                     .suggest(SuggestKind::Players),
//!             )
//!             .hooks([Hook::Command]))
//!     }
//!
//!     fn command(&mut self, ctx: Context) -> Action {
//!         Action::message(format!("Hi, {}!", ctx.name))
//!     }
//! }
//!
//! rift_plugin::export!(Greeter);
//! ```
//!
//! Build it with `cargo build --release --target wasm32-wasip2`. Rust's
//! linker for that target emits a component directly; no `cargo component`
//! or `wasm-tools component new` step is needed.
//!
//! # State
//!
//! The component model calls exports as free functions, so the SDK keeps the
//! single plugin instance for you: [`export!`] stores one value of your type
//! in a thread-local, created with [`Default`] right before
//! [`Plugin::init`], and every hook receives it as `&mut self`. Keep plugin
//! state in your struct's fields. This is sound because guests are
//! single-threaded and the host never calls one instance concurrently or
//! re-entrantly. [`Plugin::shutdown`] runs last and the instance is dropped
//! after it returns.
//!
//! # Hooks
//!
//! Every [`Plugin`] method has a default that does nothing (`continue`,
//! `default`, `none` or an empty list). List the hooks you implement in
//! [`Registration::hooks`] so the host skips the rest. An empty list delivers
//! every hook except [`Hook::Chat`] and [`Hook::Kick`], which are only ever
//! delivered when listed. [`Plugin::dialog`] and [`Plugin::job_done`] need no
//! entry.
//!
//! The decision hooks answer with an [`Action`]; each accepts a subset:
//!
//! | Hook | Accepts |
//! | --- | --- |
//! | `login`, `before_transfer`, `chat` | `Continue`, `deny` |
//! | `initial_server`, `kick` | `Continue`, `deny`, `server` |
//! | `command` | `Continue`, `message`, `server`, `queue`, `LeaveQueue` |
//!
//! # Host API
//!
//! [`players`], [`store`], [`permissions`], [`bus`], [`servers`], [`timers`],
//! [`background`], [`log`], [`meta`] and [`text`] mirror the WIT interfaces of
//! the same names. Errors are the host's message as a `String`. [`log`] also installs
//! a [`::log`] logger before `init`, so `log::info!` and friends reach Rift's
//! log. Plugin configuration arrives as JSON; decode it with [`config()`].
//!
//! WASI 0.2 (`std::fs` on the granted directories, clocks, random) works
//! through the standard library. A `wasi:http` helper is not provided.
//!
//! # Text
//!
//! Everything a player reads is a [`Text`]: [`Text::Plain`], shown exactly
//! as written, or [`Text::Json`], a JSON text component with colours, line
//! breaks, hover text and click events. Every function and constructor that
//! shows text takes `impl Into<Text>`:
//!
//! - `&str` and `String` become plain text;
//! - the [`text`] builders ([`text::mini`] for Rift markup, [`text::legacy`]
//!   for `&c` codes, [`text::Component`] field by field) and a
//!   [`serde_json::Value`] become JSON text.
//!
//! ```no_run
//! use rift_plugin::{Action, TitleSpec, players, text};
//! use serde_json::json;
//!
//! players::message("Steve", "Shown <exactly> as written").ok();
//!
//! let welcome = text::mini("<gold>Welcome, <player>!</gold> <gray><click:run_command:/spawn>[Spawn]")
//!     .arg("player", "Steve");
//! players::message("Steve", welcome).ok();
//! players::title(
//!     "Steve",
//!     &TitleSpec::new(text::mini("<green>Lobby")).subtitle(text::legacy("&7Have fun")),
//! )
//! .ok();
//!
//! let reply = Action::message(
//!     text::Component::text("[Accept]")
//!         .color("green")
//!         .bold()
//!         .run_command("/party accept Alex")
//!         .hover_text("Join Alex's party"),
//! );
//! let banned = Action::deny(json!({
//!     "text": "You are banned\n", "color": "red",
//!     "extra": [{"text": "Reason: spam", "color": "gray"}],
//! }));
//! ```
//!
//! Rift spells hover and click events the way each player's client reads
//! them, so either the 1.21.5+ form (`click_event`, `hover_event`) or the
//! older one (`clickEvent`, `hoverEvent`) works for every client. Markup is
//! only ever read from an explicit `{"mini": ...}` object, which
//! [`text::mini`] writes; a string is always shown as written, so text from
//! players can be passed safely as plain text or as a markup argument.
//!
//! # Chat and kicks
//!
//! [`Plugin::chat`] sees every chat message, and every command that reaches
//! the backend, before the backend does, and may drop it. [`Plugin::kick`]
//! decides what happens to a player a backend kicked. Both are delivered
//! only to a registration that lists them:
//!
//! ```no_run
//! use rift_plugin::{Action, ChatAttempt, ChatKind, Context, Hook, Plugin, Registration};
//!
//! #[derive(Default)]
//! struct Mute {
//!     muted: Vec<String>,
//! }
//!
//! impl Plugin for Mute {
//!     fn init(&mut self, _config: &str) -> Result<Registration, String> {
//!         Ok(Registration::new().hooks([Hook::Chat, Hook::Kick]))
//!     }
//!
//!     fn chat(&mut self, attempt: ChatAttempt) -> Action {
//!         if attempt.kind == ChatKind::Message && self.muted.contains(&attempt.ctx.uuid) {
//!             return Action::deny("You are muted.");
//!         }
//!         Action::Continue
//!     }
//!
//!     fn kick(&mut self, ctx: Context) -> Action {
//!         match ctx.reason.as_deref() {
//!             Some(reason) if reason.contains("restarting") => Action::server("lobby"),
//!             _ => Action::Continue,
//!         }
//!     }
//! }
//!
//! rift_plugin::export!(Mute);
//! ```
//!
//! Rift asks a plugin that lists [`Hook::Chat`] about every message of every
//! player. Dropping one signed message breaks the player's signature chain,
//! so unless the proxy sets `authentication.signed_chat = false`, the first
//! deny costs that player their chat signing until they reconnect.
//!
//! # Background work
//!
//! A plugin runs one call at a time, so a slow lookup in `login` would keep
//! every other hook waiting. The [`background`] module moves such work to
//! worker instances (manifest `[background] workers = N`): the hook defers
//! its answer, queues a job and returns; a worker runs [`Plugin::work`];
//! [`Plugin::job_done`] receives the result and gives the answer.
//!
//! ```no_run
//! use rift_plugin::{Action, Context, Hook, Plugin, Registration, background};
//! use std::collections::HashMap;
//!
//! #[derive(Default)]
//! struct Bans {
//!     /// Job handle to the ticket of the login that waits for it.
//!     waiting: HashMap<u64, u64>,
//! }
//!
//! impl Plugin for Bans {
//!     fn init(&mut self, _config: &str) -> Result<Registration, String> {
//!         Ok(Registration::new().hooks([Hook::Login]))
//!     }
//!
//!     fn login(&mut self, ctx: Context) -> Action {
//!         let Ok(ticket) = background::defer() else {
//!             return Action::deny("Try again in a moment.");
//!         };
//!         match background::run(ctx.uuid.as_bytes()) {
//!             Ok(job) => {
//!                 self.waiting.insert(job, ticket);
//!             }
//!             Err(_) => {
//!                 let _ = background::resolve(ticket, Action::deny("Try again in a moment."));
//!             }
//!         }
//!         // Ignored: the answer is the one given to `resolve`.
//!         Action::Continue
//!     }
//!
//!     fn job_done(&mut self, job: u64, outcome: Result<Vec<u8>, String>) {
//!         let Some(ticket) = self.waiting.remove(&job) else { return };
//!         let action = match outcome {
//!             Ok(reason) if reason.is_empty() => Action::Continue,
//!             Ok(reason) => Action::deny(String::from_utf8_lossy(&reason).into_owned()),
//!             Err(_) => Action::deny("Try again in a moment."),
//!         };
//!         let _ = background::resolve(ticket, action);
//!     }
//!
//!     // Runs on a worker: another instance, with its own state.
//!     fn work(&mut self, uuid: Vec<u8>) -> Result<Vec<u8>, String> {
//!         let uuid = String::from_utf8(uuid).map_err(|error| error.to_string())?;
//!         // The slow part; an empty answer means "not banned".
//!         Ok(std::fs::read(format!("/data/bans/{uuid}")).unwrap_or_default())
//!     }
//! }
//!
//! rift_plugin::export!(Bans);
//! ```
//!
//! A worker is a separate instance of the same component with its own
//! memory: its state is its own `Default` value, [`Plugin::init`] never runs
//! there, and it receives no hooks. [`Plugin::work_init`] runs once per
//! worker before its first job, with the plugin's configuration. The
//! deferred hook's deadline keeps running while the job does.
//!
//! # Dialogs
//!
//! [`players::show_dialog`] shows a dialog (Minecraft 1.21.6+) in the
//! vanilla JSON format. A button whose `custom` or `dynamic/custom` action
//! has an `id` without a namespace comes back to [`Plugin::dialog`]:
//!
//! ```no_run
//! use rift_plugin::{Context, DialogResponse, Plugin, players};
//! use serde_json::json;
//!
//! #[derive(Default)]
//! struct Rules;
//!
//! impl Plugin for Rules {
//!     fn join(&mut self, ctx: Context) {
//!         let _ = players::show_dialog(&ctx.uuid, &json!({
//!             "type": "confirmation",
//!             "title": "Server rules",
//!             "body": "Be nice.",
//!             "yes": {"label": "Accept", "action": {"type": "custom", "id": "accept"}},
//!             "no": {"label": "Leave", "action": {"type": "custom", "id": "leave"}},
//!         }));
//!     }
//!
//!     fn dialog(&mut self, response: DialogResponse) {
//!         if response.action == "leave" {
//!             let _ = players::kick(&response.ctx.uuid, "You declined the rules.");
//!         }
//!     }
//! }
//! # rift_plugin::export!(Rules);
//! ```

#![warn(missing_docs)]

/// The raw generated bindings for the `plugin` world, including WASI.
///
/// Prefer the wrappers at the crate root; this is an escape hatch.
#[allow(missing_docs, clippy::all)]
pub mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "rift:plugin/plugin@0.3.0",
        pub_export_macro: true,
        export_macro_name: "__export_rift_plugin",
        default_bindings_module: "rift_plugin::bindings",
        generate_all,
    });
}

pub use bindings::rift::plugin::types::{
    BossBarSpec, BusMessage, ChatAttempt, ChatKind, CommandSpec, Connection, Context,
    DialogResponse, Hook, HttpRequest, HttpResponse, Level, Ping, PlayerInfo, PlayerRecord,
    ProfileProperty, Registration, RiftEvent, RouteDecision, StatusOverride, SuggestKind,
    Suggestion, Text, TitleSpec,
};

/// The answer to a decision hook: the contract's `decision` variant.
///
/// Each hook accepts a subset; an answer outside it counts as a failed call:
/// - `login`, `before_transfer`, `chat`: `Continue`, [`deny`](Action::deny)
/// - `initial_server`, `kick`: `Continue`, [`deny`](Action::deny),
///   [`server`](Action::server)
/// - `command`: `Continue`, [`message`](Action::message),
///   [`server`](Action::server), [`queue`](Action::queue), `LeaveQueue`
pub type Action = bindings::rift::plugin::types::Decision;

use serde::de::DeserializeOwned;

/// A Rift plugin. Implement the hooks you need and pass the type to
/// [`export!`].
///
/// The instance is created with [`Default`] before [`Plugin::init`]; see the
/// [crate documentation](crate#state) for how state is kept.
#[allow(unused_variables)]
pub trait Plugin: Default + 'static {
    /// Called once per instance with the plugin's configuration as JSON
    /// (`{}` without one). Create timers and bus subscriptions here.
    fn init(&mut self, config: &str) -> Result<Registration, String> {
        Ok(Registration::new())
    }

    /// The configuration changed and the manifest sets `hot_reload = true`.
    fn reload(&mut self, config: &str) -> Result<(), String> {
        Ok(())
    }

    /// The instance is about to be dropped.
    fn shutdown(&mut self, reason: &str) {}

    /// A TCP connection was accepted, before any bytes are read.
    fn route(&mut self, connection: Connection) -> RouteDecision {
        RouteDecision::Default
    }

    /// A player authenticated. Accepts `continue` and `deny`.
    fn login(&mut self, ctx: Context) -> Action {
        Action::Continue
    }

    /// Choose the first backend. Accepts `continue`, `deny` and `server`.
    fn initial_server(&mut self, ctx: Context) -> Action {
        Action::Continue
    }

    /// A transfer is about to happen. Accepts `continue` and `deny`.
    fn before_transfer(&mut self, ctx: Context) -> Action {
        Action::Continue
    }

    /// A registered command ran; `ctx.command` is its name, also when an
    /// alias was typed, and `ctx.args` the text after it. Accepts
    /// `continue`, `message`, `server`, `queue` and `leave-queue`.
    ///
    /// For a command registered with [`CommandSpec::console`], `ctx.console`
    /// tells that the operator console runs it: there is no player then
    /// (`ctx.uuid` is empty and `ctx.name` is `CONSOLE`), and a `message`
    /// answer is shown to the operator.
    fn command(&mut self, ctx: Context) -> Action {
        Action::Continue
    }

    /// Complete an argument of a command registered with
    /// [`SuggestKind::Dynamic`].
    fn suggest(&mut self, ctx: Context) -> Vec<Suggestion> {
        Vec::new()
    }

    /// A server-list ping; return the fields to replace.
    fn status(&mut self, ping: Ping) -> Option<StatusOverride> {
        None
    }

    /// An authenticated request to `/ext` or `/ext/*` on the web server.
    fn http(&mut self, request: HttpRequest) -> Option<HttpResponse> {
        None
    }

    /// A player sent a chat message, or a command that neither Rift nor a
    /// plugin owns, and the backend has not seen it yet. Accepts `continue`,
    /// which lets it through unchanged, and `deny`, which drops it and shows
    /// the player the text.
    ///
    /// Delivered only when [`Registration::hooks`] lists [`Hook::Chat`]; see
    /// the [crate documentation](crate#chat-and-kicks) for what that
    /// requires of the proxy. For a command, `attempt.ctx.command` is the
    /// root name and `attempt.ctx.args` the rest.
    fn chat(&mut self, attempt: ChatAttempt) -> Action {
        Action::Continue
    }

    /// A backend kicked a player in play: `ctx.server` is the backend and
    /// `ctx.reason` its reason without formatting. `continue` leaves it to
    /// `network.on_kick`, `deny` disconnects the player with the text and
    /// `server` moves them there.
    ///
    /// Delivered only when [`Registration::hooks`] lists [`Hook::Kick`].
    fn kick(&mut self, ctx: Context) -> Action {
        Action::Continue
    }

    /// A player joined.
    fn join(&mut self, ctx: Context) {}

    /// A transfer finished (see `ctx.success`).
    fn after_transfer(&mut self, ctx: Context) {}

    /// A session closed (see `ctx.reason`).
    fn disconnect(&mut self, ctx: Context) {}

    /// A message arrived on a subscription made with [`bus::subscribe`].
    fn message(&mut self, message: BusMessage) {}

    /// A timer made with [`timers::every`] or [`timers::after`] fired.
    fn timer(&mut self, handle: u64) {}

    /// A Rift event (`rift.events.*`).
    fn event(&mut self, event: RiftEvent) {}

    /// A player clicked a plugin action of a dialog this plugin showed with
    /// [`players::show_dialog`]. Delivered without being listed in
    /// [`Registration::hooks`].
    fn dialog(&mut self, response: DialogResponse) {}

    /// A player clicked, typed in or left a menu this plugin showed with
    /// `players::menu`. Delivered without being listed in
    /// [`Registration::hooks`].
    fn menu(&mut self, event: menu::MenuEvent) {}

    /// A job started with [`background::run`] finished: `handle` is the one
    /// `run` returned and `outcome` what [`Plugin::work`] returned on the
    /// worker, or why the job failed. Delivered without being listed in
    /// [`Registration::hooks`].
    fn job_done(&mut self, handle: u64, outcome: Result<Vec<u8>, String>) {}

    /// Background workers only: called once per worker before its first
    /// job, with the plugin's configuration as JSON.
    ///
    /// A worker is a separate instance of the component. `self` is a fresh
    /// [`Default`] value that only `work_init` and [`Plugin::work`] ever
    /// see; [`Plugin::init`] is never called on it.
    fn work_init(&mut self, config: &str) -> Result<(), String> {
        Ok(())
    }

    /// Background workers only: one job queued with [`background::run`].
    /// It may block; the plugin's hooks keep running meanwhile. The result
    /// reaches [`Plugin::job_done`] on the plugin's main instance.
    fn work(&mut self, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        Err("this plugin has no background work".into())
    }
}

/// Exports a [`Plugin`] implementation as the component's
/// `rift:plugin/handler` interface. Use it exactly once per plugin crate.
///
/// ```ignore
/// rift_plugin::export!(MyPlugin);
/// ```
#[macro_export]
macro_rules! export {
    ($plugin:ty) => {
        #[doc(hidden)]
        type __RiftPluginExport = $crate::__private::Adapter<$plugin>;
        $crate::bindings::__export_rift_plugin!(__RiftPluginExport with_types_in $crate::bindings);
    };
}

/// Decodes the JSON configuration `init`, `reload` and `work_init` receive.
///
/// The host passes `{}` when the plugin has no configuration, so mark
/// optional fields with `#[serde(default)]`.
pub fn config<T: DeserializeOwned>(json: &str) -> Result<T, String> {
    serde_json::from_str(json).map_err(|error| format!("invalid plugin configuration: {error}"))
}

impl Text {
    /// Plain text of 1..=1024 bytes, shown exactly as written. The same as
    /// converting a `&str` or `String`.
    pub fn plain(text: impl Into<String>) -> Self {
        Self::Plain(text.into())
    }

    /// A JSON text component of at most 32 KiB that is already JSON text.
    /// The [`text`] builders and [`serde_json::Value`] convert into this
    /// with `into()`.
    pub fn json(json: impl Into<String>) -> Self {
        Self::Json(json.into())
    }
}

impl From<&str> for Text {
    /// [`Text::Plain`].
    fn from(text: &str) -> Self {
        Self::Plain(text.to_owned())
    }
}

impl From<String> for Text {
    /// [`Text::Plain`].
    fn from(text: String) -> Self {
        Self::Plain(text)
    }
}

impl From<&String> for Text {
    /// [`Text::Plain`].
    fn from(text: &String) -> Self {
        Self::Plain(text.clone())
    }
}

impl From<serde_json::Value> for Text {
    /// [`Text::Json`]: the value is a JSON text component.
    fn from(component: serde_json::Value) -> Self {
        Self::Json(component.to_string())
    }
}

impl From<&serde_json::Value> for Text {
    /// [`Text::Json`]: the value is a JSON text component.
    fn from(component: &serde_json::Value) -> Self {
        Self::Json(component.to_string())
    }
}

impl PartialEq for Text {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Plain(a), Self::Plain(b)) | (Self::Json(a), Self::Json(b)) => a == b,
            _ => false,
        }
    }
}

impl Registration {
    /// No commands and no hooks, which delivers every hook except
    /// [`Hook::Chat`] and [`Hook::Kick`].
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
            hooks: Vec::new(),
        }
    }

    /// Registers a command.
    pub fn command(mut self, command: CommandSpec) -> Self {
        self.commands.push(command);
        self
    }

    /// Asks for one more hook to be delivered.
    pub fn hook(mut self, hook: Hook) -> Self {
        self.hooks.push(hook);
        self
    }

    /// Asks for these hooks to be delivered.
    pub fn hooks(mut self, hooks: impl IntoIterator<Item = Hook>) -> Self {
        self.hooks.extend(hooks);
        self
    }
}

impl Default for Registration {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandSpec {
    /// A command without aliases, permission node or suggestions, for
    /// players only. Without a permission every player may see and run it.
    ///
    /// Names are 1 to 64 bytes of `a-z0-9_-`; `server` and `hub` are
    /// reserved.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            aliases: Vec::new(),
            permission: String::new(),
            suggest: SuggestKind::None,
            console: false,
        }
    }

    /// Another name for the command, written like the name. A command has
    /// at most 16; the hooks always see the name in `ctx.command`.
    pub fn alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    /// More names for the command; see [`CommandSpec::alias`].
    pub fn aliases<S: Into<String>>(mut self, aliases: impl IntoIterator<Item = S>) -> Self {
        self.aliases.extend(aliases.into_iter().map(Into::into));
        self
    }

    /// The permission node a player needs to see and run the command. An
    /// empty node opens the command to every player.
    pub fn permission(mut self, node: impl Into<String>) -> Self {
        self.permission = node.into();
        self
    }

    /// How the command's arguments are completed.
    pub fn suggest(mut self, kind: SuggestKind) -> Self {
        self.suggest = kind;
        self
    }

    /// Operators may run the command from the console (`rift admin plugins
    /// run`); [`Plugin::command`] then sees `ctx.console`.
    pub fn console(mut self) -> Self {
        self.console = true;
        self
    }
}

impl Action {
    /// `deny(reason)`: refuse, and show the player the reason.
    pub fn deny(reason: impl Into<Text>) -> Self {
        Self::Deny(reason.into())
    }

    /// `message(text)`: reply to whoever ran a command.
    pub fn message(text: impl Into<Text>) -> Self {
        Self::Message(text.into())
    }

    /// `server(name)`: send the player to a backend.
    pub fn server(name: impl Into<String>) -> Self {
        Self::Server(name.into())
    }

    /// `queue(name)`: put the player into the queue for a backend.
    pub fn queue(name: impl Into<String>) -> Self {
        Self::Queue(name.into())
    }
}

impl PartialEq for Action {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Continue, Self::Continue) | (Self::LeaveQueue, Self::LeaveQueue) => true,
            (Self::Deny(a), Self::Deny(b)) | (Self::Message(a), Self::Message(b)) => a == b,
            (Self::Server(a), Self::Server(b)) | (Self::Queue(a), Self::Queue(b)) => a == b,
            _ => false,
        }
    }
}

impl DialogResponse {
    /// The player's input values, decoded from `values_json`. They come from
    /// the client: validate them like any player input.
    pub fn values<T: DeserializeOwned>(&self) -> Result<T, String> {
        serde_json::from_str(&self.values_json)
            .map_err(|error| format!("invalid dialog values: {error}"))
    }

    /// The action's `payload` or `additions`, as the plugin wrote it; `None`
    /// when it had none.
    pub fn payload<T: DeserializeOwned>(&self) -> Result<Option<T>, String> {
        self.payload_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|error| format!("invalid dialog payload: {error}"))
    }
}

impl Suggestion {
    /// A suggestion without a tooltip.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tooltip: None,
        }
    }

    /// The tooltip shown next to the suggestion.
    pub fn tooltip(mut self, tooltip: impl Into<Text>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }
}

impl Default for StatusOverride {
    /// Replaces nothing. Set the fields to replace:
    ///
    /// ```
    /// use rift_plugin::{StatusOverride, text};
    ///
    /// let status = StatusOverride {
    ///     motd: Some(text::mini("<gold>Rift</gold>\n<gray>Now with plugins").into()),
    ///     max_players: Some(500),
    ///     ..Default::default()
    /// };
    /// ```
    fn default() -> Self {
        Self {
            motd: None,
            version: None,
            protocol: None,
            online: None,
            max_players: None,
            sample: None,
            favicon: None,
        }
    }
}

impl TitleSpec {
    /// A title with the vanilla times (10 ticks fade in, 70 stay, 20 fade
    /// out).
    pub fn new(title: impl Into<Text>) -> Self {
        Self {
            title: Some(title.into()),
            subtitle: None,
            fade_in: 10,
            stay: 70,
            fade_out: 20,
        }
    }

    /// Only a subtitle, with the vanilla times: the title above it stays
    /// empty.
    pub fn subtitle_only(subtitle: impl Into<Text>) -> Self {
        Self {
            title: None,
            subtitle: Some(subtitle.into()),
            fade_in: 10,
            stay: 70,
            fade_out: 20,
        }
    }

    /// The subtitle.
    pub fn subtitle(mut self, subtitle: impl Into<Text>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    /// Fade-in, stay and fade-out times in ticks.
    pub fn times(mut self, fade_in: i32, stay: i32, fade_out: i32) -> Self {
        self.fade_in = fade_in;
        self.stay = stay;
        self.fade_out = fade_out;
        self
    }
}

/// Inventory menus.
pub mod menu {
    pub use crate::bindings::rift::plugin::types::MenuEvent;
}

/// The joined players, and what Rift remembers about those who left.
/// `target` is a name, a UUID or a connection id.
///
/// The functions that show text take anything that converts into a
/// [`Text`]: a string for plain text, a [`text`]
/// builder or a [`serde_json::Value`] for a JSON text component. Each
/// returns whether the player was online.
///
/// [`PlayerInfo`] carries what Rift knows about an online
/// player, including `ip`, the profile `properties`, `visible_to`, `locale`
/// (the language the client reported, `None` until the client has sent it:
/// never in [`Plugin::login`], and in
/// [`Plugin::join`] only for 1.20.2 and newer clients)
/// and `language` (the one set with [`set_language`](players::set_language), else `locale`).
///
/// [`lookup`](players::lookup) and [`search`](players::search) answer with a
/// [`PlayerRecord`] for every player who joined at
/// least once, online or not.
pub mod players {
    use crate::Text;
    use crate::bindings::rift::plugin::players as host;
    pub use host::{
        boss_bar, clear_dialog, close_menu, dialog, get, list, lookup, menu, names, search,
        set_language, title, transfer, update_menu,
    };

    /// Send a chat message to a player.
    pub fn message(target: &str, text: impl Into<Text>) -> Result<bool, String> {
        host::message(target, &text.into())
    }

    /// Show text above a player's hotbar.
    pub fn action_bar(target: &str, text: impl Into<Text>) -> Result<bool, String> {
        host::action_bar(target, &text.into())
    }

    /// Disconnect a player with a reason.
    pub fn kick(target: &str, reason: impl Into<Text>) -> Result<bool, String> {
        host::kick(target, &reason.into())
    }

    /// Send a chat message to every player. Returns how many received it.
    pub fn broadcast(text: impl Into<Text>) -> Result<u32, String> {
        host::broadcast(&text.into())
    }

    /// Replace a player's tab-list header and footer. Both `None` restores
    /// the configured tab list.
    ///
    /// ```no_run
    /// use rift_plugin::{players, text};
    ///
    /// players::tab_list("Steve", Some(text::mini("<gold>Rift").into()), Some("Have fun".into())).ok();
    /// players::tab_list("Steve", None, None).ok();
    /// ```
    pub fn tab_list(
        target: &str,
        header: Option<Text>,
        footer: Option<Text>,
    ) -> Result<bool, String> {
        host::tab_list(target, header.as_ref(), footer.as_ref())
    }

    /// Show a dialog in the vanilla JSON format (Minecraft 1.21.6+). Returns
    /// false when the player is offline, older than 1.21.6 or has too many
    /// undelivered actions; an invalid dialog is an error.
    pub fn show_dialog(target: &str, dialog: &serde_json::Value) -> Result<bool, String> {
        self::dialog(target, &dialog.to_string())
    }
}

/// The durable key-value store. Namespaces are prefixed with
/// `plugin.<name>.` by the host.
pub mod store {
    pub use crate::bindings::rift::plugin::store::{delete, get, increment, list, set};
    use serde::Serialize;
    use serde::de::DeserializeOwned;

    /// Reads a JSON-encoded entry.
    pub fn get_json<T: DeserializeOwned>(namespace: &str, key: &str) -> Result<Option<T>, String> {
        match get(namespace, key)? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| format!("invalid JSON in {namespace}/{key}: {error}")),
            None => Ok(None),
        }
    }

    /// Writes an entry as JSON.
    pub fn set_json<T: Serialize + ?Sized>(
        namespace: &str,
        key: &str,
        value: &T,
    ) -> Result<(), String> {
        let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
        set(namespace, key, &bytes)
    }
}

/// Rift's permission store: user nodes and group memberships.
///
/// `add_group(uuid, group, expires_at)` adds the user to a group, until
/// `expires_at` Unix seconds when given (see [`meta::time`]
/// for the current time).
pub mod permissions {
    pub use crate::bindings::rift::plugin::permissions::{
        add_group, groups, has, list_groups, meta, remove_group, set, set_meta,
    };

    /// The meta key that makes a player visible only to viewers holding the
    /// node in its value (see docs/permissions.md, "Visibility").
    pub const VISIBLE_TO: &str = "rift.visible-to";
}

/// The message bus. Subscriptions deliver to [`Plugin::message`].
pub mod bus {
    use crate::BusMessage;
    pub use crate::bindings::rift::plugin::bus::{
        publish, request, stream_publish, subscribe, unsubscribe,
    };

    /// Publishes `payload` to the message's reply subject. Returns `None`
    /// when the message has none, otherwise how many subscribers received it.
    pub fn reply(message: &BusMessage, payload: &[u8]) -> Result<Option<u32>, String> {
        match &message.reply {
            Some(subject) => publish(subject, payload, None).map(Some),
            None => Ok(None),
        }
    }
}

/// Managed servers, through the `rift.control.*` operations. Results are
/// the control service's JSON replies.
pub mod servers {
    pub use crate::bindings::rift::plugin::servers::{
        control, create, download, remove, start, stop,
    };
}

/// Timers fire through [`Plugin::timer`] with the
/// returned handle.
pub mod timers {
    pub use crate::bindings::rift::plugin::timers::{after, cancel, every};
}

/// Work that must not hold up the plugin's hooks.
///
/// A plugin runs one call at a time, so a hook that waits for a database or
/// a web service keeps every other hook waiting. Background workers take
/// that work: each is a separate instance of the component, started when the
/// manifest sets `[background] workers`. The pattern has three steps (the
/// [crate documentation](crate#background-work) has a complete example):
///
/// 1. In a decision hook (`login`, `initial_server`, `before_transfer`,
///    `command`, `chat` or `kick`), call [`defer`](background::defer) for a ticket and [`run`](background::run)
///    with the job's payload, remember the ticket under the job's handle and
///    return. The hook's own return value is ignored.
/// 2. A worker calls [`Plugin::work`] with the payload.
///    It may block.
/// 3. [`Plugin::job_done`] receives the handle and
///    the result on the main instance; call [`resolve`](background::resolve) with the ticket and
///    the answer.
///
/// The deferred hook's deadline keeps running: an answer that comes too late
/// counts as a missed deadline, and [`resolve`](background::resolve) then returns `false`. [`run`](background::run)
/// also works without [`defer`](background::defer), from any hook, for work nobody waits for.
pub mod background {
    use crate::Action;
    use crate::bindings::rift::plugin::background as host;
    pub use host::{defer, is_worker, run};

    /// Answer a deferred hook with the ticket [`defer`] returned. Returns
    /// `false` when nobody waits for the ticket any more: the deadline
    /// passed, or it was answered already.
    pub fn resolve(ticket: u64, action: Action) -> Result<bool, String> {
        host::resolve(ticket, &action)
    }
}

/// Plugin metadata and clocks. `api` is the `rift:plugin` version the host
/// implements, such as `0.2.1`.
pub mod meta {
    pub use crate::bindings::rift::plugin::meta::{
        api, clock, config_dir, data_dir, name, time, version,
    };
}

/// Rift's log stream, prefixed with the plugin name by the host.
///
/// The SDK installs a [`::log`] logger before [`Plugin::init`]
/// (and before [`Plugin::work_init`] on a worker),
/// so the `log` crate's macros work as usual. They are re-exported here for
/// plugins that do not depend on `log` themselves:
/// `rift_plugin::log::info!("ready")`.
pub mod log {
    pub use crate::bindings::rift::plugin::log::log;
    #[doc(no_inline)]
    pub use ::log::{debug, error, info, trace, warn};

    use crate::Level;

    struct HostLogger;

    static LOGGER: HostLogger = HostLogger;

    impl ::log::Log for HostLogger {
        fn enabled(&self, _metadata: &::log::Metadata) -> bool {
            true
        }

        fn log(&self, record: &::log::Record) {
            let level = match record.level() {
                ::log::Level::Trace => Level::Trace,
                ::log::Level::Debug => Level::Debug,
                ::log::Level::Info => Level::Info,
                ::log::Level::Warn => Level::Warn,
                ::log::Level::Error => Level::Error,
            };
            match record.args().as_str() {
                Some(message) => log(level, message),
                None => log(level, &record.args().to_string()),
            }
        }

        fn flush(&self) {}
    }

    /// Installs the host logger once. Later calls do nothing, so a plugin may
    /// lower the level with `log::set_max_level` in `init`.
    pub(crate) fn install() {
        if ::log::set_logger(&LOGGER).is_ok() {
            ::log::set_max_level(::log::LevelFilter::Trace);
        }
    }
}

/// Builders for JSON text: each converts into a [`Text::Json`](crate::Text),
/// so it can be passed wherever text is shown.
///
/// - [`text::mini`] writes Rift markup as `{"mini": "...", "args": {...}}`; Rift
///   turns it into a component. See `docs/text.md` for the tags.
/// - [`text::legacy`] writes `&c` codes as `{"legacy": "...", "marker": "&"}`.
/// - [`text::message`] names a message from the plugin's `messages.toml` as
///   `{"message": "...", "args": {...}}`; Rift renders it in the player's
///   language.
/// - [`text::Component`] builds a JSON text component field by field, with the
///   1.21.5 spellings `click_event` and `hover_event` (Rift rewrites them
///   for older clients).
///
/// Every builder converts into [`Text`] and into a
/// [`serde_json::Value`] for use inside other components, and implements
/// [`Display`](std::fmt::Display) (its JSON text). A bare string is never
/// read as markup, so player input passed as an argument or as text cannot
/// add tags or click events.
///
/// [`text::render`], [`text::plain`], [`text::to_legacy`] and
/// [`text::to_mini`] ask Rift to convert text, for text that leaves through
/// something other than a player (a bus payload, an HTTP response, a log
/// line) and to check a template from the configuration in `init`. They need
/// `min_api = "0.2.1"` in `plugin.toml`.
pub mod text {
    use crate::bindings::rift::plugin::text as host;
    use serde_json::{Map, Value};
    use std::fmt;

    /// The JSON text component `text` becomes, with markup, legacy codes and
    /// messages resolved, in `language` (such as `de` or `de_de`) or the
    /// default. Plain text becomes a JSON string. An invalid text or
    /// language is an error.
    ///
    /// ```no_run
    /// use rift_plugin::text;
    ///
    /// // {"color":"red","text":"Hi"}
    /// let red = text::render(text::mini("<red>Hi"), None)?;
    /// // The plugin's own message `greeting`, in German.
    /// let words = text::plain(text::message("greeting"), Some("de"))?;
    /// # let _ = (red, words);
    /// # Ok::<(), String>(())
    /// ```
    pub fn render(text: impl Into<crate::Text>, language: Option<&str>) -> Result<String, String> {
        host::render(&text.into(), language)
    }

    /// The words of `text` without formatting, with messages resolved in
    /// `language` or the default; see [`render`].
    pub fn plain(text: impl Into<crate::Text>, language: Option<&str>) -> Result<String, String> {
        host::plain(&text.into(), language)
    }

    /// `text` with `§` codes, for surfaces that only take strings; see
    /// [`render`].
    pub fn to_legacy(
        text: impl Into<crate::Text>,
        language: Option<&str>,
    ) -> Result<String, String> {
        host::to_legacy(&text.into(), language)
    }

    /// Markup that writes `text`; plain text comes back escaped. See
    /// [`render`].
    pub fn to_mini(text: impl Into<crate::Text>, language: Option<&str>) -> Result<String, String> {
        host::to_mini(&text.into(), language)
    }

    /// Rift markup, such as `<gold>Hi <player>!`. Add arguments with
    /// [`Mini::arg`] and [`Mini::arg_component`].
    pub fn mini(markup: impl Into<String>) -> Mini {
        Mini {
            markup: markup.into(),
            args: Map::new(),
        }
    }

    /// `text` escaped so it can be spliced into markup: every `\` becomes
    /// `\\` and every `<` becomes `\<`. Nothing else changes.
    ///
    /// ```
    /// use rift_plugin::text;
    ///
    /// assert_eq!(text::escape("a < b"), "a \\< b");
    /// assert_eq!(text::escape("<red>\\"), "\\<red>\\\\");
    /// ```
    ///
    /// An argument ([`Mini::arg`]) is never parsed and needs no escaping.
    pub fn escape(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for c in text.chars() {
            if c == '<' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
        out
    }

    /// The value of a style argument: `{"style": markup}`.
    fn style(markup: impl Into<String>) -> Value {
        let mut object = Map::new();
        object.insert("style".into(), Value::String(markup.into()));
        Value::Object(object)
    }

    /// Markup with named arguments; see [`mini`].
    #[derive(Debug, Clone, PartialEq)]
    pub struct Mini {
        markup: String,
        args: Map<String, Value>,
    }

    impl Mini {
        /// `<name>` shows `text` exactly as written: it is never parsed, so
        /// a player name cannot add tags.
        pub fn arg(mut self, name: impl Into<String>, text: impl Into<String>) -> Self {
            self.args.insert(name.into(), Value::String(text.into()));
            self
        }

        /// `<name>` shows a component with its own formatting, such as a
        /// [`Component`], a [`legacy`] object or a [`serde_json::Value`].
        pub fn arg_component(
            mut self,
            name: impl Into<String>,
            component: impl Into<Value>,
        ) -> Self {
            self.args.insert(name.into(), component.into());
            self
        }

        /// `<name>` is a style: markup made only of formatting tags, such as
        /// `<#55ffff><bold>`. It wraps whatever the template puts inside
        /// `<name>...</name>`.
        ///
        /// ```
        /// use rift_plugin::text;
        ///
        /// let tag = text::mini("<team>[<tag>]</team>")
        ///     .arg_style("team", "<#55ffff><bold>")
        ///     .arg("tag", "RED");
        /// assert_eq!(
        ///     serde_json::Value::from(tag),
        ///     serde_json::json!({
        ///         "mini": "<team>[<tag>]</team>",
        ///         "args": {"team": {"style": "<#55ffff><bold>"}, "tag": "RED"},
        ///     })
        /// );
        /// ```
        pub fn arg_style(mut self, name: impl Into<String>, markup: impl Into<String>) -> Self {
            self.args.insert(name.into(), style(markup));
            self
        }

        /// The authoring object as JSON.
        pub fn to_value(&self) -> Value {
            let mut object = Map::new();
            object.insert("mini".into(), Value::String(self.markup.clone()));
            if !self.args.is_empty() {
                object.insert("args".into(), Value::Object(self.args.clone()));
            }
            Value::Object(object)
        }
    }

    /// A message from the plugin's `messages.toml`, by key. A key with `:`
    /// names another catalog, such as `party:invited` or `rift:queue.full`.
    /// Rift renders it in the recipient's language and checks the key and
    /// the arguments at the call.
    ///
    /// ```
    /// use rift_plugin::text;
    ///
    /// let invited = text::message("invited").arg("from", "Alex");
    /// assert_eq!(
    ///     invited.to_string(),
    ///     r#"{"args":{"from":"Alex"},"message":"invited"}"#
    /// );
    /// ```
    pub fn message(key: impl Into<String>) -> Message {
        Message {
            key: key.into(),
            args: Map::new(),
        }
    }

    /// A message with named arguments; see [`message`].
    #[derive(Debug, Clone, PartialEq)]
    pub struct Message {
        key: String,
        args: Map<String, Value>,
    }

    impl Message {
        /// `name` shows `text` exactly as written: it is never parsed.
        pub fn arg(mut self, name: impl Into<String>, text: impl Into<String>) -> Self {
            self.args.insert(name.into(), Value::String(text.into()));
            self
        }

        /// `name` shows a component with its own formatting, such as a
        /// [`Component`] or a [`mini`] object.
        pub fn arg_component(
            mut self,
            name: impl Into<String>,
            component: impl Into<Value>,
        ) -> Self {
            self.args.insert(name.into(), component.into());
            self
        }

        /// `name` is a style argument (see [`Mini::arg_style`]); the
        /// catalog declares it under `styles`.
        pub fn arg_style(mut self, name: impl Into<String>, markup: impl Into<String>) -> Self {
            self.args.insert(name.into(), style(markup));
            self
        }

        /// The authoring object as JSON.
        pub fn to_value(&self) -> Value {
            let mut object = Map::new();
            object.insert("message".into(), Value::String(self.key.clone()));
            if !self.args.is_empty() {
                object.insert("args".into(), Value::Object(self.args.clone()));
            }
            Value::Object(object)
        }
    }

    /// Legacy formatting codes with the `&` marker, such as `&cRed &lbold`;
    /// `&#rrggbb` writes a hex colour. Use [`Legacy::marker`] for another
    /// marker.
    pub fn legacy(text: impl Into<String>) -> Legacy {
        Legacy {
            text: text.into(),
            marker: '&',
        }
    }

    /// Legacy codes; see [`legacy`].
    #[derive(Debug, Clone, PartialEq)]
    pub struct Legacy {
        text: String,
        marker: char,
    }

    impl Legacy {
        /// The character that starts a code, such as `§`; one symbol, not a
        /// letter, digit or space.
        pub fn marker(mut self, marker: char) -> Self {
            self.marker = marker;
            self
        }

        /// The authoring object as JSON.
        pub fn to_value(&self) -> Value {
            let mut object = Map::new();
            object.insert("legacy".into(), Value::String(self.text.clone()));
            object.insert("marker".into(), Value::String(self.marker.to_string()));
            Value::Object(object)
        }
    }

    /// A JSON text component, built field by field.
    ///
    /// ```
    /// use rift_plugin::text::Component;
    ///
    /// let invite = Component::text("Alex invited you. ")
    ///     .extra(Component::text("[Accept]").color("green").bold().run_command("/party accept Alex"));
    /// assert_eq!(
    ///     serde_json::Value::from(invite),
    ///     serde_json::json!({
    ///         "text": "Alex invited you. ",
    ///         "extra": [{
    ///             "text": "[Accept]", "color": "green", "bold": true,
    ///             "click_event": {"action": "run_command", "command": "/party accept Alex"},
    ///         }],
    ///     })
    /// );
    /// ```
    #[derive(Debug, Clone, PartialEq)]
    pub struct Component(Map<String, Value>);

    impl Component {
        /// A component showing `text`.
        pub fn text(text: impl Into<String>) -> Self {
            Self(Map::new()).field("text", text.into())
        }

        /// A translation the client looks up by `key`, such as
        /// `multiplayer.player.joined`. Add arguments with
        /// [`Component::with`] and a fallback with [`Component::fallback`].
        ///
        /// ```
        /// use rift_plugin::text::Component;
        ///
        /// let joined = Component::translate("multiplayer.player.joined")
        ///     .with(Component::text("Alex").color("yellow"))
        ///     .fallback("Alex joined");
        /// assert_eq!(
        ///     serde_json::Value::from(joined),
        ///     serde_json::json!({
        ///         "translate": "multiplayer.player.joined",
        ///         "with": [{"text": "Alex", "color": "yellow"}],
        ///         "fallback": "Alex joined",
        ///     })
        /// );
        /// ```
        pub fn translate(key: impl Into<String>) -> Self {
            Self(Map::new()).field("translate", key.into())
        }

        /// Appends an argument of a [`Component::translate`]: a
        /// [`Component`], an authoring object or a string shown as written.
        pub fn with(self, argument: impl Into<Value>) -> Self {
            self.push("with", argument.into())
        }

        /// The text a client shows when it has no translation for the key.
        pub fn fallback(self, text: impl Into<String>) -> Self {
            self.field("fallback", text.into())
        }

        /// The key the player bound to `key`, such as `key.jump`.
        pub fn keybind(key: impl Into<String>) -> Self {
            Self(Map::new()).field("keybind", key.into())
        }

        /// A sprite from the blocks atlas, such as `item/diamond`. Choose
        /// another atlas with [`Component::atlas`]. Clients before 1.21.9
        /// do not show it.
        pub fn sprite(sprite: impl Into<String>) -> Self {
            Self(Map::new())
                .field("object", "atlas")
                .field("sprite", sprite.into())
        }

        /// The atlas of a [`Component::sprite`], such as `minecraft:gui`.
        pub fn atlas(self, atlas: impl Into<String>) -> Self {
            self.field("atlas", atlas.into())
        }

        /// The head of the player called `name`. Clients before 1.21.9 do
        /// not show it.
        ///
        /// ```
        /// use rift_plugin::text::Component;
        ///
        /// assert_eq!(
        ///     serde_json::Value::from(Component::head("Alex").hat(false)),
        ///     serde_json::json!({"object": "player", "player": {"name": "Alex"}, "hat": false})
        /// );
        /// ```
        pub fn head(name: impl Into<String>) -> Self {
            let mut player = Map::new();
            player.insert("name".into(), Value::String(name.into()));
            Self(Map::new())
                .field("object", "player")
                .field("player", Value::Object(player))
        }

        /// Whether a [`Component::head`] shows the hat layer (it does unless
        /// set to `false`).
        pub fn hat(self, shown: bool) -> Self {
            self.field("hat", shown)
        }

        /// A colour name such as `red` or `#ff8800`.
        pub fn color(self, color: impl Into<String>) -> Self {
            self.field("color", color.into())
        }

        /// Bold text.
        pub fn bold(self) -> Self {
            self.field("bold", true)
        }

        /// Italic text.
        pub fn italic(self) -> Self {
            self.field("italic", true)
        }

        /// Underlined text.
        pub fn underlined(self) -> Self {
            self.field("underlined", true)
        }

        /// Struck-through text.
        pub fn strikethrough(self) -> Self {
            self.field("strikethrough", true)
        }

        /// Obfuscated (scrambling) text.
        pub fn obfuscated(self) -> Self {
            self.field("obfuscated", true)
        }

        /// Turns a decoration (`bold`, `italic`, `underlined`,
        /// `strikethrough` or `obfuscated`) on or off. Off overrides what a
        /// parent component sets: `decoration("italic", false)`.
        pub fn decoration(self, name: &str, on: bool) -> Self {
            self.field(name, on)
        }

        /// The font, such as `minecraft:uniform`.
        pub fn font(self, font: impl Into<String>) -> Self {
            self.field("font", font.into())
        }

        /// A text shadow in `color` (`#rrggbb`) with `opacity` from 0 to 1,
        /// as `<shadow:#rrggbb:opacity>` writes it. An opacity of 0 hides
        /// the shadow. A color that is not `#rrggbb` makes Rift reject the
        /// component at the call.
        ///
        /// ```
        /// use rift_plugin::text::Component;
        ///
        /// let value = serde_json::Value::from(Component::text("x").shadow("#ff0000", 0.25));
        /// assert_eq!(value["shadow_color"], 0x40ff0000);
        /// ```
        pub fn shadow(self, color: &str, opacity: f64) -> Self {
            match shadow_color(color, opacity) {
                Some(argb) => self.field("shadow_color", argb),
                None => self.field("shadow_color", color),
            }
        }

        fn click(self, action: &str, key: &str, value: impl Into<String>) -> Self {
            let mut click = Map::new();
            click.insert("action".into(), Value::String(action.into()));
            click.insert(key.into(), Value::String(value.into()));
            self.field("click_event", Value::Object(click))
        }

        /// A click runs `command` (with its leading `/`) as the player.
        pub fn run_command(self, command: impl Into<String>) -> Self {
            self.click("run_command", "command", command)
        }

        /// A click puts `command` into the player's chat box.
        pub fn suggest_command(self, command: impl Into<String>) -> Self {
            self.click("suggest_command", "command", command)
        }

        /// A click opens `url` (`http` or `https`) after a confirmation.
        pub fn open_url(self, url: impl Into<String>) -> Self {
            self.click("open_url", "url", url)
        }

        /// A click copies `text` to the clipboard.
        pub fn copy_to_clipboard(self, text: impl Into<String>) -> Self {
            self.click("copy_to_clipboard", "value", text)
        }

        /// A click turns a book to `page`, counting from 1.
        pub fn change_page(self, page: u32) -> Self {
            let mut click = Map::new();
            click.insert("action".into(), Value::String("change_page".into()));
            click.insert("page".into(), Value::from(page));
            self.field("click_event", Value::Object(click))
        }

        /// Hover text shown as written.
        pub fn hover_text(self, text: impl Into<String>) -> Self {
            self.hover(Value::String(text.into()))
        }

        /// Hover text as a component, such as another [`Component`] or a
        /// [`mini`] object.
        pub fn hover(self, component: impl Into<Value>) -> Self {
            let mut hover = Map::new();
            hover.insert("action".into(), Value::String("show_text".into()));
            hover.insert("value".into(), component.into());
            self.field("hover_event", Value::Object(hover))
        }

        /// Hover shows `count` of the item `id`, such as `minecraft:diamond`.
        pub fn hover_item(self, id: impl Into<String>, count: u32) -> Self {
            let mut hover = Map::new();
            hover.insert("action".into(), Value::String("show_item".into()));
            hover.insert("id".into(), Value::String(id.into()));
            hover.insert("count".into(), Value::from(count));
            self.field("hover_event", Value::Object(hover))
        }

        /// Hover shows the entity of type `kind`, such as `minecraft:pig`,
        /// with `uuid` in its hyphenated form.
        pub fn hover_entity(self, kind: impl Into<String>, uuid: impl Into<String>) -> Self {
            let mut hover = Map::new();
            hover.insert("action".into(), Value::String("show_entity".into()));
            hover.insert("id".into(), Value::String(kind.into()));
            hover.insert("uuid".into(), Value::String(uuid.into()));
            self.field("hover_event", Value::Object(hover))
        }

        /// [`Component::hover_entity`] with a name: a [`Component`], an
        /// authoring object or a string shown as written.
        pub fn hover_entity_named(
            self,
            kind: impl Into<String>,
            uuid: impl Into<String>,
            name: impl Into<Value>,
        ) -> Self {
            let mut this = self.hover_entity(kind, uuid);
            if let Some(Value::Object(hover)) = this.0.get_mut("hover_event") {
                hover.insert("name".into(), name.into());
            }
            this
        }

        /// Text inserted into the chat box on shift-click.
        pub fn insertion(self, text: impl Into<String>) -> Self {
            self.field("insertion", text.into())
        }

        /// Appends a child: a [`Component`], an authoring object or a string
        /// shown as written. Children inherit this component's style.
        pub fn extra(self, child: impl Into<Value>) -> Self {
            self.push("extra", child.into())
        }

        fn push(mut self, key: &str, item: Value) -> Self {
            match self.0.get_mut(key) {
                Some(Value::Array(items)) => items.push(item),
                _ => {
                    self.0.insert(key.into(), Value::Array(vec![item]));
                }
            }
            self
        }

        /// Sets any other field to any JSON value, such as
        /// `field("insertion", "text")` or a field newer than this SDK.
        pub fn field(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
            self.0.insert(key.into(), value.into());
            self
        }

        /// The component as JSON.
        pub fn to_value(&self) -> Value {
            Value::Object(self.0.clone())
        }
    }

    /// The ARGB integer vanilla stores for a shadow, or `None` when `color`
    /// is not `#rrggbb`. The same arithmetic as the host's `<shadow>` tag.
    fn shadow_color(color: &str, opacity: f64) -> Option<i64> {
        let hex = color.strip_prefix('#')?;
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let rgb = u32::from_str_radix(hex, 16).ok()?;
        let opacity = if opacity.is_nan() {
            0.0
        } else {
            opacity.clamp(0.0, 1.0)
        };
        let alpha = (opacity * 255.0 + 0.5) as u32;
        // Vanilla stores ARGB as a signed int.
        Some(i64::from(((alpha << 24) | rgb) as i32))
    }

    macro_rules! json_text {
        ($($builder:ty),*) => {$(
            impl fmt::Display for $builder {
                /// The JSON text.
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    fmt::Display::fmt(&self.to_value(), f)
                }
            }

            impl From<$builder> for Value {
                fn from(builder: $builder) -> Value {
                    builder.to_value()
                }
            }

            impl From<$builder> for crate::Text {
                /// [`Text::Json`](crate::Text::Json) with the builder's JSON.
                fn from(builder: $builder) -> crate::Text {
                    crate::Text::Json(builder.to_string())
                }
            }

            impl From<&$builder> for crate::Text {
                /// [`Text::Json`](crate::Text::Json) with the builder's JSON.
                fn from(builder: &$builder) -> crate::Text {
                    crate::Text::Json(builder.to_string())
                }
            }
        )*};
    }

    json_text!(Mini, Message, Legacy, Component);
}

#[doc(hidden)]
pub mod __private {
    use crate::bindings::exports::rift::plugin::handler::Guest;
    use crate::*;
    use std::any::Any;
    use std::cell::RefCell;
    use std::marker::PhantomData;

    thread_local! {
        static INSTANCE: RefCell<Option<Box<dyn Any>>> = const { RefCell::new(None) };
    }

    /// Runs `f` on the instance, creating it first when there is none: a
    /// background worker never sees `init`.
    fn with<P: Plugin, R>(f: impl FnOnce(&mut P) -> R) -> R {
        INSTANCE.with(|slot| {
            let mut slot = slot.borrow_mut();
            let plugin = slot
                .get_or_insert_with(|| Box::new(P::default()))
                .downcast_mut::<P>()
                .expect("one plugin type per component");
            f(plugin)
        })
    }

    /// Connects a [`Plugin`] to the generated `Guest` trait.
    pub struct Adapter<P>(PhantomData<P>);

    impl<P: Plugin> Guest for Adapter<P> {
        fn init(config_json: String) -> Result<Registration, String> {
            crate::log::install();
            INSTANCE.with(|slot| *slot.borrow_mut() = Some(Box::new(P::default())));
            with(|p: &mut P| p.init(&config_json))
        }

        fn reload(config_json: String) -> Result<(), String> {
            with(|p: &mut P| p.reload(&config_json))
        }

        fn shutdown(reason: String) {
            with(|p: &mut P| p.shutdown(&reason));
            let instance = INSTANCE.with(|slot| slot.borrow_mut().take());
            drop(instance);
        }

        fn route(connection: Connection) -> RouteDecision {
            with(|p: &mut P| p.route(connection))
        }

        fn login(ctx: Context) -> Action {
            with(|p: &mut P| p.login(ctx))
        }

        fn initial_server(ctx: Context) -> Action {
            with(|p: &mut P| p.initial_server(ctx))
        }

        fn before_transfer(ctx: Context) -> Action {
            with(|p: &mut P| p.before_transfer(ctx))
        }

        fn command(ctx: Context) -> Action {
            with(|p: &mut P| p.command(ctx))
        }

        fn suggest(ctx: Context) -> Vec<Suggestion> {
            with(|p: &mut P| p.suggest(ctx))
        }

        fn status(ping: Ping) -> Option<StatusOverride> {
            with(|p: &mut P| p.status(ping))
        }

        fn http(request: HttpRequest) -> Option<HttpResponse> {
            with(|p: &mut P| p.http(request))
        }

        fn chat(attempt: ChatAttempt) -> Action {
            with(|p: &mut P| p.chat(attempt))
        }

        fn kick(ctx: Context) -> Action {
            with(|p: &mut P| p.kick(ctx))
        }

        fn join(ctx: Context) {
            with(|p: &mut P| p.join(ctx))
        }

        fn after_transfer(ctx: Context) {
            with(|p: &mut P| p.after_transfer(ctx))
        }

        fn disconnect(ctx: Context) {
            with(|p: &mut P| p.disconnect(ctx))
        }

        fn message(message: BusMessage) {
            with(|p: &mut P| p.message(message))
        }

        fn timer(handle: u64) {
            with(|p: &mut P| p.timer(handle))
        }

        fn event(event: RiftEvent) {
            with(|p: &mut P| p.event(event))
        }

        fn dialog(response: DialogResponse) {
            with(|p: &mut P| p.dialog(response))
        }

        fn menu(event: menu::MenuEvent) {
            with(|p: &mut P| p.menu(event))
        }

        fn job_done(handle: u64, outcome: Result<Vec<u8>, String>) {
            with(|p: &mut P| p.job_done(handle, outcome))
        }

        // A worker instance: `init` never ran here, so the logger is
        // installed and the instance created on the first of these calls.
        fn work_init(config_json: String) -> Result<(), String> {
            crate::log::install();
            with(|p: &mut P| p.work_init(&config_json))
        }

        fn work(payload: Vec<u8>) -> Result<Vec<u8>, String> {
            crate::log::install();
            with(|p: &mut P| p.work(payload))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builders_fill_the_wit_records() {
        let registration = Registration::new()
            .command(
                CommandSpec::new("hello")
                    .alias("hi")
                    .aliases(["hey", "yo"])
                    .permission("hello.use")
                    .suggest(SuggestKind::Players)
                    .console(),
            )
            .command(CommandSpec::new("plain"))
            .hooks([Hook::Login, Hook::Command])
            .hook(Hook::Chat);
        let hello = &registration.commands[0];
        assert_eq!(hello.name, "hello");
        assert_eq!(hello.aliases, ["hi", "hey", "yo"]);
        assert_eq!(hello.permission, "hello.use");
        assert!(matches!(hello.suggest, SuggestKind::Players));
        assert!(hello.console);
        let plain = &registration.commands[1];
        assert!(plain.aliases.is_empty() && plain.permission.is_empty() && !plain.console);
        assert!(matches!(plain.suggest, SuggestKind::None));
        assert_eq!(
            registration.hooks,
            vec![Hook::Login, Hook::Command, Hook::Chat]
        );
        assert!(Registration::default().commands.is_empty());
        assert!(Registration::default().hooks.is_empty());
    }

    #[test]
    fn strings_are_plain_and_builders_are_json() {
        assert_eq!(Text::from("<red>hi"), Text::Plain("<red>hi".into()));
        assert_eq!(Text::from(String::from("a")), Text::plain("a"));
        assert_eq!(Text::from(&String::from("a")), Text::plain("a"));
        assert_eq!(
            Text::from(text::mini("<red>x")),
            Text::Json(r#"{"mini":"<red>x"}"#.into())
        );
        assert_eq!(
            Text::from(text::legacy("&cx")),
            Text::json(r#"{"legacy":"&cx","marker":"&"}"#)
        );
        assert_eq!(
            Text::from(&text::Component::text("x").bold()),
            Text::json(r#"{"bold":true,"text":"x"}"#)
        );
        assert_eq!(
            Text::from(json!({"text": "x"})),
            Text::json(r#"{"text":"x"}"#)
        );
        // A JSON string is still JSON text: shown as written by the host.
        assert_eq!(Text::from(&json!("a\"b")), Text::json(r#""a\"b""#));
        assert_ne!(Text::plain("x"), Text::json("x"));
    }

    #[test]
    fn actions_take_any_text() {
        assert_eq!(Action::deny("no"), Action::Deny(Text::Plain("no".into())));
        assert_eq!(
            Action::deny(json!({"text": "Banned", "color": "red"})),
            Action::Deny(Text::Json(r#"{"color":"red","text":"Banned"}"#.into()))
        );
        assert_eq!(
            Action::message(format!("Hi, {}!", "Alex")),
            Action::Message(Text::Plain("Hi, Alex!".into()))
        );
        assert_eq!(
            Action::message(text::mini("<gold>Hi")),
            Action::Message(Text::Json(r#"{"mini":"<gold>Hi"}"#.into()))
        );
        assert_eq!(Action::server("lobby"), Action::Server("lobby".into()));
        assert_eq!(Action::queue("game"), Action::Queue("game".into()));
        assert_ne!(Action::server("a"), Action::queue("a"));
        assert_ne!(Action::deny("a"), Action::message("a"));
        assert_ne!(Action::Continue, Action::LeaveQueue);
    }

    #[test]
    fn titles_suggestions_and_status_take_any_text() {
        let title = TitleSpec::new(text::mini("<red>T"))
            .subtitle("sub")
            .times(1, 2, 3);
        assert_eq!(title.title, Some(Text::json(r#"{"mini":"<red>T"}"#)));
        assert_eq!(title.subtitle, Some(Text::plain("sub")));
        assert_eq!((title.fade_in, title.stay, title.fade_out), (1, 2, 3));
        let plain = TitleSpec::new("T");
        assert_eq!(plain.subtitle, None);
        assert_eq!((plain.fade_in, plain.stay, plain.fade_out), (10, 70, 20));
        let subtitle = TitleSpec::subtitle_only("only").times(0, 20, 0);
        assert_eq!(subtitle.title, None);
        assert_eq!(subtitle.subtitle, Some(Text::plain("only")));
        assert_eq!(subtitle.stay, 20);

        assert_eq!(Suggestion::new("a").tooltip, None);
        assert_eq!(
            Suggestion::new("a").tooltip("tip").tooltip,
            Some(Text::plain("tip"))
        );
        assert_eq!(
            Suggestion::new("a").tooltip(text::legacy("&7tip")).tooltip,
            Some(Text::json(r#"{"legacy":"&7tip","marker":"&"}"#))
        );

        let status = StatusOverride {
            version: Some("Rift".into()),
            sample: Some(vec!["one".into(), text::mini("<green>two").into()]),
            ..Default::default()
        };
        assert_eq!(status.version, Some(Text::plain("Rift")));
        assert_eq!(status.sample.map(|lines| lines.len()), Some(2));
        let nothing = StatusOverride::default();
        assert!(nothing.motd.is_none() && nothing.version.is_none() && nothing.sample.is_none());
        assert!(nothing.protocol.is_none() && nothing.online.is_none());
        assert!(nothing.max_players.is_none() && nothing.favicon.is_none());
    }

    #[test]
    fn hooks_have_defaults() {
        #[derive(Default)]
        struct Nothing;
        impl Plugin for Nothing {}
        let mut plugin = Nothing;
        let registration = plugin.init("{}").unwrap();
        assert!(registration.commands.is_empty() && registration.hooks.is_empty());
        assert_eq!(plugin.work_init("{}"), Ok(()));
        assert_eq!(
            plugin.work(b"job".to_vec()),
            Err("this plugin has no background work".to_owned())
        );
        plugin.job_done(1, Ok(Vec::new()));
        plugin.job_done(2, Err("failed".into()));
    }

    #[test]
    fn text_builders_write_escaped_json() {
        let welcome = text::mini("<gold>Hi \"<player>\"</gold>")
            .arg("player", "St<b>eve\n")
            .arg_component("rank", text::legacy("&cAdmin"));
        let value: serde_json::Value = serde_json::from_str(&welcome.to_string()).unwrap();
        assert_eq!(
            value,
            json!({
                "mini": "<gold>Hi \"<player>\"</gold>",
                "args": {"player": "St<b>eve\n", "rank": {"legacy": "&cAdmin", "marker": "&"}},
            })
        );
        assert_eq!(text::mini("<red>x").to_string(), r#"{"mini":"<red>x"}"#);
        assert_eq!(
            text::legacy("§aHi").marker('§').to_string(),
            r#"{"legacy":"§aHi","marker":"§"}"#
        );
        let component = text::Component::text("[a\"b]")
            .color("#ff8800")
            .bold()
            .italic()
            .underlined()
            .strikethrough()
            .obfuscated()
            .suggest_command("/msg ")
            .hover(text::mini("<green>Hover"))
            .insertion("ins")
            .extra("plain")
            .extra(text::Component::text("child").open_url("https://example.org"))
            .field("font", "minecraft:uniform");
        let value: serde_json::Value = serde_json::from_str(&component.to_string()).unwrap();
        assert_eq!(
            value,
            json!({
                "text": "[a\"b]", "color": "#ff8800", "bold": true, "italic": true,
                "underlined": true, "strikethrough": true, "obfuscated": true,
                "click_event": {"action": "suggest_command", "command": "/msg "},
                "hover_event": {"action": "show_text", "value": {"mini": "<green>Hover"}},
                "insertion": "ins",
                "extra": ["plain", {"text": "child", "click_event": {"action": "open_url", "url": "https://example.org"}}],
                "font": "minecraft:uniform",
            })
        );
        let value: serde_json::Value = text::Component::text("c")
            .copy_to_clipboard("v")
            .run_command("/x")
            .hover_text("t")
            .into();
        assert_eq!(
            value["click_event"],
            json!({"action": "run_command", "command": "/x"})
        );
        assert_eq!(
            value["hover_event"],
            json!({"action": "show_text", "value": "t"})
        );
    }

    #[test]
    fn escape_matches_the_shared_vectors() {
        for (input, escaped) in [
            ("plain", "plain"),
            ("a < b", r"a \< b"),
            ("<red>", r"\<red>"),
            (r"\", r"\\"),
            (r"\<", r"\\\<"),
            ("", ""),
        ] {
            assert_eq!(text::escape(input), escaped, "escape({input:?})");
        }
    }

    #[test]
    fn messages_and_style_arguments() {
        let invited = text::message("party:invited")
            .arg("from", "Alex")
            .arg_component("server", text::Component::text("lobby").color("aqua"))
            .arg_style("team", "<#55ffff><bold>");
        assert_eq!(
            serde_json::Value::from(invited),
            json!({
                "message": "party:invited",
                "args": {
                    "from": "Alex",
                    "server": {"text": "lobby", "color": "aqua"},
                    "team": {"style": "<#55ffff><bold>"},
                },
            })
        );
        assert_eq!(
            Text::from(text::message("created")),
            Text::json(r#"{"message":"created"}"#)
        );
        assert_eq!(
            text::mini("<team>x</team>")
                .arg_style("team", "<red>")
                .to_string(),
            r#"{"args":{"team":{"style":"<red>"}},"mini":"<team>x</team>"}"#
        );
        // A message nests like any other authoring object.
        let value: serde_json::Value = text::mini("<m>")
            .arg_component("m", text::message("hi"))
            .into();
        assert_eq!(value["args"]["m"], json!({"message": "hi"}));
    }

    #[test]
    fn component_contents_styles_and_events() {
        fn value(component: text::Component) -> serde_json::Value {
            component.into()
        }
        assert_eq!(
            value(
                text::Component::translate("chat.type.text")
                    .with("Alex")
                    .with(text::Component::text("hi").italic())
                    .fallback("<Alex> hi")
            ),
            json!({
                "translate": "chat.type.text",
                "with": ["Alex", {"text": "hi", "italic": true}],
                "fallback": "<Alex> hi",
            })
        );
        assert_eq!(
            value(text::Component::keybind("key.jump")),
            json!({"keybind": "key.jump"})
        );
        assert_eq!(
            value(text::Component::sprite("item/diamond")),
            json!({"object": "atlas", "sprite": "item/diamond"})
        );
        assert_eq!(
            value(text::Component::sprite("block/stone").atlas("minecraft:blocks")),
            json!({"object": "atlas", "sprite": "block/stone", "atlas": "minecraft:blocks"})
        );
        assert_eq!(
            value(text::Component::head("Alex")),
            json!({"object": "player", "player": {"name": "Alex"}})
        );
        assert_eq!(
            value(text::Component::head("Alex").hat(false)),
            json!({"object": "player", "player": {"name": "Alex"}, "hat": false})
        );
        assert_eq!(
            value(
                text::Component::text("x")
                    .font("minecraft:uniform")
                    .decoration("italic", false)
                    .bold()
                    .decoration("bold", false)
                    .field("insertion", "i")
            ),
            json!({"text": "x", "font": "minecraft:uniform", "italic": false, "bold": false, "insertion": "i"})
        );
        // The host's `<shadow>` arithmetic: opacity * 255, rounded, as signed ARGB.
        for (color, opacity, argb) in [
            ("#ff0000", 0.25, json!(0x40ff0000)),
            ("#000000", 1.0, json!(-16777216)),
            ("#FFAA00", 0.5, json!(-2130728448)),
            ("#123456", 0.0, json!(0x123456)),
            ("#ffffff", 2.0, json!(-1)),
            ("#ffffff", f64::NAN, json!(0xffffff)),
            ("red", 0.5, json!("red")),
            ("#fffff", 0.5, json!("#fffff")),
            ("#+fffff", 0.5, json!("#+fffff")),
        ] {
            assert_eq!(
                value(text::Component::text("x").shadow(color, opacity))["shadow_color"],
                argb,
                "shadow({color:?}, {opacity})"
            );
        }
        assert_eq!(
            value(text::Component::text("x").change_page(3))["click_event"],
            json!({"action": "change_page", "page": 3})
        );
        assert_eq!(
            value(text::Component::text("x").hover_item("minecraft:diamond", 2))["hover_event"],
            json!({"action": "show_item", "id": "minecraft:diamond", "count": 2})
        );
        let uuid = "00000000-0000-0000-0000-000000000001";
        assert_eq!(
            value(text::Component::text("x").hover_entity("minecraft:pig", uuid))["hover_event"],
            json!({"action": "show_entity", "id": "minecraft:pig", "uuid": uuid})
        );
        assert_eq!(
            value(text::Component::text("x").hover_entity_named(
                "minecraft:pig",
                uuid,
                text::mini("<red>Bob")
            ))["hover_event"],
            json!({"action": "show_entity", "id": "minecraft:pig", "uuid": uuid, "name": {"mini": "<red>Bob"}})
        );
    }

    #[test]
    fn config_decodes_json() {
        #[derive(serde::Deserialize)]
        struct Config {
            #[serde(default)]
            deny: Option<String>,
        }
        let config: Config = config(r#"{"deny":"Mallory"}"#).unwrap();
        assert_eq!(config.deny.as_deref(), Some("Mallory"));
        let empty: Config = super::config("{}").unwrap();
        assert_eq!(empty.deny, None);
        assert!(super::config::<Config>("nope").is_err());
    }
}
