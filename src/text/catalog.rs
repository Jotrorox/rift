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
//! `text.languages` holds the same messages in other languages, by tag:
//!
//! ```lua
//! text = {
//!   languages = {
//!     de = { ["queue.left"] = "Warteschlange verlassen." },
//!   },
//! }
//! ```
//!
//! A player's session renders notices in the player's language (the one a
//! plugin set with `players.set-language`, else the client's locale): the
//! exact tag (`de_at`), then its language (`de`), then `text.messages`.
//!
//! Templates are checked when the configuration loads: an unknown key, an
//! unknown tag or an argument the message does not have is an error then,
//! not when a player triggers it. The defaults keep Rift's wording from
//! before the catalogue and are unstyled, so nothing changes until an
//! operator styles them.
//!
//! The catalogue also holds each enabled plugin's `messages.toml`
//! ([`messages`]) under the plugin's folder name, its namespace. The
//! operator replaces those in the same tables by qualified key:
//!
//! ```lua
//! text = {
//!   messages = { ["party:invited"] = "<accent><from></accent> wants you in their party." },
//!   languages = { fr = { ["party:invited"] = "<accent><from></accent> vous invite." } },
//! }
//! ```
//!
//! For each step of the language order the operator's template wins over
//! the plugin's: `text.languages[tag]`, then the plugin's
//! `[languages.<tag>]`, the same pair for the language part, then
//! `text.messages` and the plugin's `[messages]`. Callers send any of them
//! with [`Catalog::message`].
use super::{
    Text,
    markup::{self, Arg, Args, Style, Styles},
    messages::{self, Message, Messages},
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
    LoginVersion => "login.version" []
        "This server does not accept your Minecraft version.",
    LoginNoVersion => "login.no_version" []
        "No server on this network supports your Minecraft version.",
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
    ConnectionKicked => "connection.kicked" ["server", "reason"]
        "You were kicked from <server>: <reason>",
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
    ChatFailed => "chat.failed" []
        "Your message could not be checked and was not sent.",
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

/// The theme, Rift's own message templates and every enabled plugin's
/// catalogue, with the operator's replacements for both.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Catalog {
    styles: Styles,
    messages: BTreeMap<Notice, String>,
    /// `text.languages`: templates by lowercase language tag.
    languages: BTreeMap<String, BTreeMap<Notice, String>>,
    /// Plugin catalogues by namespace (the plugin's folder name).
    namespaces: BTreeMap<String, Namespace>,
    /// Replacements for a namespace no enabled plugin has; `rift check
    /// --strict` makes them errors.
    warnings: Vec<String>,
}

/// One plugin's `messages.toml` and the operator's replacements for it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Namespace {
    plugin: Messages,
    /// `text.messages["namespace:key"]` by key.
    replaced: BTreeMap<String, String>,
    /// `text.languages[tag]["namespace:key"]` by tag, then key.
    replaced_in: BTreeMap<String, BTreeMap<String, String>>,
}

impl Catalog {
    /// A catalogue from theme styles and message templates by key, both
    /// checked here. Styles extend and replace the built-in theme. A
    /// qualified key (`party:invited`) replaces a plugin's message; see
    /// [`Catalog::with_messages`].
    pub fn new<'a>(
        theme: impl IntoIterator<Item = (&'a str, &'a str)>,
        messages: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, String> {
        Self::themed(theme)?.with_messages(messages)
    }

    /// A catalogue with theme styles only, which extend and replace the
    /// built-in theme. Add plugins next, then the operator's replacements.
    pub fn themed<'a>(theme: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<Self, String> {
        let mut styles = Styles::default();
        for (name, markup) in theme {
            styles
                .set(name, markup)
                .map_err(|error| format!("text.theme: {error}"))?;
        }
        Ok(Self {
            styles,
            ..Self::default()
        })
    }

    /// Add an enabled plugin's catalogue under its namespace, the plugin's
    /// folder name. Every template must parse with the theme and with the
    /// arguments and styles its message declares. Add plugins before the
    /// operator's replacements, which are checked against them.
    pub fn with_plugin(mut self, namespace: &str, messages: Messages) -> Result<Self, String> {
        let label = format!("plugin {namespace}: {}", messages::FILE);
        for (key, message) in messages.iter() {
            self.check_message(
                &format!("{label}: [messages] {key:?}"),
                message,
                &message.text,
            )?;
            for (tag, template) in &message.languages {
                self.check_message(
                    &format!("{label}: [languages.{tag}] {key:?}"),
                    message,
                    template,
                )?;
            }
        }
        self.namespaces.insert(
            namespace.to_owned(),
            Namespace {
                plugin: messages,
                ..Namespace::default()
            },
        );
        Ok(self)
    }

    /// Add `text.messages`. An unqualified key is one of Rift's own
    /// messages; `namespace:key` replaces a plugin's. A key the namespace of
    /// an added plugin lacks is an error; a namespace no added plugin has is
    /// a [warning](Catalog::warnings) and the replacement is ignored.
    pub fn with_messages<'a>(
        mut self,
        messages: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, String> {
        let path = "text.messages";
        for (key, template) in messages {
            match qualified(path, key)? {
                None => {
                    let notice = Notice::from_key(key).ok_or_else(|| {
                        format!("{path}: unknown message {key:?}; see docs/text.md for the keys")
                    })?;
                    self.check(notice, template)?;
                    self.messages.insert(notice, template.to_owned());
                }
                Some((namespace, name)) => {
                    if self.check_replacement(path, namespace, name, template)? {
                        self.namespace_mut(namespace)
                            .replaced
                            .insert(name.to_owned(), template.to_owned());
                    }
                }
            }
        }
        Ok(self)
    }

    /// Add the messages of one language (`text.languages[tag]`), checked
    /// like `text.messages`.
    pub fn with_language<'a>(
        mut self,
        tag: &str,
        messages: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, String> {
        let language = crate::directory::language_tag(tag)
            .map_err(|error| format!("text.languages: {tag:?}: {error}"))?;
        let path = format!("text.languages.{language}");
        let mut templates = BTreeMap::new();
        for (key, template) in messages {
            match qualified(&path, key)? {
                None => {
                    let notice = Notice::from_key(key).ok_or_else(|| {
                        format!("{path}: unknown message {key:?}; see docs/text.md for the keys")
                    })?;
                    self.check_at(&path, notice, template)?;
                    templates.insert(notice, template.to_owned());
                }
                Some((namespace, name)) => {
                    if self.check_replacement(&path, namespace, name, template)? {
                        self.namespace_mut(namespace)
                            .replaced_in
                            .entry(language.clone())
                            .or_default()
                            .insert(name.to_owned(), template.to_owned());
                    }
                }
            }
        }
        self.languages.insert(language, templates);
        Ok(self)
    }

    /// The language tags with messages of their own.
    pub fn languages(&self) -> impl Iterator<Item = &str> {
        self.languages.keys().map(String::as_str)
    }

    /// The catalogue of the plugin `namespace`, if an enabled plugin has it
    /// (a plugin without `messages.toml` has an empty one).
    pub fn namespace(&self, namespace: &str) -> Option<&Messages> {
        self.namespaces
            .get(namespace)
            .map(|namespace| &namespace.plugin)
    }

    /// Every enabled plugin's catalogue by namespace, sorted; for message
    /// and language counts.
    pub fn namespaces(&self) -> impl Iterator<Item = (&str, &Messages)> {
        self.namespaces
            .iter()
            .map(|(name, namespace)| (name.as_str(), &namespace.plugin))
    }

    /// Replacements Rift ignored because no enabled plugin has their
    /// namespace. `rift check` and every load print them; `rift check
    /// --strict` fails on them.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// A template must parse with the message's arguments and no others.
    fn check(&self, notice: Notice, template: &str) -> Result<(), String> {
        self.check_at("text.messages", notice, template)
    }

    fn check_at(&self, path: &str, notice: Notice, template: &str) -> Result<(), String> {
        let label = format!("{path}[{:?}]", notice.key());
        if template.is_empty() || template.len() > 4096 {
            return Err(format!("{label} must contain 1..=4096 bytes"));
        }
        let args = sample_args(notice);
        let component = markup::parse(template, &args, &self.styles)
            .map_err(|error| format!("{label}: {error}"))?;
        Text::component(component, &label).map(drop)
    }

    /// A plugin template must parse with the arguments and styles its
    /// message declares and no others.
    fn check_message(&self, label: &str, message: &Message, template: &str) -> Result<(), String> {
        if template.is_empty() || template.len() > messages::MAX_TEMPLATE_BYTES {
            return Err(format!(
                "{label} must contain 1..={} bytes",
                messages::MAX_TEMPLATE_BYTES
            ));
        }
        let component = markup::parse(template, &message_samples(message), &self.styles)
            .map_err(|error| format!("{label}: {error}"))?;
        Text::component(component, label).map(drop)
    }

    /// Check the operator's `template` for `namespace:name` at `path`.
    /// `Ok(false)`: no added plugin has the namespace, so the replacement
    /// is ignored with a warning.
    fn check_replacement(
        &mut self,
        path: &str,
        namespace: &str,
        name: &str,
        template: &str,
    ) -> Result<bool, String> {
        let label = format!("{path}[\"{namespace}:{name}\"]");
        let Some(space) = self.namespaces.get(namespace) else {
            self.warnings.push(format!(
                "{label}: no enabled plugin has the namespace {namespace:?}; the replacement is ignored"
            ));
            return Ok(false);
        };
        let message = space.plugin.get(name).ok_or_else(|| {
            format!(
                "{path}: unknown message \"{namespace}:{name}\"; plugin {namespace} has no message {name:?} in its {}",
                messages::FILE
            )
        })?;
        self.check_message(&label, message, template)?;
        Ok(true)
    }

    /// The namespace of an added plugin.
    fn namespace_mut(&mut self, namespace: &str) -> &mut Namespace {
        self.namespaces
            .get_mut(namespace)
            .expect("checked by check_replacement")
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

    /// The template for `notice` in `language`: the exact tag, then the
    /// part before its first `_`, then [`Catalog::markup`].
    pub fn markup_in(&self, notice: Notice, language: Option<&str>) -> &str {
        first_template(language, |tag| match tag {
            Some(tag) => self.languages.get(tag)?.get(&notice).map(String::as_str),
            None => Some(self.markup(notice)),
        })
        .unwrap_or(notice.default_markup())
    }

    /// `notice` with `args` filled in. Text that renders without formatting
    /// stays plain, as Rift sent it before the catalogue.
    pub fn render(&self, notice: Notice, args: &Args) -> Text {
        self.render_in(notice, args, language().as_deref())
    }

    /// [`Catalog::render`] in an explicit language instead of the session's.
    pub fn render_in(&self, notice: Notice, args: &Args, language: Option<&str>) -> Text {
        self.render_template(
            notice.key(),
            self.markup_in(notice, language),
            notice.default_markup(),
            args,
            || notice.default_markup().to_owned(),
        )
    }

    /// The message `key` as a JSON text component, ready to splice into a
    /// larger component; see [`Catalog::message_text`].
    pub fn message(
        &self,
        caller: Option<&str>,
        key: &str,
        args: &Args,
        language: Option<&str>,
    ) -> Result<Json, String> {
        Ok(match self.message_text(caller, key, args, language)? {
            Text::Plain(text) => serde_json::json!({ "text": text }),
            Text::Component(component) => component,
        })
    }

    /// The message `key`, as written by a caller in the namespace `caller`
    /// (a plugin's folder name; `None` for `rift.lua` and bus callers),
    /// rendered with `args` in `language` (a normalised tag such as
    /// `de_at`; `None` for the default) and the theme.
    ///
    /// - A key without `:` belongs to the caller's own namespace, which a
    ///   caller without one cannot use.
    /// - `namespace:key` names a plugin's message: `party:invited`.
    /// - `rift:key` names one of Rift's own notices as the operator styled
    ///   it: `rift:queue.full`. Its arguments are the notice's, and it has
    ///   no styles.
    ///
    /// The call is checked: an unknown namespace or key, a missing
    /// argument, an argument the message does not declare, and content
    /// where it declares a style (or a style where it declares content)
    /// are errors. Templates are looked up in the operator's
    /// `text.languages[tag]`, the plugin's `[languages.<tag>]`, the same
    /// pair for the language part of the tag, then `text.messages` and the
    /// plugin's `[messages]`. Once a call is accepted only size can fail a
    /// render; Rift then falls back to the plugin's default template, then
    /// to its words.
    pub fn message_text(
        &self,
        caller: Option<&str>,
        key: &str,
        args: &Args,
        language: Option<&str>,
    ) -> Result<Text, String> {
        match key.split_once(':') {
            Some((messages::RESERVED, name)) => self.notice_message(name, args, language),
            Some((namespace, name)) => self.plugin_message(namespace, name, args, language),
            None => match caller {
                Some(caller) => self.plugin_message(caller, key, args, language),
                None => Err(format!(
                    "message {key:?}: a key without a namespace names the caller's own message, and only a persistent plugin has messages of its own; elsewhere write namespace:key, such as \"party:invited\" or \"rift:queue.full\""
                )),
            },
        }
    }

    fn notice_message(
        &self,
        name: &str,
        args: &Args,
        language: Option<&str>,
    ) -> Result<Text, String> {
        let label = format!("message \"{}:{name}\"", messages::RESERVED);
        let notice = Notice::from_key(name).ok_or_else(|| {
            format!("{label}: Rift has no such message; see docs/text.md for the keys")
        })?;
        check_call(&label, args, notice.args(), &[])?;
        Ok(self.render_in(notice, args, language))
    }

    fn plugin_message(
        &self,
        namespace: &str,
        name: &str,
        args: &Args,
        language: Option<&str>,
    ) -> Result<Text, String> {
        let label = format!("message \"{namespace}:{name}\"");
        let space = self
            .namespaces
            .get(namespace)
            .ok_or_else(|| format!("{label}: no enabled plugin has the namespace {namespace:?}"))?;
        let message = space.plugin.get(name).ok_or_else(|| {
            format!(
                "{label}: plugin {namespace} has no such message in its {}",
                messages::FILE
            )
        })?;
        let content: Vec<&str> = message.args.iter().map(String::as_str).collect();
        let styles: Vec<&str> = message.styles.iter().map(String::as_str).collect();
        check_call(&label, args, &content, &styles)?;
        let template = first_template(language, |tag| match tag {
            Some(tag) => space
                .replaced_in
                .get(tag)
                .and_then(|templates| templates.get(name))
                .or_else(|| message.languages.get(tag))
                .map(String::as_str),
            None => Some(space.replaced.get(name).unwrap_or(&message.text).as_str()),
        })
        .unwrap_or(message.text.as_str());
        Ok(
            self.render_template(&label, template, &message.text, args, || {
                self.words(name, message)
            }),
        )
    }

    /// `template` with `args`; when it does not render (only size can fail
    /// a checked template), `default`, then `words`.
    fn render_template(
        &self,
        label: &str,
        template: &str,
        default: &str,
        args: &Args,
        words: impl FnOnce() -> String,
    ) -> Text {
        let rendered = |template: &str| {
            markup::parse(template, args, &self.styles)
                .and_then(|component| simplify(component, label))
        };
        rendered(template)
            .or_else(|_| rendered(default))
            .unwrap_or_else(|_| Text::Plain(words()))
    }

    /// The words of a plugin message's default template with empty
    /// arguments, as plain text within its limit; the key when it has none.
    fn words(&self, key: &str, message: &Message) -> String {
        let mut args = Args::new();
        for name in &message.args {
            args.insert(name, Arg::Text(String::new()));
        }
        for name in &message.styles {
            args.insert(name, Arg::Style(Style::default()));
        }
        let mut words = markup::parse(&message.text, &args, &self.styles)
            .map(|component| Text::Component(component).to_string())
            .unwrap_or_default();
        let mut end = words.len().min(super::MAX_PLAIN_BYTES);
        while !words.is_char_boundary(end) {
            end -= 1;
        }
        words.truncate(end);
        if words.is_empty() {
            key.clone_into(&mut words);
        }
        words
    }
}

/// The first template `at` has for `language`: at the exact tag, then at
/// the part before its first `_`, then at `None`, the default.
fn first_template<'a>(
    language: Option<&str>,
    at: impl Fn(Option<&str>) -> Option<&'a str>,
) -> Option<&'a str> {
    language
        .and_then(|tag| at(Some(tag)).or_else(|| at(Some(tag.split('_').next()?))))
        .or_else(|| at(None))
}

/// The namespace and key of a configuration table's `namespace:key`, or
/// `None` for an unqualified key: one of Rift's own messages.
fn qualified<'k>(path: &str, key: &'k str) -> Result<Option<(&'k str, &'k str)>, String> {
    let Some((namespace, name)) = key.split_once(':') else {
        return Ok(None);
    };
    if namespace == messages::RESERVED {
        return Err(format!(
            "{path}: {key:?}: write Rift's own messages without \"{}:\", as {name:?}",
            messages::RESERVED
        ));
    }
    if !crate::plugins::manifest::valid_name(namespace) || !messages::valid_key(name) {
        return Err(format!(
            "{path}: {key:?} is not a message key; a plugin's message is namespace:key, such as \"party:invited\""
        ));
    }
    Ok(Some((namespace, name)))
}

/// A call must give every declared argument, content for content and a
/// style for a style, and nothing else.
fn check_call(label: &str, args: &Args, content: &[&str], styles: &[&str]) -> Result<(), String> {
    for name in args.names() {
        let style = matches!(args.get(name), Some(Arg::Style(_)));
        if content.contains(&name) {
            if style {
                return Err(format!(
                    "{label}: <{name}> is a content argument, not a style"
                ));
            }
        } else if styles.contains(&name) {
            if !style {
                return Err(format!(
                    "{label}: <{name}> is a style argument; pass a style, not content"
                ));
            }
        } else {
            return Err(format!("{label}: the message has no argument <{name}>"));
        }
    }
    if let Some(name) = content
        .iter()
        .chain(styles)
        .find(|name| args.get(name).is_none())
    {
        return Err(format!("{label}: argument <{name}> is missing"));
    }
    Ok(())
}

/// Each argument of a plugin message as placeholder text and each style as
/// an empty style, for checking templates.
fn message_samples(message: &Message) -> Args {
    let mut args = Args::new();
    for name in &message.args {
        args.insert(name, Arg::Text("x".into()));
    }
    for name in &message.styles {
        args.insert(name, Arg::Style(Style::default()));
    }
    args
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
    /// The catalogue of a configuration being loaded on this thread, so its
    /// own text uses its own theme and messages before it is published.
    static LOADING: std::cell::RefCell<Option<Arc<Catalog>>> = const { std::cell::RefCell::new(None) };
}

/// Restores the catalogue in effect when dropped (see [`loading`]).
pub struct LoadingCatalog(Option<Arc<Catalog>>);

impl Drop for LoadingCatalog {
    fn drop(&mut self) {
        let previous = self.0.take();
        LOADING.with(|loading| *loading.borrow_mut() = previous);
    }
}

/// Use `catalog` (its theme and messages) for text converted on this thread
/// until the guard drops, while the configuration that defines it loads.
pub fn loading(catalog: Arc<Catalog>) -> LoadingCatalog {
    LoadingCatalog(LOADING.with(|loading| loading.borrow_mut().replace(catalog)))
}

/// The catalogue text uses on this thread: the one being loaded, else
/// [`current`].
pub fn active() -> Arc<Catalog> {
    LOADING
        .with(|loading| loading.borrow().clone())
        .unwrap_or_else(current)
}

/// The theme in effect, for markup written by plugins and the configuration.
pub fn theme() -> Styles {
    active().styles().clone()
}

tokio::task_local! {
    /// The language of the player whose session runs on this task.
    static LANGUAGE: std::cell::RefCell<Option<String>>;
}

/// Run a player's session so that notices rendered on its task use the
/// player's language once [`set_language`] names it.
pub async fn session<F: Future>(session: F) -> F::Output {
    LANGUAGE.scope(std::cell::RefCell::new(None), session).await
}

/// The language of this task's player; nothing happens outside [`session`].
pub fn set_language(language: Option<&str>) {
    let _ = LANGUAGE.try_with(|current| {
        if current.borrow().as_deref() != language {
            *current.borrow_mut() = language.map(str::to_owned);
        }
    });
}

/// The language of this task's player, if a session set one.
pub fn language() -> Option<String> {
    LANGUAGE
        .try_with(|current| current.borrow().clone())
        .ok()
        .flatten()
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

/// A plugin catalogue for tests that send plugin messages.
#[cfg(test)]
pub(crate) const TEST_MESSAGES: &str = r#"
[messages]
created = "Created."
greet = { text = "Hello <name>.", args = ["name"] }
denied = "No entry."
tagged = { text = "<team>[<tag>]</team>", args = ["tag"], styles = ["team"] }

[languages.de]
created = "Erstellt."
greet = "Hallo <name>."
denied = "Kein Zutritt."
"#;

/// The example plugins whose `messages.toml` [`test_catalog`] adds under
/// their folder names (`examples/plugins/<name>`).
#[cfg(test)]
const EXAMPLE_PLUGINS: &[&str] = &["hello-rs", "hello-c", "hello-go", "hello-lua"];

/// The built-in catalogue with [`TEST_MESSAGES`] as the plugins `party`
/// and `t` (the folder the plugin tests install), and each example plugin's
/// own `messages.toml` as shipped.
#[cfg(test)]
pub(crate) fn test_catalog() -> Catalog {
    let messages = |namespace: &str| Messages::parse(TEST_MESSAGES, namespace).unwrap();
    let mut catalog = Catalog::default()
        .with_plugin("party", messages("party"))
        .unwrap()
        .with_plugin("t", messages("t"))
        .unwrap();
    let examples = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/plugins");
    for name in EXAMPLE_PLUGINS {
        let example = messages::read(&examples.join(name), name)
            .unwrap_or_else(|| panic!("examples/plugins/{name} has no {}", messages::FILE))
            .unwrap();
        catalog = catalog.with_plugin(name, example).unwrap();
    }
    catalog
}

/// Install [`test_catalog`] for the whole test process, for tests whose
/// text is converted on threads they do not own (plugin threads). It only
/// adds plugin namespaces, so tests that use Rift's own messages see the
/// built-in ones. Add a namespace a test needs to [`test_catalog`] rather
/// than installing another catalogue: tests run in parallel and share this
/// one.
#[cfg(test)]
pub(crate) fn install_test_catalog() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| install(Arc::new(test_catalog())));
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
    fn languages_fall_back_from_region_to_language_to_the_default() {
        let catalog = Catalog::new([], [("queue.left", "Left.")])
            .unwrap()
            .with_language(
                "de",
                [("queue.left", "Verlassen."), ("queue.full", "Voll.")],
            )
            .unwrap()
            .with_language("de-AT", [("queue.left", "<bad>Servus.")])
            .unwrap();
        let text = |notice, language| catalog.render_in(notice, &Args::new(), language);
        assert_eq!(text(Notice::QueueLeft, None), Text::Plain("Left.".into()));
        assert_eq!(
            text(Notice::QueueLeft, Some("de_de")),
            Text::Plain("Verlassen.".into())
        );
        assert!(matches!(
            text(Notice::QueueLeft, Some("de_at")),
            Text::Component(_)
        ));
        assert_eq!(
            text(Notice::QueueFull, Some("de_at")),
            Text::Plain("Voll.".into())
        );
        assert_eq!(
            text(Notice::QueueLeft, Some("fr_fr")),
            Text::Plain("Left.".into())
        );
        // A message a language lacks comes from `text.messages` or English.
        assert_eq!(
            text(Notice::QueueExpired, Some("de")),
            Text::Plain(Notice::QueueExpired.default_markup().into())
        );
        assert_eq!(catalog.languages().collect::<Vec<_>>(), ["de", "de_at"]);
        assert_eq!(
            Catalog::default()
                .with_language("de", [("queue.left", "Left <server>")])
                .unwrap_err(),
            "text.languages.de[\"queue.left\"]: <server> at byte 5: unknown tag; not built in, \
             an argument or a style"
        );
        assert!(Catalog::default().with_language("x", []).is_err());
        assert!(
            Catalog::default()
                .with_language("de", [("nope", "x")])
                .unwrap_err()
                .starts_with("text.languages.de: unknown message")
        );
    }

    #[tokio::test]
    async fn a_session_renders_in_its_players_language() {
        let catalog = Catalog::default()
            .with_language("de", [("queue.left", "Verlassen.")])
            .unwrap();
        // Outside a session there is no language to set.
        set_language(Some("de"));
        assert_eq!(language(), None);
        session(async {
            assert_eq!(
                catalog.render(Notice::QueueLeft, &Args::new()),
                Text::Plain("Left the queue.".into())
            );
            set_language(Some("de_de"));
            assert_eq!(language().as_deref(), Some("de_de"));
            assert_eq!(
                catalog.render(Notice::QueueLeft, &Args::new()),
                Text::Plain("Verlassen.".into())
            );
            set_language(None);
            assert_eq!(language(), None);
        })
        .await;
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
        // docs/text.md and docs/players-api.md.
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
        // docs/text.md, "Plugin messages".
        let party = Messages::parse(
            r#"
[messages]
created = "<good>Party created."
invited = { text = "<accent><from></accent> invited you to a party.", args = ["from"] }
full    = { text = "<bad>The party is full (<size>/<max>).", args = ["size", "max"] }
tagged  = { text = "<team>[<tag>]</team> <name>", args = ["tag", "name"], styles = ["team"] }

[languages.de]
created = "<good>Party erstellt."
invited = "<accent><from></accent> hat dich in eine Party eingeladen."
"#,
            "party",
        )
        .unwrap();
        let catalog = Catalog::themed([])
            .unwrap()
            .with_plugin("party", party)
            .unwrap()
            .with_messages([(
                "party:invited",
                "<accent><from></accent> wants you in their party.",
            )])
            .unwrap()
            .with_language(
                "fr",
                [("party:invited", "<accent><from></accent> vous invite.")],
            )
            .unwrap();
        assert!(catalog.warnings().is_empty());
        let invited = |language| {
            catalog
                .message_text(
                    Some("party"),
                    "invited",
                    &Args::new().text("from", "Steve"),
                    language,
                )
                .unwrap()
                .to_string()
        };
        assert_eq!(invited(None), "Steve wants you in their party.");
        assert_eq!(invited(Some("fr_fr")), "Steve vous invite.");
        assert_eq!(
            invited(Some("de_de")),
            "Steve hat dich in eine Party eingeladen."
        );
    }

    const PARTY: &str = r#"
[messages]
created = "Created."
invited = { text = "<accent><from></accent> invited you.", args = ["from"] }
tagged = { text = "<team>[<tag>]</team> <name>", args = ["tag", "name"], styles = ["team"] }

[languages.de]
created = "Erstellt."
invited = "<from> hat dich eingeladen."

[languages.de_at]
created = "Gegründet."
"#;

    /// A catalogue with the plugin `party` enabled.
    fn party() -> Catalog {
        Catalog::themed([])
            .unwrap()
            .with_plugin("party", Messages::parse(PARTY, "party").unwrap())
            .unwrap()
    }

    #[test]
    fn plugin_messages_follow_the_language_order_with_operator_replacements() {
        let created = |catalog: &Catalog, language| {
            catalog
                .message_text(Some("party"), "created", &Args::new(), language)
                .unwrap()
        };
        let plain = |text: &str| Text::Plain(text.into());
        let catalog = party();
        assert_eq!(created(&catalog, None), plain("Created."));
        assert_eq!(created(&catalog, Some("de_de")), plain("Erstellt."));
        assert_eq!(created(&catalog, Some("de_at")), plain("Gegründet."));
        assert_eq!(created(&catalog, Some("fr_fr")), plain("Created."));
        // Step 3: `text.messages` beats the plugin's default.
        let catalog = party().with_messages([("party:created", "Made.")]).unwrap();
        assert_eq!(created(&catalog, None), plain("Made."));
        assert_eq!(created(&catalog, Some("fr_fr")), plain("Made."));
        assert_eq!(created(&catalog, Some("de_de")), plain("Erstellt."));
        // Step 2: the operator's language part beats the plugin's, but not
        // the plugin's exact tag, which is step 1.
        let catalog = catalog
            .with_language("de", [("party:created", "Gemacht.")])
            .unwrap();
        assert_eq!(created(&catalog, Some("de_de")), plain("Gemacht."));
        assert_eq!(created(&catalog, Some("de")), plain("Gemacht."));
        assert_eq!(created(&catalog, Some("de_at")), plain("Gegründet."));
        // Step 1: the operator's exact tag beats everything.
        let catalog = catalog
            .with_language("de-AT", [("party:created", "Servus.")])
            .unwrap();
        assert_eq!(created(&catalog, Some("de_at")), plain("Servus."));
        assert_eq!(created(&catalog, Some("de_de")), plain("Gemacht."));
        assert_eq!(created(&catalog, Some("fr_fr")), plain("Made."));
        // A message the language lacks comes from the default.
        assert_eq!(
            catalog
                .message_text(Some("party"), "tagged", &tagged_args(), Some("de_at"))
                .unwrap()
                .to_string(),
            "[AB] Steve"
        );
        // Rift's own messages are untouched.
        assert_eq!(
            catalog.render_in(Notice::QueueLeft, &Args::new(), Some("de")),
            plain("Left the queue.")
        );
        assert_eq!(catalog.languages().collect::<Vec<_>>(), ["de", "de_at"]);
    }

    fn tagged_args() -> Args {
        Args::new()
            .style("team", markup::style_arg("<red><bold>").unwrap())
            .text("tag", "AB")
            .text("name", "Steve")
    }

    #[test]
    fn messages_are_sent_by_key_and_checked_at_the_call() {
        let catalog = party();
        let from = Args::new().text("from", "Steve");
        let invited = catalog
            .message(Some("party"), "invited", &from, None)
            .unwrap();
        assert_eq!(
            Text::Component(invited.clone()).to_string(),
            "Steve invited you."
        );
        assert!(
            invited.to_string().contains("\"color\":\"aqua\""),
            "{invited}"
        );
        // A qualified key reaches the same message from anywhere.
        assert_eq!(
            catalog.message(None, "party:invited", &from, None).unwrap(),
            invited
        );
        assert_eq!(
            catalog
                .message(Some("other"), "party:invited", &from, None)
                .unwrap(),
            invited
        );
        assert_eq!(
            catalog
                .message_text(Some("party"), "invited", &from, Some("de_de"))
                .unwrap(),
            Text::Plain("Steve hat dich eingeladen.".into())
        );
        // Plain text is a component with text alone.
        assert_eq!(
            catalog
                .message(None, "party:created", &Args::new(), None)
                .unwrap(),
            json!({"text": "Created."})
        );
        // `rift:` names Rift's own messages as the operator styled them.
        let queued = Args::new().text("server", "lobby").text("position", "3");
        assert_eq!(
            catalog
                .message_text(Some("party"), "rift:queue.joined", &queued, Some("de"))
                .unwrap(),
            catalog.render_in(Notice::QueueJoined, &queued, Some("de"))
        );
        // Style arguments wrap what the template puts inside them.
        let tagged = catalog
            .message(None, "party:tagged", &tagged_args(), None)
            .unwrap();
        assert_eq!(Text::Component(tagged.clone()).to_string(), "[AB] Steve");
        assert!(tagged.to_string().contains("\"color\":\"red\""), "{tagged}");
        let style = || Arg::Style(Style::default());
        let with = |args: &[(&str, Arg)]| {
            let mut all = Args::new();
            for (name, value) in args {
                all.insert(name, value.clone());
            }
            all
        };
        let text = |value: &str| Arg::Text(value.into());
        for (caller, key, args, expected) in [
            (
                None,
                "invited",
                from.clone(),
                "message \"invited\": a key without a namespace names the caller's own message",
            ),
            (
                Some("party"),
                "nope",
                Args::new(),
                "message \"party:nope\": plugin party has no such message in its messages.toml",
            ),
            (
                None,
                "other:created",
                Args::new(),
                "message \"other:created\": no enabled plugin has the namespace \"other\"",
            ),
            (
                Some("other"),
                "created",
                Args::new(),
                "no enabled plugin has the namespace \"other\"",
            ),
            (
                None,
                "rift:queue.nope",
                Args::new(),
                "message \"rift:queue.nope\": Rift has no such message",
            ),
            (
                None,
                "party:invited",
                Args::new(),
                "message \"party:invited\": argument <from> is missing",
            ),
            (
                None,
                "party:invited",
                with(&[("from", text("x")), ("to", text("y"))]),
                "message \"party:invited\": the message has no argument <to>",
            ),
            (
                None,
                "party:created",
                from.clone(),
                "the message has no argument <from>",
            ),
            (
                None,
                "party:invited",
                with(&[("from", style())]),
                "<from> is a content argument, not a style",
            ),
            (
                None,
                "party:tagged",
                with(&[("team", text("x")), ("tag", text("t")), ("name", text("n"))]),
                "<team> is a style argument",
            ),
            (
                None,
                "party:tagged",
                with(&[("tag", text("t")), ("name", text("n"))]),
                "argument <team> is missing",
            ),
            (
                None,
                "rift:queue.left",
                with(&[("server", text("x"))]),
                "message \"rift:queue.left\": the message has no argument <server>",
            ),
            (
                None,
                "rift:switch.failed",
                with(&[("reason", style())]),
                "<reason> is a content argument",
            ),
            (
                None,
                "rift:switch.failed",
                Args::new(),
                "argument <reason> is missing",
            ),
        ] {
            let error = catalog.message(caller, key, &args, None).unwrap_err();
            assert!(error.contains(expected), "{key}: {error}");
        }
    }

    #[test]
    fn a_message_that_does_not_render_with_its_arguments_falls_back() {
        // Too many painted characters for the replacement: the plugin's
        // default template.
        let catalog = party()
            .with_messages([(
                "party:invited",
                "<gradient:red:blue><from></gradient> invited you.",
            )])
            .unwrap();
        let long = Args::new().text("from", "x".repeat(1000));
        let text = catalog
            .message_text(None, "party:invited", &long, None)
            .unwrap();
        assert!(text.to_string().ends_with("x invited you."), "{text}");
        assert!(
            matches!(&text, Text::Component(component) if component.to_string().contains("aqua")),
            "{text:?}"
        );
        // Too large for every template: the default's words.
        let huge = Args::new().text("from", "x".repeat(40 * 1024));
        let text = catalog
            .message_text(None, "party:invited", &huge, None)
            .unwrap();
        assert!(
            matches!(&text, Text::Plain(words) if words.trim() == "invited you."),
            "{text:?}"
        );
    }

    #[test]
    fn plugin_templates_and_replacements_are_checked_at_load() {
        let plugin = |text: &str| {
            Catalog::themed([])
                .unwrap()
                .with_plugin("party", Messages::parse(text, "party").unwrap())
        };
        for (text, expected) in [
            (
                "[messages]\na = \"<bolt>x\"",
                "plugin party: messages.toml: [messages] \"a\": <bolt> at byte 0: unknown tag",
            ),
            (
                "[messages]\na = \"Hi <name>\"",
                "<name> at byte 3: unknown tag",
            ),
            (
                "[messages]\na = { text = \"<name>\", args = [\"name\"] }\n[languages.de]\na = \"<nom>\"",
                "plugin party: messages.toml: [languages.de] \"a\": <nom> at byte 0: unknown tag",
            ),
            (
                "[messages]\na = { text = \"<color:orange>x\" }",
                "is not a colour name",
            ),
        ] {
            let error = plugin(text).unwrap_err();
            assert!(error.contains(expected), "{text}: {error}");
        }
        // Templates use the theme of the configuration being loaded.
        assert!(plugin("[messages]\na = \"<brand>x\"").is_err());
        Catalog::themed([("brand", "<gold>")])
            .unwrap()
            .with_plugin(
                "party",
                Messages::parse("[messages]\na = \"<brand>x\"", "party").unwrap(),
            )
            .unwrap();
        plugin("[messages]\na = { text = \"<team>x</team>\", styles = [\"team\"] }").unwrap();
        for (entry, expected) in [
            (
                ("party:nope", "x"),
                "text.messages: unknown message \"party:nope\"; plugin party has no message \"nope\" in its messages.toml",
            ),
            (
                ("party:invited", "<from> <to>"),
                "text.messages[\"party:invited\"]: <to> at byte 7: unknown tag",
            ),
            (("party:tagged", "<team>x</team> <bolt>"), "unknown tag"),
            (
                ("party:created", ""),
                "text.messages[\"party:created\"] must contain 1..=4096 bytes",
            ),
            (
                ("rift:queue.left", "x"),
                "write Rift's own messages without \"rift:\"",
            ),
            (("party:", "x"), "is not a message key"),
            (("par ty:x", "x"), "is not a message key"),
            (
                ("queue.nope", "x"),
                "text.messages: unknown message \"queue.nope\"; see docs/text.md for the keys",
            ),
        ] {
            let error = party().with_messages([entry]).unwrap_err();
            assert!(error.contains(expected), "{entry:?}: {error}");
        }
        let error = party()
            .with_language("de", [("party:invited", "<to>")])
            .unwrap_err();
        assert!(
            error.starts_with("text.languages.de[\"party:invited\"]: <to> at byte 0"),
            "{error}"
        );
        let error = party()
            .with_language("de", [("party:nope", "x")])
            .unwrap_err();
        assert!(
            error.starts_with("text.languages.de: unknown message \"party:nope\""),
            "{error}"
        );
        // An enabled plugin without messages.toml has an empty catalogue, so
        // an unknown key there is an error too.
        let catalog = party().with_plugin("empty", Messages::default()).unwrap();
        assert!(
            catalog
                .clone()
                .with_messages([("empty:x", "y")])
                .unwrap_err()
                .contains("unknown message \"empty:x\"")
        );
        let counts: Vec<_> = catalog
            .namespaces()
            .map(|(name, messages)| (name, messages.len(), messages.languages().count()))
            .collect();
        assert_eq!(counts, [("empty", 0, 0), ("party", 3, 2)]);
        assert_eq!(catalog.namespace("party").unwrap().len(), 3);
        assert!(catalog.namespace("other").is_none());
    }

    #[test]
    fn replacements_for_a_plugin_that_is_not_enabled_are_warnings() {
        let catalog = party()
            .with_messages([("other:created", "<bolt>")])
            .unwrap()
            .with_language("de", [("other:created", "x")])
            .unwrap();
        assert_eq!(
            catalog.warnings(),
            [
                "text.messages[\"other:created\"]: no enabled plugin has the namespace \"other\"; the replacement is ignored",
                "text.languages.de[\"other:created\"]: no enabled plugin has the namespace \"other\"; the replacement is ignored",
            ]
        );
        assert!(
            catalog
                .message(None, "other:created", &Args::new(), None)
                .is_err()
        );
        assert!(Catalog::default().warnings().is_empty());
        // Unqualified keys stay Rift's own, exactly as before.
        assert_eq!(
            Catalog::new([], [("queue.left", "Bye.")]).unwrap(),
            Catalog::themed([])
                .unwrap()
                .with_messages([("queue.left", "Bye.")])
                .unwrap()
        );
    }
}
