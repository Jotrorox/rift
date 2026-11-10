//! Guest SDK for Rift WebAssembly plugins.
//!
//! A Rift plugin is a WebAssembly component built for `wasm32-wasip2` against
//! the `plugin` world of `rift:plugin@0.1.3`. This crate ships that WIT,
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
//! for the rich text functions of 0.1.2 below.
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

#![warn(missing_docs)]

/// The raw generated bindings for the `plugin` world, including WASI.
///
/// Prefer the wrappers at the crate root; this is an escape hatch.
#[allow(missing_docs, clippy::all)]
pub mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "rift:plugin/plugin@0.1.3",
        pub_export_macro: true,
        export_macro_name: "__export_rift_plugin",
        default_bindings_module: "rift_plugin::bindings",
        generate_all,
    });
}

pub use bindings::rift::plugin::types::{
    BossBarSpec, BusMessage, CommandSpec, Connection, Context, DialogResponse, Hook, HttpRequest,
    HttpResponse, Level, Ping, PlayerInfo, Registration, RiftEvent, RouteDecision, StatusOverride,
    SuggestKind, Suggestion, TitleSpec,
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

/// The joined players. `target` is a name, a UUID or a connection id.
///
/// The `_json` functions take a JSON text component as text, for example
/// `&serde_json::json!({...}).to_string()`; see [rich text](crate#rich-text).
pub mod players {
    pub use crate::bindings::rift::plugin::players::{
        action_bar, action_bar_json, boss_bar, broadcast, broadcast_json, clear_dialog, dialog,
        get, kick, kick_json, list, message, message_json, names, tab_list, title, transfer,
        visible_to,
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

        fn suggest(ctx: Context) -> Vec<Suggestion> {
            with(|p: &mut P| p.suggest(ctx))
        }

        fn status(ping: Ping) -> Option<StatusOverride> {
            with(|p: &mut P| p.status(ping))
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

    /// The visible text of a JSON text component, without formatting.
    pub(crate) fn words(json: &str) -> String {
        fn collect(value: &serde_json::Value, out: &mut String) {
            match value {
                serde_json::Value::String(text) => out.push_str(text),
                serde_json::Value::Array(items) => items.iter().for_each(|item| collect(item, out)),
                serde_json::Value::Object(fields) => {
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
