//! The `players` interface of the plugin contract, as `rift.players.<op>`
//! requests on the message bus and as the player endpoints of the HTTP API.
//!
//! The WIT promises one host API for every execution mode, but services on
//! the bus only answered decision hooks: a ban service had to hold
//! `control = true` to kick anyone, and could not kick at all with a
//! formatted screen. This module decodes the JSON form of each `players`
//! call into a [`Request`] that runs against the same process-wide
//! [`Players`] directory and with the same checks as the plugin API, so the
//! bus, HTTP and plugins cannot drift apart.
//!
//! Payloads mirror the WIT parameters with snake_case field names, the
//! convention of the other bus payloads (`connection_id`). The `-json`
//! operations take their component either as a JSON string, exactly as the
//! WIT does, or embedded as a JSON object or list, which is what a JSON
//! client naturally writes. Plain operations never interpret JSON, so a
//! service that relays what a player typed cannot be tricked into sending
//! click events.
use crate::{
    extensions::{PlayerAction, Players},
    plugins::api::TitleSpec,
    text::Text,
};
use serde_json::{Map, Value, json};

/// Every `rift.players.*` subject starts with this.
pub const PREFIX: &str = "rift.players";
/// Payloads are small objects; a component is at most 32 KiB.
pub const MAX_PAYLOAD: usize = 64 * 1024;

/// The operations, in the order of the WIT `players` interface. Dialogs,
/// tab lists and boss bars stay plugin-only: their clicks and lifetimes
/// belong to an owner, which a bus request does not have.
pub const OPERATIONS: &[&str] = &[
    "list",
    "get",
    "names",
    "message",
    "action-bar",
    "title",
    "kick",
    "transfer",
    "broadcast",
    "message-json",
    "action-bar-json",
    "kick-json",
    "broadcast-json",
    "title-json",
];

/// One decoded `players` call.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    List {
        server: Option<String>,
    },
    Get {
        target: String,
    },
    Names {
        prefix: Option<String>,
    },
    /// `message` and `message-json`, already checked by [`Text`].
    Message {
        target: String,
        text: Text,
    },
    ActionBar {
        target: String,
        text: Text,
    },
    Title {
        target: String,
        title: TitleSpec,
    },
    Kick {
        target: String,
        reason: Text,
    },
    Transfer {
        target: String,
        server: String,
    },
    /// Everyone, or the players on one server or service group.
    Broadcast {
        text: Text,
        server: Option<String>,
    },
}

impl Request {
    /// Decode `operation` (the subject after `rift.players.`) and its JSON
    /// object payload. Unknown operations and fields are errors.
    pub fn decode(operation: &str, payload: &[u8]) -> Result<Self, String> {
        if payload.len() > MAX_PAYLOAD {
            return Err("players payload exceeds 64 KiB".into());
        }
        let value: Value = if payload.iter().all(u8::is_ascii_whitespace) {
            // `list` and `names` take no arguments; an empty body is `{}`.
            Value::Object(Map::new())
        } else {
            serde_json::from_slice(payload).map_err(|_| "expected a JSON object")?
        };
        let object = value.as_object().ok_or("expected a JSON object")?;
        let fields: &[&str] = match operation {
            "list" => &["server"],
            "get" => &["target"],
            "names" => &["prefix"],
            "message" | "action-bar" | "message-json" | "action-bar-json" => &["target", "text"],
            "title" | "title-json" => {
                &["target", "title", "subtitle", "fade_in", "stay", "fade_out"]
            }
            "kick" | "kick-json" => &["target", "reason"],
            "transfer" => &["target", "server"],
            "broadcast" | "broadcast-json" => &["text", "server"],
            _ => return Err(format!("unknown players operation {operation:?}")),
        };
        if let Some(field) = object.keys().find(|key| !fields.contains(&key.as_str())) {
            return Err(format!("unknown field {field:?} for players.{operation}"));
        }
        let target = || name(object, "target", true).map(Option::unwrap_or_default);
        let server = |required| name(object, "server", required);
        let plain = |field: &str, label: &str| match object.get(field) {
            Some(Value::String(text)) => Text::plain(text.as_str(), label),
            _ => Err(format!("{field} must be a string")),
        };
        let rich = |field: &str, label: &str| match object.get(field) {
            Some(Value::String(text)) => Text::json(text, label),
            Some(value @ (Value::Object(_) | Value::Array(_))) => {
                Text::component(value.clone(), label)
            }
            _ => Err(format!(
                "{field} must be a JSON text component, as a string or embedded"
            )),
        };
        Ok(match operation {
            "list" => Self::List {
                server: server(false)?,
            },
            "get" => Self::Get { target: target()? },
            "names" => Self::Names {
                prefix: match object.get("prefix") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(prefix)) if prefix.len() <= 256 => Some(prefix.clone()),
                    Some(_) => return Err("prefix must be a string of at most 256 bytes".into()),
                },
            },
            "message" => Self::Message {
                target: target()?,
                text: plain("text", "message")?,
            },
            "message-json" => Self::Message {
                target: target()?,
                text: rich("text", "message")?,
            },
            "action-bar" => Self::ActionBar {
                target: target()?,
                text: plain("text", "action bar text")?,
            },
            "action-bar-json" => Self::ActionBar {
                target: target()?,
                text: rich("text", "action bar text")?,
            },
            "title" | "title-json" => {
                let ticks = |field: &str, default: i32| match object.get(field) {
                    None | Some(Value::Null) => Ok(default),
                    Some(value) => value
                        .as_i64()
                        .and_then(|ticks| i32::try_from(ticks).ok())
                        .ok_or_else(|| format!("{field} must be an integer number of ticks")),
                };
                // `title` takes plain strings; `title-json` takes JSON text
                // components, as strings or embedded, like the other -json
                // operations. An empty or absent text is left out.
                let text = |field: &str| -> Result<Option<Text>, String> {
                    match object.get(field) {
                        None | Some(Value::Null) => Ok(None),
                        Some(Value::String(text)) if text.is_empty() => Ok(None),
                        Some(Value::String(text)) if operation == "title" => {
                            Ok(Some(Text::Plain(text.clone())))
                        }
                        Some(_) if operation == "title" => Err(format!("{field} must be a string")),
                        Some(_) => rich(field, field).map(Some),
                    }
                };
                Self::Title {
                    target: target()?,
                    // The vanilla `/title` defaults.
                    title: TitleSpec {
                        title: text("title")?.unwrap_or_else(|| Text::Plain(String::new())),
                        subtitle: text("subtitle")?,
                        fade_in: ticks("fade_in", 10)?,
                        stay: ticks("stay", 70)?,
                        fade_out: ticks("fade_out", 20)?,
                    },
                }
            }
            "kick" => Self::Kick {
                target: target()?,
                reason: plain("reason", "kick reason")?,
            },
            "kick-json" => Self::Kick {
                target: target()?,
                reason: rich("reason", "kick reason")?,
            },
            "transfer" => Self::Transfer {
                target: target()?,
                server: server(true)?.unwrap_or_default(),
            },
            "broadcast" => Self::Broadcast {
                text: plain("text", "message")?,
                server: server(false)?,
            },
            "broadcast-json" => Self::Broadcast {
                text: rich("text", "message")?,
                server: server(false)?,
            },
            _ => unreachable!("checked above"),
        })
    }

    /// The operation name, as in [`OPERATIONS`]; `-json` variants report
    /// their plain name, since they do the same thing.
    pub fn operation(&self) -> &'static str {
        match self {
            Self::List { .. } => "list",
            Self::Get { .. } => "get",
            Self::Names { .. } => "names",
            Self::Message { .. } => "message",
            Self::ActionBar { .. } => "action-bar",
            Self::Title { .. } => "title",
            Self::Kick { .. } => "kick",
            Self::Transfer { .. } => "transfer",
            Self::Broadcast { .. } => "broadcast",
        }
    }

    /// Whether the call acts on players, and so belongs in the audit log.
    /// Reads are not audited.
    pub fn acts(&self) -> bool {
        !matches!(
            self,
            Self::List { .. } | Self::Get { .. } | Self::Names { .. }
        )
    }

    /// What an audit record names: the player, the destination of a
    /// transfer, or the server a broadcast went to. Never the text.
    pub fn target(&self) -> String {
        match self {
            Self::List { server } | Self::Broadcast { server, .. } => {
                server.clone().unwrap_or_default()
            }
            Self::Names { .. } => String::new(),
            Self::Transfer { target, server } => format!("{target} -> {server}"),
            Self::Get { target }
            | Self::Message { target, .. }
            | Self::ActionBar { target, .. }
            | Self::Title { target, .. }
            | Self::Kick { target, .. } => target.clone(),
        }
    }

    /// The player named by the request, if it names one.
    pub fn player(&self) -> Option<&str> {
        match self {
            Self::Get { target }
            | Self::Message { target, .. }
            | Self::ActionBar { target, .. }
            | Self::Title { target, .. }
            | Self::Kick { target, .. }
            | Self::Transfer { target, .. } => Some(target),
            _ => None,
        }
    }

    /// Run the call. Actions answer whether the player was online, as in
    /// the WIT; `broadcast` answers how many players received it.
    pub fn execute(self, players: &Players) -> Result<Value, String> {
        let send = |target: &str, action| {
            players
                .send(target, action)
                .map(Value::Bool)
                .map_err(|error| error.to_string())
        };
        match self {
            Self::List { server } => players.list(server.as_deref()).map(|list| json!(list)),
            Self::Get { target } => players.get(&target).map(|info| json!(info)),
            Self::Names { prefix } => Ok(json!(
                players.names_with_prefix(prefix.as_deref().unwrap_or_default())
            )),
            Self::Message { target, text } => send(&target, PlayerAction::Message(text)),
            Self::ActionBar { target, text } => send(&target, PlayerAction::ActionBar(text)),
            Self::Title { target, title } => send(&target, title_action(&title)?),
            Self::Kick { target, reason } => send(&target, PlayerAction::Kick(reason)),
            Self::Transfer { target, server } => {
                players.transfer(&target, &server).map(Value::Bool)
            }
            Self::Broadcast { text, server } => players
                .broadcast(&text, server.as_deref())
                .map(|count| json!(count)),
        }
    }
}

/// A player name, UUID, connection id or server name of 1..=256 bytes.
fn name(
    object: &Map<String, Value>,
    field: &str,
    required: bool,
) -> Result<Option<String>, String> {
    match object.get(field) {
        None | Some(Value::Null) if !required => Ok(None),
        Some(Value::String(value)) if !value.is_empty() && value.len() <= 256 => {
            Ok(Some(value.clone()))
        }
        _ => Err(format!("{field} must be a string of 1..=256 bytes")),
    }
}

/// The checks of `players.title`, shared by every caller: plain or rich
/// text, where an empty plain title or subtitle is left out.
pub fn title_action(title: &TitleSpec) -> Result<PlayerAction, String> {
    const MAX_TEXT: usize = crate::text::MAX_PLAIN_BYTES;
    let checked = |text: &Text, label: &str| -> Result<Option<Text>, String> {
        match text {
            Text::Plain(plain) if plain.is_empty() => Ok(None),
            Text::Plain(plain) if plain.len() > MAX_TEXT => {
                Err(format!("{label} must contain at most 1024 bytes"))
            }
            text => text.validate(label).map(|()| Some(text.clone())),
        }
    };
    let main = checked(&title.title, "title")?;
    let subtitle = match &title.subtitle {
        Some(subtitle) => checked(subtitle, "subtitle")?,
        None => None,
    };
    if main.is_none() && subtitle.is_none() {
        return Err("title requires a title or subtitle".into());
    }
    let times = [title.fade_in, title.stay, title.fade_out];
    if times.iter().any(|ticks| !(0..=1200).contains(ticks)) {
        return Err("title times must be 0..=1200 ticks".into());
    }
    Ok(PlayerAction::Title {
        title: main,
        subtitle,
        times,
    })
}

/// Who a bus request acts as in the audit log, from [`Message::sender`].
///
/// Remote callers are their authenticated login, as `bus:<login>`. Rift tags
/// in-process requests from persistent plugins as `plugin:<name>`; logins
/// cannot contain `:`, so neither can pose as the other, and neither can
/// collide with a web operator name. A request without a sender comes from
/// the configuration script and acts as `anonymous`, the name of the subject
/// namespace (`rift.control`, `rift.players`).
///
/// [`Message::sender`]: crate::messaging::Message::sender
pub fn actor(sender: Option<&str>, anonymous: &str) -> String {
    match sender {
        Some(local) if local.contains(':') => local.to_owned(),
        Some(login) => format!("bus:{login}"),
        None => anonymous.to_owned(),
    }
}

/// The in-process sender a persistent plugin's requests carry on the
/// subjects where Rift audits callers.
pub fn plugin_sender(plugin: &str) -> String {
    format!("plugin:{plugin}")
}

/// Whether `subject` is one whose handler audits its caller: plugins tag
/// only these, because other handlers (the permission service) treat any
/// sender as a remote principal.
pub fn audited_subject(subject: &str) -> bool {
    ["rift.control", PREFIX, crate::network_service::PREFIX]
        .iter()
        .any(|prefix| {
            subject
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('.'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_mirror_the_wit_and_refuse_unknown_fields() {
        assert_eq!(
            Request::decode("list", b"").unwrap(),
            Request::List { server: None }
        );
        assert_eq!(
            Request::decode("transfer", br#"{"target":"Alice","server":"lobby"}"#).unwrap(),
            Request::Transfer {
                target: "Alice".into(),
                server: "lobby".into()
            }
        );
        for (operation, payload, message) in [
            ("kick", r#"{"target":"Alice"}"#, "reason must be a string"),
            (
                "kick",
                r#"{"target":"","reason":"x"}"#,
                "target must be a string",
            ),
            (
                "message",
                r#"{"target":"A","text":"x","extra":1}"#,
                "unknown field",
            ),
            ("stop", "{}", "unknown players operation"),
            ("message", "[]", "expected a JSON object"),
            ("transfer", r#"{"target":"A"}"#, "server must be a string"),
        ] {
            let error = Request::decode(operation, payload.as_bytes()).unwrap_err();
            assert!(error.contains(message), "{operation} {payload}: {error}");
        }
    }

    #[test]
    fn json_operations_take_strings_or_embedded_components() {
        let screen = json!({"text":"Banned","color":"red","extra":[{"text":"\nAppeal at example.org","color":"gray"}]});
        let embedded = Request::decode(
            "kick-json",
            json!({"target":"Mallory","reason":screen})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        let as_string = Request::decode(
            "kick-json",
            json!({"target":"Mallory","reason":screen.to_string()})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(embedded, as_string);
        let Request::Kick {
            reason: Text::Component(component),
            ..
        } = embedded
        else {
            panic!("expected a component");
        };
        assert_eq!(component["color"], "red");
        // Plain operations never read JSON, even when the text looks like it.
        let plain = Request::decode(
            "message",
            br#"{"target":"A","text":"{\"text\":\"x\",\"clickEvent\":{}}"}"#,
        )
        .unwrap();
        assert!(matches!(
            plain,
            Request::Message {
                text: Text::Plain(_),
                ..
            }
        ));
        assert!(Request::decode("message", br#"{"target":"A","text":{"text":"x"}}"#).is_err());
        assert!(Request::decode("message-json", br#"{"target":"A","text":"{oops"}"#).is_err());
    }

    #[test]
    fn audit_names_callers_without_letting_them_collide() {
        assert_eq!(actor(Some("relaxoban"), "rift.players"), "bus:relaxoban");
        assert_eq!(actor(Some("plugin:bans"), "rift.players"), "plugin:bans");
        assert_eq!(actor(None, "rift.control"), "rift.control");
        assert!(audited_subject("rift.players.kick"));
        assert!(audited_subject("rift.control.stop"));
        assert!(audited_subject("rift.network.connect"));
        assert!(!audited_subject("rift.playersx.kick"));
        assert!(!audited_subject("rift.perms.hello"));
        let request = Request::decode("title", br#"{"target":"A","title":"Hi"}"#).unwrap();
        assert!(request.acts());
        assert_eq!(request.target(), "A");
        let Request::Title { title, .. } = &request else {
            panic!()
        };
        assert_eq!((title.fade_in, title.stay, title.fade_out), (10, 70, 20));
        assert_eq!(title.title, Text::Plain("Hi".into()));
        let rich = Request::decode(
            "title-json",
            br#"{"target":"A","title":{"mini":"<gold>Hi"},"subtitle":"{\"text\":\"s\"}"}"#,
        )
        .unwrap();
        let Request::Title { title, .. } = &rich else {
            panic!()
        };
        assert_eq!(
            title.title,
            Text::Component(json!({"text": "Hi", "color": "gold"}))
        );
        assert_eq!(title.subtitle, Some(Text::Component(json!({"text": "s"}))));
        assert_eq!(rich.operation(), "title");
        assert!(Request::decode("title", br#"{"target":"A","title":{"text":"x"}}"#).is_err());
        assert!(!Request::decode("names", b"{}").unwrap().acts());
    }
}
