//! Host bindings for the `core` world of `rift:plugin@0.2.0`: generated
//! types and traits, the trait implementations that forward every import to
//! [`Services`](crate::plugins::services::Services), and conversions between
//! the generated types and the engine-neutral [`api`] types. WASI is linked
//! separately (see [`super::wasi`]).
use super::State;
use crate::plugins::api;

wasmtime::component::bindgen!({
    path: "wit",
    world: "rift:plugin/core",
});

use self::rift::plugin::{bus, log, meta, permissions, players, servers, store, timers, types};

/// Guest paths of the granted directories (`meta.data-dir`, `meta.config-dir`).
pub(super) const DATA_DIR: &str = "/data";
pub(super) const CONFIG_DIR: &str = "/host";

// Guest values into neutral values.

fn level(level: types::Level) -> api::Level {
    match level {
        types::Level::Trace => api::Level::Trace,
        types::Level::Debug => api::Level::Debug,
        types::Level::Info => api::Level::Info,
        types::Level::Warn => api::Level::Warn,
        types::Level::Error => api::Level::Error,
    }
}

fn title(title: types::TitleSpec) -> api::TitleSpec {
    api::TitleSpec {
        title: title.title,
        subtitle: title.subtitle,
        fade_in: title.fade_in,
        stay: title.stay,
        fade_out: title.fade_out,
    }
}

fn boss_bar(bar: types::BossBarSpec) -> api::BossBarSpec {
    api::BossBarSpec {
        text_json: bar.text_json,
        progress: bar.progress,
        color: bar.color,
        style: bar.style,
        darken_sky: bar.darken_sky,
        play_music: bar.play_music,
        create_fog: bar.create_fog,
    }
}

pub(super) fn hook(hook: types::Hook) -> api::Hook {
    match hook {
        types::Hook::Route => api::Hook::Route,
        types::Hook::Login => api::Hook::Login,
        types::Hook::InitialServer => api::Hook::InitialServer,
        types::Hook::BeforeTransfer => api::Hook::BeforeTransfer,
        types::Hook::Command => api::Hook::Command,
        types::Hook::Suggest => api::Hook::Suggest,
        types::Hook::Status => api::Hook::Status,
        types::Hook::Http => api::Hook::Http,
        types::Hook::Join => api::Hook::Join,
        types::Hook::AfterTransfer => api::Hook::AfterTransfer,
        types::Hook::Disconnect => api::Hook::Disconnect,
        types::Hook::Message => api::Hook::Message,
        types::Hook::Timer => api::Hook::Timer,
        types::Hook::Event => api::Hook::Event,
    }
}

fn suggest_kind(kind: types::SuggestKind) -> api::SuggestKind {
    match kind {
        types::SuggestKind::None => api::SuggestKind::None,
        types::SuggestKind::Fixed(entries) => api::SuggestKind::Fixed(entries),
        types::SuggestKind::Players => api::SuggestKind::Players,
        types::SuggestKind::Servers => api::SuggestKind::Servers,
        types::SuggestKind::Dynamic => api::SuggestKind::Dynamic,
    }
}

pub(super) fn registration(registration: types::Registration) -> api::Registration {
    api::Registration {
        commands: registration
            .commands
            .into_iter()
            .map(|command| api::CommandSpec {
                name: command.name,
                permission: command.permission,
                suggest: suggest_kind(command.suggest),
            })
            .collect(),
        hooks: registration.hooks.into_iter().map(hook).collect(),
    }
}

pub(super) fn route_decision(decision: types::RouteDecision) -> api::RouteDecision {
    match decision {
        types::RouteDecision::Default => api::RouteDecision::Default,
        types::RouteDecision::Backend(backend) => api::RouteDecision::Backend(backend),
        types::RouteDecision::Reject(reason) => api::RouteDecision::Reject { reason },
    }
}

/// Fails for a `-json` case whose text is not a valid text component; the
/// host then treats the decision like any other invalid one.
pub(super) fn action(action: types::Action) -> Result<api::Action, String> {
    use crate::text::Text;
    Ok(match action {
        types::Action::Continue => api::Action::Continue,
        types::Action::Deny(reason) => api::Action::Deny(reason.into()),
        types::Action::Server(server) => api::Action::Server(server),
        types::Action::Message(text) => api::Action::Message(text.into()),
        types::Action::Queue(server) => api::Action::Queue(server),
        types::Action::LeaveQueue => api::Action::LeaveQueue,
        types::Action::DenyJson(reason) => api::Action::Deny(Text::json(&reason, "deny reason")?),
        types::Action::MessageJson(text) => api::Action::Message(Text::json(&text, "message")?),
    })
}

pub(super) fn suggestions(suggestions: Vec<types::Suggestion>) -> Vec<api::Suggestion> {
    suggestions
        .into_iter()
        .map(|suggestion| api::Suggestion {
            text: suggestion.text,
            tooltip: suggestion.tooltip,
        })
        .collect()
}

pub(super) fn status_override(status: types::StatusOverride) -> api::StatusOverride {
    api::StatusOverride {
        motd_json: status.motd_json,
        version: status.version,
        protocol: status.protocol,
        online: status.online,
        max_players: status.max_players,
        sample: status.sample,
        favicon: status.favicon,
    }
}

pub(super) fn http_response(response: types::HttpResponse) -> api::HttpResponse {
    api::HttpResponse {
        status: response.status,
        content_type: response.content_type,
        body: response.body,
    }
}

// Neutral values into guest values.

fn player_info(player: api::PlayerInfo) -> types::PlayerInfo {
    types::PlayerInfo {
        connection_id: player.connection_id,
        uuid: player.uuid,
        name: player.name,
        server: player.server,
        protocol: player.protocol,
    }
}

pub(super) fn context(context: api::Context) -> types::Context {
    types::Context {
        connection_id: context.id,
        uuid: context.uuid,
        name: context.name,
        listener: context.listener,
        hostname: context.hostname,
        peer_ip: context.peer_ip,
        protocol: context.protocol,
        server: context.server,
        default_server: context.default_server,
        target: context.target,
        reason: context.reason,
        success: context.success,
        command: context.command,
        args: context.args,
        word: context.word,
        position: u32::try_from(context.position).unwrap_or(u32::MAX),
    }
}

pub(super) fn connection(connection: api::Connection) -> types::Connection {
    types::Connection {
        listener: connection.listener,
        peer_addr: connection.peer_addr,
        peer_ip: connection.peer_ip,
        peer_port: connection.peer_port,
        local_addr: connection.local_addr,
        local_ip: connection.local_ip,
        local_port: connection.local_port,
        default_backend: connection.default_backend,
    }
}

pub(super) fn ping(ping: api::Ping) -> types::Ping {
    types::Ping {
        listener: ping.listener,
        hostname: ping.hostname,
        protocol: ping.protocol,
        peer_ip: ping.peer_ip,
        backend: ping.backend,
        maintenance: ping.maintenance,
        online: ping.online,
        max_players: ping.max_players,
    }
}

pub(super) fn http_request(request: api::HttpRequest) -> types::HttpRequest {
    types::HttpRequest {
        method: request.method,
        path: request.path,
        query: request.query,
        body: request.body,
        headers: request.headers.into_iter().collect(),
        context_json: request.context.to_string(),
    }
}

pub(super) fn bus_message(message: api::BusMessage) -> types::BusMessage {
    types::BusMessage {
        handle: message.handle,
        subject: message.subject,
        payload: message.payload,
        reply: message.reply,
    }
}

pub(super) fn rift_event(event: api::RiftEvent) -> types::RiftEvent {
    types::RiftEvent {
        subject: event.subject,
        payload_json: event.payload_json,
    }
}

// Imports: thin forwarders to `Services`.

impl types::Host for State {}

impl players::Host for State {
    fn list(&mut self, server: Option<String>) -> Result<Vec<types::PlayerInfo>, String> {
        let players = self.services.players().list(server.as_deref())?;
        Ok(players.into_iter().map(player_info).collect())
    }

    fn get(&mut self, target: String) -> Result<Option<types::PlayerInfo>, String> {
        Ok(self.services.players().get(&target)?.map(player_info))
    }

    fn names(&mut self, prefix: Option<String>) -> Vec<String> {
        self.services.players().names(prefix.as_deref())
    }

    fn message(&mut self, target: String, text: String) -> Result<bool, String> {
        self.services.players().message(&target, &text)
    }

    fn message_json(&mut self, target: String, text_json: String) -> Result<bool, String> {
        self.services.players().message_json(&target, &text_json)
    }

    fn action_bar(&mut self, target: String, text: String) -> Result<bool, String> {
        self.services.players().action_bar(&target, &text)
    }

    fn action_bar_json(&mut self, target: String, text_json: String) -> Result<bool, String> {
        self.services.players().action_bar_json(&target, &text_json)
    }

    fn title(&mut self, target: String, spec: types::TitleSpec) -> Result<bool, String> {
        self.services.players().title(&target, &title(spec))
    }

    fn tab_list(
        &mut self,
        target: String,
        header_json: Option<String>,
        footer_json: Option<String>,
    ) -> Result<bool, String> {
        self.services
            .players()
            .tab_list(&target, header_json.as_deref(), footer_json.as_deref())
    }

    fn boss_bar(
        &mut self,
        target: String,
        name: String,
        bar: Option<types::BossBarSpec>,
    ) -> Result<bool, String> {
        let bar = bar.map(boss_bar);
        self.services
            .players()
            .boss_bar(&target, &name, bar.as_ref())
    }

    fn kick(&mut self, target: String, reason: String) -> Result<bool, String> {
        self.services.players().kick(&target, &reason)
    }

    fn kick_json(&mut self, target: String, reason_json: String) -> Result<bool, String> {
        self.services.players().kick_json(&target, &reason_json)
    }

    fn transfer(&mut self, target: String, server: String) -> Result<bool, String> {
        self.services.players().transfer(&target, &server)
    }

    fn broadcast(&mut self, text: String) -> Result<u32, String> {
        self.services.players().broadcast(&text)
    }

    fn broadcast_json(&mut self, text_json: String) -> Result<u32, String> {
        self.services.players().broadcast_json(&text_json)
    }
}

impl store::Host for State {
    fn get(&mut self, namespace: String, key: String) -> Result<Option<Vec<u8>>, String> {
        self.services.store().get(&namespace, &key)
    }

    fn set(&mut self, namespace: String, key: String, value: Vec<u8>) -> Result<(), String> {
        self.services.store().set(&namespace, &key, &value)
    }

    fn delete(&mut self, namespace: String, key: String) -> Result<bool, String> {
        self.services.store().delete(&namespace, &key)
    }

    fn increment(&mut self, namespace: String, key: String, delta: i64) -> Result<i64, String> {
        self.services.store().increment(&namespace, &key, delta)
    }

    fn list(
        &mut self,
        namespace: String,
        prefix: String,
    ) -> Result<Vec<(String, Vec<u8>)>, String> {
        self.services.store().list(&namespace, &prefix)
    }
}

impl permissions::Host for State {
    fn has(&mut self, uuid: String, node: String) -> bool {
        self.services.permissions().has(&uuid, &node)
    }

    fn set(&mut self, uuid: String, node: String, value: Option<bool>) -> Result<(), String> {
        self.services.permissions().set(&uuid, &node, value)
    }

    fn add_group(&mut self, uuid: String, group: String) -> Result<bool, String> {
        self.services.permissions().add_group(&uuid, &group)
    }

    fn remove_group(&mut self, uuid: String, group: String) -> Result<bool, String> {
        self.services.permissions().remove_group(&uuid, &group)
    }

    fn groups(&mut self, uuid: String) -> Vec<String> {
        self.services.permissions().groups(&uuid)
    }

    fn list_groups(&mut self) -> String {
        self.services.permissions().list_groups()
    }
}

impl bus::Host for State {
    fn publish(
        &mut self,
        subject: String,
        payload: Vec<u8>,
        reply: Option<String>,
    ) -> Result<u32, String> {
        self.services
            .bus()
            .publish(&subject, &payload, reply.as_deref())
    }

    fn request(
        &mut self,
        subject: String,
        payload: Vec<u8>,
        timeout_ms: u32,
    ) -> Result<Vec<u8>, String> {
        self.services.bus().request(&subject, &payload, timeout_ms)
    }

    fn subscribe(&mut self, subject: String, queue_group: Option<String>) -> Result<u64, String> {
        self.services
            .bus()
            .subscribe(&subject, queue_group.as_deref())
    }

    fn unsubscribe(&mut self, handle: u64) -> bool {
        self.services.bus().unsubscribe(handle)
    }

    fn stream_publish(&mut self, subject: String, payload: Vec<u8>) -> Result<u64, String> {
        self.services.bus().stream_publish(&subject, &payload)
    }
}

impl servers::Host for State {
    fn create(&mut self, group: String) -> Result<String, String> {
        self.services.servers().create(&group)
    }

    fn remove(&mut self, name: String) -> Result<String, String> {
        self.services.servers().remove(&name)
    }

    fn start(&mut self, name: String) -> Result<String, String> {
        self.services.servers().start(&name)
    }

    fn stop(&mut self, name: String) -> Result<String, String> {
        self.services.servers().stop(&name)
    }

    fn download(&mut self, template: String) -> Result<String, String> {
        self.services.servers().download(&template)
    }

    fn control(&mut self, operation: String, payload_json: String) -> Result<String, String> {
        self.services.servers().control(&operation, &payload_json)
    }
}

impl timers::Host for State {
    fn every(&mut self, ms: u32) -> u64 {
        self.services.timers().every(ms)
    }

    fn after(&mut self, ms: u32) -> u64 {
        self.services.timers().after(ms)
    }

    fn cancel(&mut self, handle: u64) -> bool {
        self.services.timers().cancel(handle)
    }
}

impl log::Host for State {
    fn log(&mut self, severity: types::Level, message: String) {
        self.services.log(level(severity), &message);
    }
}

impl meta::Host for State {
    fn name(&mut self) -> String {
        self.services.meta().name().to_owned()
    }

    fn version(&mut self) -> String {
        self.services.meta().version().to_owned()
    }

    fn data_dir(&mut self) -> String {
        DATA_DIR.to_owned()
    }

    fn config_dir(&mut self) -> Option<String> {
        self.services
            .meta()
            .config_dir()
            .map(|_| CONFIG_DIR.to_owned())
    }

    fn clock(&mut self) -> f64 {
        self.services.meta().clock()
    }

    fn time(&mut self) -> i64 {
        self.services.meta().time()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_and_contexts_convert_both_ways() {
        let names: Vec<_> = [
            types::Hook::Route,
            types::Hook::Login,
            types::Hook::InitialServer,
            types::Hook::BeforeTransfer,
            types::Hook::Command,
            types::Hook::Suggest,
            types::Hook::Status,
            types::Hook::Http,
            types::Hook::Join,
            types::Hook::AfterTransfer,
            types::Hook::Disconnect,
            types::Hook::Message,
            types::Hook::Timer,
            types::Hook::Event,
        ]
        .into_iter()
        .map(hook)
        .collect();
        assert_eq!(names, api::Hook::ALL);
        let converted = context(api::Context {
            id: 9,
            name: "Alex".into(),
            position: 2,
            server: Some("lobby".into()),
            ..Default::default()
        });
        assert_eq!(converted.connection_id, 9);
        assert_eq!(converted.position, 2);
        assert_eq!(converted.server.as_deref(), Some("lobby"));
        assert_eq!(
            route_decision(types::RouteDecision::Reject(Some("no".into()))),
            api::RouteDecision::Reject {
                reason: Some("no".into())
            }
        );
        let request = http_request(api::HttpRequest {
            method: "GET".into(),
            path: "/x".into(),
            query: String::new(),
            body: String::new(),
            headers: [("a".to_owned(), "b".to_owned())].into(),
            context: serde_json::json!({"scopes": []}),
        });
        assert_eq!(request.headers, [("a".to_owned(), "b".to_owned())]);
        assert_eq!(request.context_json, r#"{"scopes":[]}"#);
    }
}
