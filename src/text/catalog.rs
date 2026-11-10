//! Rift's own messages and the theme.
//!
//! Every text Rift shows players on its own behalf is a [`Notice`] with a
//! key, the arguments it fills in and a default English template in markup.
//! Operators replace templates and theme styles in the configuration:
//!
//! ```lua
//! text = {
//!   theme = { accent = "<#ffaa00>", bad = "<#ff5555><bold>" },
//!   messages = {
//!     ["queue.joined"] = "<good>Queued for <accent><server></accent>. Position: <position>.",
//!     ["login.maintenance"] = "<gradient:gold:red>Maintenance</gradient><newline><muted>Back soon.",
//!   },
//! }
//! ```
//!
//! Templates are checked when the configuration loads: an unknown key, an
//! unknown tag or an argument the message does not have is an error then,
//! not when a player triggers it. The defaults keep Rift's wording from
//! before the catalogue and are unstyled, so nothing changes until an
//! operator styles them.
use super::{
    Text,
    markup::{self, Arg, Args, Styles},
};
use serde_json::Value as Json;
use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock, RwLock},
};

macro_rules! notices {
    ($($variant:ident => $key:literal [$($arg:literal),*] $text:literal,)*) => {
        /// A message Rift shows players on its own behalf.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Notice {
            $($variant,)*
        }

        impl Notice {
            pub const ALL: &'static [Notice] = &[$(Notice::$variant,)*];

            /// The configuration key, such as `queue.joined`.
            pub fn key(self) -> &'static str {
                match self {
                    $(Notice::$variant => $key,)*
                }
            }

            /// The arguments the template may use, such as `<server>`.
            pub fn args(self) -> &'static [&'static str] {
                match self {
                    $(Notice::$variant => &[$($arg),*],)*
                }
            }

            /// The built-in English template.
            pub fn default_markup(self) -> &'static str {
                match self {
                    $(Notice::$variant => $text,)*
                }
            }
        }
    };
}

notices! {
    // Logins and the handshake.
    LoginMaintenance => "login.maintenance" []
        "The network is under maintenance. Please try again later.",
    LoginRateLimited => "login.rate_limited" []
        "Too many login attempts. Please wait before reconnecting.",
    LoginUnknownHost => "login.unknown_host" []
        "No server is configured for this hostname.",
    LoginForwardingVersion => "login.forwarding_version" []
        "This server requires Minecraft 1.13 or newer.",
    LoginInvalidStart => "login.invalid_start" []
        "Invalid or missing login start.",
    LoginAuthDenied => "login.auth_denied" []
        "Unable to verify your Minecraft account. Please sign in again.",
    LoginAuthInvalid => "login.auth_invalid" []
        "Invalid authentication response. Please reconnect.",
    LoginAuthUnavailable => "login.auth_unavailable" []
        "Minecraft authentication is unavailable or timed out. Please try again later.",
    LoginExtensionsVersion => "login.extensions_version" []
        "Player hooks are unavailable for this Minecraft version.",
    LoginExtensionFailed => "login.extension_failed" []
        "Login extension failed. Please try again later.",
    LoginNoAccess => "login.no_access" []
        "You do not have access to this server.",
    LoginDevMode => "login.dev_mode" []
        "This server is in development mode.",
    LoginDuplicateName => "login.duplicate_name" []
        "That player is already connected to this network.",
    LoginInvalidName => "login.invalid_name" []
        "Invalid player name.",
    LoginUnavailable => "login.unavailable" []
        "The server is currently unavailable. Please try again later.",
    LoginAlreadyConnected => "login.already_connected" []
        "This player is already connected.",
    LoginUnsupportedVersion => "login.unsupported_version" []
        "Unsupported Minecraft version. Rift supports Java 1.8.9–26.3.",
    LoginTransferVersion => "login.transfer_version" []
        "Transfer requires Minecraft 1.20.5 or newer.",
    DecisionUnavailable => "decision.unavailable" []
        "Unable to check your access, please try again later.",
    // Connections in play.
    ConnectionLostNoFallback => "connection.lost_no_fallback" []
        "The server connection was lost and no fallback was available.",
    ConnectionLost => "connection.lost" []
        "The server connection was lost. Please reconnect.",
    ConnectionFailed => "connection.failed" []
        "The server connection failed. Please reconnect.",
    ConnectionBackendClosed => "connection.backend_closed" []
        "The current backend disconnected the player.",
    ConnectionDenied => "connection.denied" []
        "The destination server denied access.",
    // Switching servers.
    SwitchIncomplete => "switch.incomplete" []
        "The server switch could not be completed.",
    SwitchStarting => "switch.starting" []
        "A server is starting. Please wait for the current transfer.",
    SwitchFailed => "switch.failed" ["reason"]
        "Unable to switch servers: <reason>",
    SwitchBusy => "switch.busy" []
        "The current server is busy. Please retry the switch.",
    SwitchNoServer => "switch.no_server" []
        "No available server could be reached.",
    // Proxy commands.
    ServerList => "server.list" ["current", "servers"]
        "Current server: <current>. Available servers: <servers>",
    ServerAlreadyOn => "server.already_on" ["current"]
        "You are already on <current>.",
    ServerUnavailable => "server.unavailable" []
        "Server unavailable or you do not have access to it.",
    ServerUsage => "server.usage" []
        "Usage: /server [name] or /hub",
    HubAlready => "hub.already" []
        "You are already in a hub.",
    CommandFailed => "command.failed" []
        "Extension command failed.",
    CommandNoPermission => "command.no_permission" []
        "You do not have permission to use this command.",
    // Queues.
    QueueJoined => "queue.joined" ["server", "position"]
        "Queued for <server>. Position: <position>.",
    QueueLeft => "queue.left" []
        "Left the queue.",
    QueueNoAccess => "queue.no_access" []
        "You do not have access to that server.",
    QueueTransferFailed => "queue.transfer_failed" ["reason"]
        "Queue transfer failed: <reason>",
    QueueExpired => "queue.expired" []
        "Queue expired after five minutes.",
    QueueNotConfigured => "queue.not_configured" []
        "No queue is configured for that server.",
    QueueAlreadyOn => "queue.already_on" []
        "You are already on that server.",
    QueueFull => "queue.full" []
        "Queue is full.",
    QueueServerFull => "queue.server_full" []
        "Server is full; use the queue command.",
    // The server list.
    StatusDefault => "status.motd" []
        "Rift",
    StatusVersion => "status.version" []
        "Rift",
    StatusMaintenance => "status.maintenance" []
        "The network is under maintenance.",
    StatusUnknownHost => "status.unknown_host" []
        "No server is configured for this hostname.",
    StatusUnavailable => "status.unavailable" []
        "The server is currently unavailable. Please try again later.",
}

impl Notice {
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|notice| notice.key() == key)
    }
}

/// The theme and the message templates in effect.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Catalog {
    styles: Styles,
    messages: BTreeMap<Notice, String>,
}

impl Catalog {
    /// A catalogue from theme styles and message templates by key, both
    /// checked here. Styles extend and replace the built-in theme.
    pub fn new<'a>(
        theme: impl IntoIterator<Item = (&'a str, &'a str)>,
        messages: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, String> {
        let mut styles = Styles::default();
        for (name, markup) in theme {
            styles
                .set(name, markup)
                .map_err(|error| format!("text.theme: {error}"))?;
        }
        let mut catalog = Self {
            styles,
            messages: BTreeMap::new(),
        };
        for (key, template) in messages {
            let notice = Notice::from_key(key).ok_or_else(|| {
                format!("text.messages: unknown message {key:?}; see docs/text.md for the keys")
            })?;
            catalog.check(notice, template)?;
            catalog.messages.insert(notice, template.to_owned());
        }
        Ok(catalog)
    }

    /// A template must parse with the message's arguments and no others.
    fn check(&self, notice: Notice, template: &str) -> Result<(), String> {
        let label = format!("text.messages[{:?}]", notice.key());
        if template.is_empty() || template.len() > 4096 {
            return Err(format!("{label} must contain 1..=4096 bytes"));
        }
        let args = sample_args(notice);
        let component = markup::parse(template, &args, &self.styles)
            .map_err(|error| format!("{label}: {error}"))?;
        Text::component(component, &label).map(drop)
    }

    pub fn styles(&self) -> &Styles {
        &self.styles
    }

    /// The template in effect for `notice`.
    pub fn markup(&self, notice: Notice) -> &str {
        self.messages
            .get(&notice)
            .map_or(notice.default_markup(), String::as_str)
    }

    /// `notice` with `args` filled in. Text that renders without formatting
    /// stays plain, as Rift sent it before the catalogue.
    pub fn render(&self, notice: Notice, args: &Args) -> Text {
        let label = notice.key();
        let rendered = |template: &str| {
            markup::parse(template, args, &self.styles)
                .and_then(|component| simplify(component, label))
        };
        rendered(self.markup(notice))
            .or_else(|_| rendered(notice.default_markup()))
            .unwrap_or_else(|_| Text::Plain(notice.default_markup().to_owned()))
    }
}

/// Each argument as placeholder text, for checking templates.
fn sample_args(notice: Notice) -> Args {
    let mut args = Args::new();
    for name in notice.args() {
        args.insert(name, Arg::Text("x".into()));
    }
    args
}

/// `{"text": "..."}` alone becomes plain text; anything else is checked as
/// a component.
fn simplify(component: Json, label: &str) -> Result<Text, String> {
    if let Json::Object(fields) = &component
        && let (1, Some(Json::String(text))) = (fields.len(), fields.get("text"))
        && !text.is_empty()
        && text.len() <= super::MAX_PLAIN_BYTES
    {
        return Ok(Text::Plain(text.clone()));
    }
    Text::component(component, label)
}

static CURRENT: LazyLock<RwLock<Arc<Catalog>>> =
    LazyLock::new(|| RwLock::new(Arc::new(Catalog::default())));

/// The catalogue of the configuration in effect.
pub fn current() -> Arc<Catalog> {
    CURRENT
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Make `catalog` the one in effect, when a configuration is published.
pub fn install(catalog: Arc<Catalog>) {
    *CURRENT
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = catalog;
}

thread_local! {
    /// The theme of a configuration being loaded on this thread, so its own
    /// markup uses its own theme before it is published.
    static LOADING: std::cell::RefCell<Option<Styles>> = const { std::cell::RefCell::new(None) };
}

/// Restores the theme in effect when dropped (see [`loading`]).
pub struct LoadingTheme(Option<Styles>);

impl Drop for LoadingTheme {
    fn drop(&mut self) {
        let previous = self.0.take();
        LOADING.with(|loading| *loading.borrow_mut() = previous);
    }
}

/// Use `styles` for markup parsed on this thread until the guard drops,
/// while the configuration that defines them loads.
pub fn loading(styles: Styles) -> LoadingTheme {
    LoadingTheme(LOADING.with(|loading| loading.borrow_mut().replace(styles)))
}

/// The theme in effect, for markup written by plugins and the configuration.
pub fn theme() -> Styles {
    LOADING
        .with(|loading| loading.borrow().clone())
        .unwrap_or_else(|| current().styles().clone())
}

/// `notice` without arguments.
pub fn notice(notice: Notice) -> Text {
    current().render(notice, &Args::new())
}

/// `notice` with `args`.
pub fn notice_with(notice: Notice, args: &Args) -> Text {
    current().render(notice, args)
}

/// The visible words of `notice`, for strings that cannot carry formatting
/// such as error messages and logs.
pub fn words(notice: Notice) -> String {
    self::notice(notice).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_render_as_plain_text_with_arguments() {
        let catalog = Catalog::default();
        assert_eq!(
            catalog.render(
                Notice::QueueJoined,
                &Args::new().text("server", "lobby").text("position", "3")
            ),
            Text::Plain("Queued for lobby. Position: 3.".into())
        );
        assert_eq!(
            catalog.render(
                Notice::SwitchFailed,
                &Args::new().component("reason", json!({"text": "Banned", "color": "red"}))
            ),
            Text::Component(json!({"text": "", "extra": [
                {"text": "Unable to switch servers: "},
                {"text": "Banned", "color": "red"},
            ]}))
        );
        // Every default parses with its arguments.
        for notice in Notice::ALL {
            catalog.check(*notice, notice.default_markup()).unwrap();
            assert!(matches!(
                catalog.render(*notice, &sample_args(*notice)),
                Text::Plain(_)
            ));
        }
    }

    #[test]
    fn keys_are_unique() {
        let mut keys: Vec<_> = Notice::ALL.iter().map(|notice| notice.key()).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), Notice::ALL.len());
    }

    #[test]
    fn operators_restyle_messages_and_the_theme() {
        let catalog = Catalog::new(
            [("accent", "<#ffaa00><bold>")],
            [(
                "queue.joined",
                "<good>Queued for <accent><server></accent> (#<position>)",
            )],
        )
        .unwrap();
        assert_eq!(
            catalog.render(
                Notice::QueueJoined,
                &Args::new().text("server", "<red>x").text("position", "1")
            ),
            Text::Component(json!({"text": "", "extra": [
                {"text": "Queued for ", "color": "green"},
                {"text": "<red>x", "color": "#ffaa00", "bold": true},
                {"text": " (#1)", "color": "green"},
            ]}))
        );
    }

    #[test]
    fn bad_templates_fail_at_load() {
        assert_eq!(
            Catalog::new([], [("queue.nope", "x")]).unwrap_err(),
            "text.messages: unknown message \"queue.nope\"; see docs/text.md for the keys"
        );
        assert_eq!(
            Catalog::new([], [("queue.left", "Left <server>")]).unwrap_err(),
            "text.messages[\"queue.left\"]: <server> at byte 5: unknown tag; not built in, an \
             argument or a style"
        );
        assert_eq!(
            Catalog::new([], [("queue.left", "<color:orange>x")]).unwrap_err(),
            "text.messages[\"queue.left\"]: <color:orange> at byte 0: \"orange\" is not a colour \
             name or #rrggbb"
        );
        assert!(Catalog::new([("bad", "<red>words")], []).is_err());
        assert!(Catalog::new([], [("queue.left", "")]).is_err());
    }

    #[test]
    fn documented_examples_load() {
        // docs/text.md and docs/extensions.md.
        Catalog::new(
            [
                ("accent", "<#ffaa00>"),
                ("bad", "<#ff5555><bold>"),
                ("link", "<aqua><u>"),
            ],
            [
                (
                    "login.maintenance",
                    "<gradient:gold:red>Maintenance</gradient><br><muted>Back soon.",
                ),
                (
                    "queue.joined",
                    "<good>Queued for <accent><server></accent>. Position: <position>.",
                ),
                ("switch.failed", "<bad>Could not switch: <reason>"),
            ],
        )
        .unwrap();
        let invite = markup::parse(
            "<inviter> invited you to their party. \
             <green><b><click:run_command:/party accept><hover:show_text:'Join the party'>\
             [Accept]</hover></click></b></green> \
             <red><b><click:run_command:/party decline>[Decline]</click></b></red>",
            &Args::new().text("inviter", "Steve"),
            &Styles::default(),
        )
        .unwrap();
        let accept = &invite["extra"][1];
        assert_eq!(accept["text"], "[Accept]");
        assert_eq!(accept["click_event"]["command"], "/party accept");
        assert_eq!(accept["hover_event"]["value"]["text"], "Join the party");
    }
}
