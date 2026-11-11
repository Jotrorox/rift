//! Guest SDK for Rift WebAssembly plugins.
//!
//! A Rift plugin is a WebAssembly component built for `wasm32-wasip2` against
//! the `plugin` world of `rift:plugin@0.1.5`. This crate ships that WIT,
//! generates the bindings and wraps them, so a plugin is an ordinary
//! `cdylib` crate:
//!
//! ```toml
//! [lib]
//! crate-type = ["cdylib"]
//!
//! [dependencies]
//! rift-plugin = "0.1"
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
//!             .command(CommandSpec::new("greet").permission("greeter.use").suggest(SuggestKind::Players))
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
//! [`Registration::hooks`] so the host skips the rest; an empty list delivers
//! every hook.
//!
//! # Host API
//!
//! [`players`], [`store`], [`permissions`], [`bus`], [`servers`], [`timers`],
//! [`log`] and [`meta`] mirror the WIT interfaces of the same names. Errors
//! are the host's message as a `String`. [`log`] also installs a [`::log`]
//! logger before `init`, so `log::info!` and friends reach Rift's log.
//! Plugin configuration arrives as JSON; decode it with [`config()`].
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
//!
//! The dialog functions are new in rift:plugin 0.1.1. A plugin that calls
//! them needs a Rift that implements 0.1.1; older hosts refuse to load it.
//! A plugin that does not call them loads on every 0.1 host. The same holds
//! for the rich text functions of 0.1.2 and [`players::title_json`] of 0.1.4
//! below.
//!
//! WASI 0.2 (`std::fs` on the granted directories, clocks, random) works
//! through the standard library. A `wasi:http` helper is not provided.
//!
//! # Rich text
//!
//! Messages, action bars, kicks and deny reasons are plain text by default.
//! For colours, line breaks, hover text and click events, pass a JSON text
//! component to the `_json` forms: [`Action::deny_json`],
//! [`Action::message_json`], [`players::message_json`],
//! [`players::action_bar_json`], [`players::kick_json`] and
//! [`players::broadcast_json`]. Rift spells hover and click events the way
//! each player's client reads them, so either the 1.21.5+ form
//! (`click_event`, `hover_event`) or the older one (`clickEvent`,
//! `hoverEvent`) works for every client.
//!
//! ```no_run
//! use rift_plugin::{Action, players};
//! use serde_json::json;
//!
//! let invite = json!([
//!     {"text": "Alex invited you to their party. "},
//!     {"text": "[Accept]", "color": "green",
//!      "click_event": {"action": "run_command", "command": "/party accept Alex"},
//!      "hover_event": {"action": "show_text", "value": "Join Alex's party"}},
//! ]);
//! players::message_json("Sam", &invite.to_string()).ok();
//!
//! let banned = Action::deny_json(json!({
//!     "text": "You are banned\n", "color": "red",
//!     "extra": [{"text": "Reason: spam", "color": "gray"}],
//! }));
//! ```
//!
//! The [`text`] builders write that JSON for you: [`text::mini`] for Rift
//! markup (`<red>`, `<click:...>`, see `docs/text.md`), [`text::legacy`] for
//! `&c` codes and [`text::Component`] for a component field by field. Markup
//! is only ever read from an explicit `{"mini": ...}` object, which
//! [`text::mini`] writes; a bare string is always shown as written, so text
//! from players can be passed safely as an argument:
//!
//! ```no_run
//! use rift_plugin::{Action, TitleJsonSpec, players, text};
//!
//! let welcome = text::mini("<gold>Welcome, <player>!</gold> <gray><click:run_command:/spawn>[Spawn]")
//!     .arg("player", "Steve");
//! players::message_json("Steve", &welcome.to_string()).ok();
//! players::title_json(
//!     "Steve",
//!     &TitleJsonSpec::new(text::mini("<green>Lobby")).subtitle(text::legacy("&7Have fun")),
//! )
//! .ok();
//! let reply = Action::message_json(
//!     text::Component::text("[Accept]")
//!         .color("green")
//!         .bold()
//!         .run_command("/party accept Alex")
//!         .hover_text("Join Alex's party"),
//! );
//! ```
//!
//! # Rich suggestions and server-list text
//!
//! [`Plugin::suggest_rich`] and [`Plugin::status_rich`] answer `suggest` and
//! `status` with JSON tooltips, version text and player-sample lines (since
//! 0.1.4, through the optional `styled` export). Their defaults call
//! [`Plugin::suggest`] and [`Plugin::status`], so implement whichever form
//! you need. Hosts before 0.1.4 call the plain hooks; the SDK then shows the
//! rich text as its words.

#![warn(missing_docs)]

/// The raw generated bindings for the `plugin` world, including WASI.
///
/// Prefer the wrappers at the crate root; this is an escape hatch.
#[allow(missing_docs, clippy::all)]
pub mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "rift:plugin/plugin@0.1.5",
        pub_export_macro: true,
        export_macro_name: "__export_rift_plugin",
        default_bindings_module: "rift_plugin::bindings",
        generate_all,
    });
}

pub use bindings::rift::plugin::types::{
    BossBarSpec, BusMessage, CommandSpec, Connection, Context, DialogResponse, Hook, HttpRequest,
    HttpResponse, Level, Ping, PlayerInfo, Registration, RichStatusOverride, RichSuggestion,
    RiftEvent, RouteDecision, StatusOverride, SuggestKind, Suggestion, TitleJsonSpec, TitleSpec,
};

/// The answer to a decision hook: the contract's `decision` variant, which is
/// `action` plus `DenyJson` and `MessageJson` for rich text (since 0.1.2).
///
/// The SDK answers through the `decisions` export, which hosts from 0.1.2
/// call. An older host calls the `handler` hooks instead and shows the rich
/// cases as their plain words.
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

    /// A registered command ran. Accepts `continue`, `message`, `server`,
    /// `queue` and `leave-queue`.
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

    /// [`Plugin::suggest`] with JSON text tooltips (since 0.1.4). The default
    /// answers with [`Plugin::suggest`].
    fn suggest_rich(&mut self, ctx: Context) -> Vec<RichSuggestion> {
        self.suggest(ctx)
            .into_iter()
            .map(RichSuggestion::from)
            .collect()
    }

    /// [`Plugin::status`] with JSON text for the version and the player
    /// sample (since 0.1.4). The default answers with [`Plugin::status`].
    fn status_rich(&mut self, ping: Ping) -> Option<RichStatusOverride> {
        self.status(ping).map(RichStatusOverride::from)
    }

    /// An authenticated request to `/api/script/*`.
    fn http(&mut self, request: HttpRequest) -> Option<HttpResponse> {
        None
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

/// Decodes the JSON configuration `init` and `reload` receive.
///
/// The host passes `{}` when the plugin has no configuration, so mark
/// optional fields with `#[serde(default)]`.
pub fn config<T: DeserializeOwned>(json: &str) -> Result<T, String> {
    serde_json::from_str(json).map_err(|error| format!("invalid plugin configuration: {error}"))
}

impl Registration {
    /// No commands and no hooks (which delivers every hook).
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
    /// A command without a permission node or suggestions.
    ///
    /// Names are 1 to 64 bytes of `a-z0-9_-`; `server` and `hub` are
    /// reserved.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            permission: String::new(),
            suggest: SuggestKind::None,
        }
    }

    /// The permission node a player needs to see and run the command.
    pub fn permission(mut self, node: impl Into<String>) -> Self {
        self.permission = node.into();
        self
    }

    /// How the command's arguments are completed.
    pub fn suggest(mut self, kind: SuggestKind) -> Self {
        self.suggest = kind;
        self
    }
}

impl Action {
    /// `deny(reason)`: plain text of 1..=1024 bytes.
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny(reason.into())
    }

    /// `deny-json(reason)`: a JSON text component of at most 32 KiB, given
    /// as a [`serde_json::Value`] or as JSON text.
    pub fn deny_json(reason: impl ToString) -> Self {
        Self::DenyJson(reason.to_string())
    }

    /// `message(text)`: reply to the player who ran a command.
    pub fn message(text: impl Into<String>) -> Self {
        Self::Message(text.into())
    }

    /// `message-json(text)`: reply with a JSON text component of at most
    /// 32 KiB, given as a [`serde_json::Value`] or as JSON text.
    pub fn message_json(text: impl ToString) -> Self {
        Self::MessageJson(text.to_string())
    }

    /// `server(name)`: send the player to a backend.
    pub fn server(name: impl Into<String>) -> Self {
        Self::Server(name.into())
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
}

impl RichSuggestion {
    /// A suggestion without a tooltip.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tooltip_json: None,
        }
    }

    /// A tooltip: a JSON text component, such as a [`text`] builder or a
    /// [`serde_json::Value`].
    pub fn tooltip(mut self, tooltip: impl ToString) -> Self {
        self.tooltip_json = Some(tooltip.to_string());
        self
    }
}

impl From<Suggestion> for RichSuggestion {
    /// A plain tooltip becomes a JSON string, which shows it as written.
    fn from(suggestion: Suggestion) -> Self {
        Self {
            text: suggestion.text,
            tooltip_json: suggestion.tooltip.map(|tooltip| text::quote(&tooltip)),
        }
    }
}

impl From<StatusOverride> for RichStatusOverride {
    /// Plain version and sample lines become JSON strings, which show them as
    /// written.
    fn from(status: StatusOverride) -> Self {
        Self {
            motd_json: status.motd_json,
            version_json: status.version.map(|version| text::quote(&version)),
            protocol: status.protocol,
            online: status.online,
            max_players: status.max_players,
            sample_json: status
                .sample
                .map(|lines| lines.iter().map(|line| text::quote(line)).collect()),
            favicon: status.favicon,
        }
    }
}

impl Default for RichStatusOverride {
    /// Replaces nothing.
    fn default() -> Self {
        Self {
            motd_json: None,
            version_json: None,
            protocol: None,
            online: None,
            max_players: None,
            sample_json: None,
            favicon: None,
        }
    }
}

impl TitleJsonSpec {
    /// A title with the vanilla times (10 ticks fade in, 70 stay, 20 fade
    /// out). `title` is a JSON text component, such as a [`text`] builder or
    /// a [`serde_json::Value`]; an empty string shows only the subtitle.
    pub fn new(title: impl ToString) -> Self {
        Self {
            title_json: title.to_string(),
            subtitle_json: None,
            fade_in: 10,
            stay: 70,
            fade_out: 20,
        }
    }

    /// The subtitle, a JSON text component.
    pub fn subtitle(mut self, subtitle: impl ToString) -> Self {
        self.subtitle_json = Some(subtitle.to_string());
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

/// The joined players. `target` is a name, a UUID or a connection id.
///
/// The `_json` functions take a JSON text component as text, for example
/// `&serde_json::json!({...}).to_string()` or
/// `&text::mini("<red>Hi").to_string()`; see [rich text](crate#rich-text).
/// `title_json` (since 0.1.4) takes a [`TitleJsonSpec`].
///
/// `locale` (since 0.1.5) is the language the player's client reported, as
/// the client spells it (`en_us`), or `None` while Rift does not know it.
/// The client sends it after login: during configuration from 1.20.2, after
/// Join Game before. So it is never known in [`Plugin::login`](crate::Plugin::login)
/// and in [`Plugin::join`](crate::Plugin::join) only for 1.20.2 and newer
/// clients.
pub mod players {
    pub use crate::bindings::rift::plugin::players::{
        action_bar, action_bar_json, boss_bar, broadcast, broadcast_json, clear_dialog, dialog,
        get, kick, kick_json, list, locale, message, message_json, names, tab_list, title,
        title_json, transfer, visible_to,
    };

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
pub mod permissions {
    pub use crate::bindings::rift::plugin::permissions::{
        add_group, groups, has, list_groups, meta, remove_group, set, set_meta,
    };

    /// The meta key that makes a player visible only to viewers holding the
    /// node in its value (see docs/permissions.md, "Visibility").
    pub const VISIBLE_TO: &str = "rift.visible-to";
}

/// The message bus. Subscriptions deliver to [`Plugin::message`](crate::Plugin::message).
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

/// Timers fire through [`Plugin::timer`](crate::Plugin::timer) with the
/// returned handle.
pub mod timers {
    pub use crate::bindings::rift::plugin::timers::{after, cancel, every};
}

/// Plugin metadata and clocks.
pub mod meta {
    pub use crate::bindings::rift::plugin::meta::{
        clock, config_dir, data_dir, name, time, version,
    };
}

/// Rift's log stream, prefixed with the plugin name by the host.
///
/// The SDK installs a [`::log`] logger before [`Plugin::init`](crate::Plugin::init),
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

/// Builders for rich text: each writes the JSON that the `_json` host
/// functions, [`Action::deny_json`], [`Action::message_json`],
/// [`TitleJsonSpec`], [`RichSuggestion::tooltip`] and the other `-json`
/// fields accept.
///
/// - [`text::mini`] writes Rift markup as `{"mini": "...", "args": {...}}`; Rift
///   turns it into a component. See `docs/text.md` for the tags.
/// - [`text::legacy`] writes `&c` codes as `{"legacy": "...", "marker": "&"}`.
/// - [`text::Component`] builds a JSON text component field by field, with the
///   1.21.5 spellings `click_event` and `hover_event` (Rift rewrites them
///   for older clients).
///
/// Every builder implements [`Display`](std::fmt::Display) (its JSON text,
/// so `to_string()` and [`ToString`] arguments work) and converts into a
/// [`serde_json::Value`] for use inside other components. A bare string is
/// never read as markup, so player input passed as an argument or as text
/// cannot add tags or click events.
pub mod text {
    use serde_json::{Map, Value};
    use std::fmt;

    /// Rift markup, such as `<gold>Hi <player>!`. Add arguments with
    /// [`Mini::arg`] and [`Mini::arg_component`].
    pub fn mini(markup: impl Into<String>) -> Mini {
        Mini {
            markup: markup.into(),
            args: Map::new(),
        }
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
            let mut fields = Map::new();
            fields.insert("text".into(), Value::String(text.into()));
            Self(fields)
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

        /// Text inserted into the chat box on shift-click.
        pub fn insertion(self, text: impl Into<String>) -> Self {
            self.field("insertion", text.into())
        }

        /// Appends a child: a [`Component`], an authoring object or a string
        /// shown as written. Children inherit this component's style.
        pub fn extra(mut self, child: impl Into<Value>) -> Self {
            let child = child.into();
            match self.0.get_mut("extra") {
                Some(Value::Array(children)) => children.push(child),
                _ => {
                    self.0.insert("extra".into(), Value::Array(vec![child]));
                }
            }
            self
        }

        /// Sets any other field, such as `font`, `shadow_color` or
        /// `"italic": false`.
        pub fn field(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
            self.0.insert(key.into(), value.into());
            self
        }

        /// The component as JSON.
        pub fn to_value(&self) -> Value {
            Value::Object(self.0.clone())
        }
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
        )*};
    }

    json_text!(Mini, Legacy, Component);

    /// `text` as a JSON string, which the `-json` fields show as written.
    pub(crate) fn quote(text: &str) -> String {
        Value::String(text.to_owned()).to_string()
    }

    /// The text of markup without its tags, for hosts without markup.
    /// `arg(name, out)` writes an argument's words.
    pub(crate) fn strip_markup(
        markup: &str,
        out: &mut String,
        mut arg: impl FnMut(&str, &mut String),
    ) {
        let chars: Vec<char> = markup.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '\\' && matches!(chars.get(i + 1), Some('<' | '\\')) {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c != '<' {
                out.push(c);
                i += 1;
                continue;
            }
            // A tag runs to the first `>` outside quotes and has no space
            // outside quotes; anything else is a literal `<`.
            let mut end = None;
            let mut quote = None;
            let mut j = i + 1;
            while j < chars.len() {
                match (chars[j], quote) {
                    (q @ ('\'' | '"'), None) => quote = Some(q),
                    (q, Some(open)) if q == open => quote = None,
                    (_, Some(_)) => {}
                    ('>', None) => {
                        end = Some(j);
                        break;
                    }
                    (space, None) if space.is_whitespace() => break,
                    _ => {}
                }
                j += 1;
            }
            match end {
                Some(end) if end > i + 1 => {
                    let tag: String = chars[i + 1..end].iter().collect();
                    let name = tag
                        .split(':')
                        .next()
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    match name.as_str() {
                        "newline" | "br" => out.push('\n'),
                        _ if name.starts_with(['/', '!']) => {}
                        _ if !tag.contains(':') => arg(&name, out),
                        _ => {}
                    }
                    i = end + 1;
                }
                _ => {
                    out.push('<');
                    i += 1;
                }
            }
        }
    }

    /// Legacy text without its codes, for hosts without legacy objects.
    pub(crate) fn strip_legacy(text: &str, marker: char, out: &mut String) {
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == marker {
                match chars.get(i + 1) {
                    Some('#')
                        if chars.len() >= i + 8
                            && chars[i + 2..i + 8].iter().all(|c| c.is_ascii_hexdigit()) =>
                    {
                        i += 8;
                        continue;
                    }
                    Some(code) if "0123456789abcdefklmnorx".contains(code.to_ascii_lowercase()) => {
                        i += 2;
                        continue;
                    }
                    _ => {}
                }
            }
            out.push(chars[i]);
            i += 1;
        }
    }
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

        // Hosts before 0.1.2 call these; later ones call `decisions`.
        fn login(ctx: Context) -> PlainAction {
            plain(with(|p: &mut P| p.login(ctx)))
        }

        fn initial_server(ctx: Context) -> PlainAction {
            plain(with(|p: &mut P| p.initial_server(ctx)))
        }

        fn before_transfer(ctx: Context) -> PlainAction {
            plain(with(|p: &mut P| p.before_transfer(ctx)))
        }

        fn command(ctx: Context) -> PlainAction {
            plain(with(|p: &mut P| p.command(ctx)))
        }

        // Hosts before 0.1.4 call these; later ones call `styled`.
        fn suggest(ctx: Context) -> Vec<Suggestion> {
            let suggestions = with(|p: &mut P| p.suggest_rich(ctx));
            suggestions
                .into_iter()
                .map(|suggestion| Suggestion {
                    text: suggestion.text,
                    tooltip: suggestion
                        .tooltip_json
                        .map(|tooltip| words(&tooltip))
                        .filter(|tooltip| !tooltip.is_empty()),
                })
                .collect()
        }

        fn status(ping: Ping) -> Option<StatusOverride> {
            let status = with(|p: &mut P| p.status_rich(ping))?;
            Some(StatusOverride {
                motd_json: status.motd_json,
                version: status.version_json.map(|version| words(&version)),
                protocol: status.protocol,
                online: status.online,
                max_players: status.max_players,
                sample: status
                    .sample_json
                    .map(|lines| lines.iter().map(|line| words(line)).collect()),
                favicon: status.favicon,
            })
        }

        fn http(request: HttpRequest) -> Option<HttpResponse> {
            with(|p: &mut P| p.http(request))
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
    }

    impl<P: Plugin> crate::bindings::exports::rift::plugin::dialogs::Guest for Adapter<P> {
        fn dialog(response: DialogResponse) {
            with(|p: &mut P| p.dialog(response))
        }
    }

    impl<P: Plugin> crate::bindings::exports::rift::plugin::decisions::Guest for Adapter<P> {
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
    }

    impl<P: Plugin> crate::bindings::exports::rift::plugin::styled::Guest for Adapter<P> {
        fn suggest(ctx: Context) -> Vec<RichSuggestion> {
            with(|p: &mut P| p.suggest_rich(ctx))
        }

        fn status(ping: Ping) -> Option<RichStatusOverride> {
            with(|p: &mut P| p.status_rich(ping))
        }
    }

    use crate::bindings::rift::plugin::types::Action as PlainAction;

    /// A decision for a host before 0.1.2, which has no rich text: a JSON
    /// text component becomes its words.
    pub(crate) fn plain(action: Action) -> PlainAction {
        match action {
            Action::Continue => PlainAction::Continue,
            Action::Deny(reason) => PlainAction::Deny(reason),
            Action::Server(server) => PlainAction::Server(server),
            Action::Message(text) => PlainAction::Message(text),
            Action::Queue(server) => PlainAction::Queue(server),
            Action::LeaveQueue => PlainAction::LeaveQueue,
            Action::DenyJson(reason) => PlainAction::Deny(words(&reason)),
            Action::MessageJson(text) => PlainAction::Message(words(&text)),
        }
    }

    /// The visible text of a JSON text component, without formatting. The
    /// `mini` and `legacy` authoring objects lose their tags and codes.
    pub(crate) fn words(json: &str) -> String {
        fn collect(value: &serde_json::Value, out: &mut String) {
            match value {
                serde_json::Value::String(text) => out.push_str(text),
                serde_json::Value::Array(items) => items.iter().for_each(|item| collect(item, out)),
                serde_json::Value::Object(fields) => {
                    if let Some(markup) = fields.get("mini").and_then(|value| value.as_str()) {
                        let args = fields.get("args").and_then(|args| args.as_object());
                        crate::text::strip_markup(markup, out, |name, out| {
                            match args.and_then(|args| {
                                args.iter()
                                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
                                    .map(|(_, value)| value)
                            }) {
                                Some(serde_json::Value::String(text)) => out.push_str(text),
                                Some(
                                    value @ (serde_json::Value::Object(_)
                                    | serde_json::Value::Array(_)),
                                ) => collect(value, out),
                                Some(value) => out.push_str(&value.to_string()),
                                None => {}
                            }
                        });
                        return;
                    }
                    if let Some(legacy) = fields.get("legacy").and_then(|value| value.as_str()) {
                        let marker = fields
                            .get("marker")
                            .and_then(|value| value.as_str())
                            .and_then(|marker| marker.chars().next())
                            .unwrap_or('&');
                        crate::text::strip_legacy(legacy, marker, out);
                        return;
                    }
                    if let Some(text) = ["text", "fallback", "translate"]
                        .iter()
                        .find_map(|key| fields.get(*key).and_then(|value| value.as_str()))
                    {
                        out.push_str(text);
                    }
                    if let Some(extra) = fields.get("extra") {
                        collect(extra, out);
                    }
                }
                _ => {}
            }
        }
        match serde_json::from_str(json) {
            Ok(value) => {
                let mut out = String::new();
                collect(&value, &mut out);
                out
            }
            Err(_) => json.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_fill_the_wit_records() {
        let registration = Registration::new()
            .command(
                CommandSpec::new("hello")
                    .permission("hello.use")
                    .suggest(SuggestKind::Players),
            )
            .hooks([Hook::Login, Hook::Command]);
        assert_eq!(registration.commands[0].name, "hello");
        assert_eq!(registration.commands[0].permission, "hello.use");
        assert!(matches!(
            registration.commands[0].suggest,
            SuggestKind::Players
        ));
        assert_eq!(registration.hooks, vec![Hook::Login, Hook::Command]);
        assert!(Registration::default().commands.is_empty());
    }

    #[test]
    fn older_hosts_get_the_words_of_rich_decisions() {
        use crate::bindings::rift::plugin::types::Action as PlainAction;
        let banned = Action::deny_json(serde_json::json!({
            "text": "Banned", "color": "red", "extra": [{"text": ": spam"}],
        }));
        assert!(matches!(
            crate::__private::plain(banned),
            PlainAction::Deny(reason) if reason == "Banned: spam"
        ));
        assert!(matches!(
            crate::__private::plain(Action::message_json("[\"a\",{\"text\":\"b\"}]")),
            PlainAction::Message(text) if text == "ab"
        ));
        assert!(matches!(
            crate::__private::plain(Action::deny("x")),
            PlainAction::Deny(reason) if reason == "x"
        ));
    }

    #[test]
    fn text_builders_write_escaped_json() {
        use serde_json::json;
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
    fn older_hosts_get_the_words_of_rich_text() {
        use crate::__private::words;
        let mini = text::mini(
            "<gold>Hi <player>!</gold><br>a < b \\<c> <hover:show_text:'<red>x y'>z</hover>",
        )
        .arg("player", "Steve")
        .arg_component("unused", text::Component::text("no"));
        assert_eq!(words(&mini.to_string()), "Hi Steve!\na < b <c> z");
        assert_eq!(
            words(&text::legacy("&cRed &#ff8800hex &zkeep").to_string()),
            "Red hex &zkeep"
        );
        assert_eq!(words("\"<red>shown\""), "<red>shown");
        let rich = RichSuggestion::from(Suggestion {
            text: "a".into(),
            tooltip: Some("{\"not\":\"json\"}".into()),
        });
        assert_eq!(
            rich.tooltip_json.as_deref(),
            Some(r#""{\"not\":\"json\"}""#)
        );
        assert_eq!(
            words(rich.tooltip_json.as_deref().unwrap()),
            "{\"not\":\"json\"}"
        );
        let status = RichStatusOverride::from(StatusOverride {
            motd_json: None,
            version: Some("Rift".into()),
            protocol: None,
            online: None,
            max_players: None,
            sample: Some(vec!["one".into()]),
            favicon: None,
        });
        assert_eq!(status.version_json.as_deref(), Some("\"Rift\""));
        assert_eq!(status.sample_json, Some(vec!["\"one\"".to_owned()]));
        let title = TitleJsonSpec::new(text::mini("<red>T"))
            .subtitle("sub")
            .times(1, 2, 3);
        assert_eq!(title.title_json, r#"{"mini":"<red>T"}"#);
        assert_eq!(title.subtitle_json.as_deref(), Some("sub"));
        assert_eq!((title.fade_in, title.stay, title.fade_out), (1, 2, 3));
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
