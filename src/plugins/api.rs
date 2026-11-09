//! Engine-neutral mirrors of the `rift:plugin@0.1.0` WIT types
//! (`wit/rift-plugin.wit`). Engines translate their own values (Lua tables,
//! wasmtime bindgen records) into these; the host never sees engine values.
//! Core types are reused where their shape already matches the contract.
use serde::{Deserialize, Serialize};

pub use crate::{
    extensions::{Action, Context},
    hooks::RouteDecision,
    http_script::{HttpRequest, HttpResponse},
    protocol::Suggestion,
};

/// The `rift:plugin` package version this host implements.
pub const API_VERSION: &str = "0.1";

/// Log severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// A joined player in the process-wide directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerInfo {
    pub connection_id: u64,
    /// 32 lowercase hexadecimal digits, without dashes.
    pub uuid: String,
    pub name: String,
    pub server: String,
    pub protocol: i32,
    /// The node a viewer needs to see this player (`rift.visible-to`), or
    /// `None`. Plugins see every player and decide with this; the WIT
    /// record has no such field, so components ask `players.visible-to`.
    #[serde(default)]
    pub visible_to: Option<String>,
}

/// Metadata available right after TCP accept (`route`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    pub listener: String,
    pub peer_addr: String,
    pub peer_ip: String,
    pub peer_port: u16,
    pub local_addr: String,
    pub local_ip: String,
    pub local_port: u16,
    pub default_backend: Option<String>,
}

impl From<&crate::hooks::ConnectionInfo> for Connection {
    fn from(info: &crate::hooks::ConnectionInfo) -> Self {
        Self {
            listener: info.listener.clone(),
            peer_addr: info.peer_addr.to_string(),
            peer_ip: info.peer_addr.ip().to_string(),
            peer_port: info.peer_addr.port(),
            local_addr: info.local_addr.to_string(),
            local_ip: info.local_addr.ip().to_string(),
            local_port: info.local_addr.port(),
            default_backend: info.default_backend.clone(),
        }
    }
}

/// A server-list ping, before any backend contact (`status`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ping {
    pub listener: String,
    pub hostname: String,
    pub protocol: i32,
    pub peer_ip: String,
    pub backend: Option<String>,
    pub maintenance: bool,
    pub online: u64,
    pub max_players: u64,
}

impl From<&crate::server_list::Ping> for Ping {
    fn from(ping: &crate::server_list::Ping) -> Self {
        Self {
            listener: ping.listener.clone(),
            hostname: ping.hostname.clone(),
            protocol: ping.protocol,
            peer_ip: ping.peer_addr.ip().to_string(),
            backend: ping.backend.clone(),
            maintenance: ping.maintenance,
            online: ping.online,
            max_players: ping.max_players,
        }
    }
}

/// Fields a `status` hook replaces, as the guest sends them. The host
/// validates them into a [`crate::server_list::StatusOverride`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusOverride {
    /// A JSON text component or a JSON string.
    pub motd_json: Option<String>,
    pub version: Option<String>,
    pub protocol: Option<i32>,
    pub online: Option<u32>,
    pub max_players: Option<u32>,
    pub sample: Option<Vec<String>>,
    /// A `data:image/png;base64,...` URI.
    pub favicon: Option<String>,
}

/// A bus message delivered to a subscription made with `bus.subscribe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusMessage {
    pub handle: u64,
    pub subject: String,
    pub payload: Vec<u8>,
    pub reply: Option<String>,
}

/// A Rift event, mirroring a `rift.events.*` publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiftEvent {
    pub subject: String,
    pub payload_json: String,
}

/// How a command's arguments are completed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestKind {
    None,
    /// A fixed list, filtered by prefix.
    Fixed(Vec<String>),
    Players,
    Servers,
    /// Ask the plugin's `suggest` export.
    Dynamic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub name: String,
    pub permission: String,
    pub suggest: SuggestKind,
}

/// Every handler export except the lifecycle ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hook {
    Route,
    Login,
    InitialServer,
    BeforeTransfer,
    Command,
    Suggest,
    Status,
    Http,
    Join,
    AfterTransfer,
    Disconnect,
    Message,
    Timer,
    Event,
    /// A click on a plugin action of a dialog the plugin showed. Like timer
    /// ticks it is addressed to one plugin, and needs no registration: the
    /// WIT `hook` enum has no case for it, so WebAssembly plugins built
    /// against 0.1.0 keep loading.
    Dialog,
}

impl Hook {
    pub const ALL: [Hook; 15] = [
        Hook::Route,
        Hook::Login,
        Hook::InitialServer,
        Hook::BeforeTransfer,
        Hook::Command,
        Hook::Suggest,
        Hook::Status,
        Hook::Http,
        Hook::Join,
        Hook::AfterTransfer,
        Hook::Disconnect,
        Hook::Message,
        Hook::Timer,
        Hook::Event,
        Hook::Dialog,
    ];

    /// The snake_case name used by manifests, Lua and the bus.
    pub fn name(self) -> &'static str {
        match self {
            Self::Route => "route",
            Self::Login => "login",
            Self::InitialServer => "initial_server",
            Self::BeforeTransfer => "before_transfer",
            Self::Command => "command",
            Self::Suggest => "suggest",
            Self::Status => "status",
            Self::Http => "http",
            Self::Join => "join",
            Self::AfterTransfer => "after_transfer",
            Self::Disconnect => "disconnect",
            Self::Message => "message",
            Self::Timer => "timer",
            Self::Event => "event",
            Self::Dialog => "dialog",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|hook| hook.name() == name)
    }

    /// Observers are fire-and-forget and go to every plugin; everything else
    /// waits for an answer under a deadline.
    pub fn is_observer(self) -> bool {
        matches!(
            self,
            Self::Join
                | Self::AfterTransfer
                | Self::Disconnect
                | Self::Message
                | Self::Timer
                | Self::Event
                | Self::Dialog
        )
    }

    /// Hooks that carry a player's identity and therefore run only with
    /// `authentication.online_mode`.
    pub fn is_player(self) -> bool {
        matches!(
            self,
            Self::Login
                | Self::InitialServer
                | Self::BeforeTransfer
                | Self::Command
                | Self::Suggest
                | Self::Join
                | Self::AfterTransfer
                | Self::Disconnect
        )
    }
}

/// What `init` returns. Subscriptions and timers are created from inside
/// `init` through the host API.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Registration {
    pub commands: Vec<CommandSpec>,
    /// Events to deliver. An empty list delivers every hook.
    pub hooks: Vec<Hook>,
}

impl Registration {
    /// Dialog clicks reach the plugin that showed the dialog whatever it
    /// listed (see [`Hook::Dialog`]).
    pub fn wants(&self, hook: Hook) -> bool {
        hook == Hook::Dialog || self.hooks.is_empty() || self.hooks.contains(&hook)
    }
}

/// A boss bar shown to one player.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BossBarSpec {
    /// A plain string or a JSON text component.
    pub text_json: String,
    /// 0.0 to 1.0.
    pub progress: f32,
    pub color: String,
    pub style: String,
    pub darken_sky: bool,
    pub play_music: bool,
    pub create_fog: bool,
}

/// A title; times are ticks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleSpec {
    pub title: String,
    pub subtitle: Option<String>,
    pub fade_in: i32,
    pub stay: i32,
    pub fade_out: i32,
}

/// One call into a plugin: every handler export except `init`, `reload` and
/// `shutdown`, which are lifecycle operations of the worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    Route(Connection),
    Login(Context),
    InitialServer(Context),
    BeforeTransfer(Context),
    Command(Context),
    Suggest(Context),
    Status(Ping),
    Http(HttpRequest),
    Join(Context),
    AfterTransfer(Context),
    Disconnect(Context),
    Message(BusMessage),
    Timer(u64),
    Event(RiftEvent),
    /// A dialog click; `Context::dialog` holds the action and values.
    Dialog(Context),
}

impl Event {
    pub fn hook(&self) -> Hook {
        match self {
            Self::Route(_) => Hook::Route,
            Self::Login(_) => Hook::Login,
            Self::InitialServer(_) => Hook::InitialServer,
            Self::BeforeTransfer(_) => Hook::BeforeTransfer,
            Self::Command(_) => Hook::Command,
            Self::Suggest(_) => Hook::Suggest,
            Self::Status(_) => Hook::Status,
            Self::Http(_) => Hook::Http,
            Self::Join(_) => Hook::Join,
            Self::AfterTransfer(_) => Hook::AfterTransfer,
            Self::Disconnect(_) => Hook::Disconnect,
            Self::Message(_) => Hook::Message,
            Self::Timer(_) => Hook::Timer,
            Self::Event(_) => Hook::Event,
            Self::Dialog(_) => Hook::Dialog,
        }
    }
}

/// A handler's result. Each hook has exactly one valid shape; the host
/// rejects a mismatched reply like an invalid Lua result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    /// Observers, and hooks the plugin did not register.
    None,
    Route(RouteDecision),
    Action(Action),
    Suggestions(Vec<Suggestion>),
    Status(Option<StatusOverride>),
    Http(Option<HttpResponse>),
}

impl Reply {
    /// What the host uses for a hook the plugin does not handle: the WIT
    /// default of each export.
    pub fn default_for(hook: Hook) -> Self {
        match hook {
            Hook::Route => Self::Route(RouteDecision::Default),
            Hook::Login | Hook::InitialServer | Hook::BeforeTransfer | Hook::Command => {
                Self::Action(Action::Continue)
            }
            Hook::Suggest => Self::Suggestions(Vec::new()),
            Hook::Status => Self::Status(None),
            Hook::Http => Self::Http(None),
            Hook::Join
            | Hook::AfterTransfer
            | Hook::Disconnect
            | Hook::Message
            | Hook::Timer
            | Hook::Event
            | Hook::Dialog => Self::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_forms_follow_the_contract_names() {
        assert_eq!(
            serde_json::to_value(Action::Deny("no".into())).unwrap(),
            serde_json::json!({"deny":"no"})
        );
        assert_eq!(
            serde_json::to_value(Action::LeaveQueue).unwrap(),
            serde_json::json!("leave_queue")
        );
        let context = Context {
            id: 7,
            ..Context::default()
        };
        let value = serde_json::to_value(Event::Login(context.clone())).unwrap();
        assert_eq!(value["login"]["connection_id"], 7);
        assert_eq!(
            serde_json::from_value::<Event>(value).unwrap(),
            Event::Login(context)
        );
        assert_eq!(
            serde_json::to_value(Hook::InitialServer).unwrap(),
            serde_json::json!("initial_server")
        );
        for hook in Hook::ALL {
            assert_eq!(Hook::parse(hook.name()), Some(hook));
            assert_eq!(
                serde_json::to_value(hook).unwrap(),
                serde_json::json!(hook.name())
            );
        }
        let registration: Registration = serde_json::from_str(
            r#"{"commands":[{"name":"ban","permission":"mod.ban","suggest":{"fixed":["a"]}}]}"#,
        )
        .unwrap();
        assert!(registration.wants(Hook::Login));
        assert_eq!(
            registration.commands[0].suggest,
            SuggestKind::Fixed(vec!["a".into()])
        );
    }
}
