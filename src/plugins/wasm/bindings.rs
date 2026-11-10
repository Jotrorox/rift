//! Host bindings for `rift:plugin@0.1.3`: generated types and traits, the
//! trait implementations that forward every import to
//! [`Services`](crate::plugins::services::Services), and conversions between
//! the generated types and the engine-neutral [`api`] types. WASI is linked
//! separately (see [`super::wasi`]).
//!
//! The host binds world `base` (every import and the `handler` export that
//! every component has) and, separately, worlds `optional` (`dialogs`),
//! `optional-decisions` (`decisions`) and `optional-styled` (`styled`), whose
//! exports a component built against an older 0.1 lacks; see
//! [`Optional::load`].
use super::State;
use crate::plugins::api;

wasmtime::component::bindgen!({
    path: "wit",
    world: "rift:plugin/base",
});

/// The optional exports (`dialogs`, since 0.1.1), sharing `base`'s types.
pub(super) mod optional {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "rift:plugin/optional",
        with: {
            "rift:plugin/types": super::rift::plugin::types,
        },
    });
}

/// The optional `decisions` export (since 0.1.2), sharing `base`'s types.
pub(super) mod optional_decisions {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "rift:plugin/optional-decisions",
        with: {
            "rift:plugin/types": super::rift::plugin::types,
        },
    });
}

/// The optional `styled` export (since 0.1.3), sharing `base`'s types.
pub(super) mod optional_styled {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "rift:plugin/optional-styled",
        with: {
            "rift:plugin/types": super::rift::plugin::types,
        },
    });
}

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
        title: crate::text::Text::Plain(title.title),
        subtitle: title.subtitle.map(crate::text::Text::Plain),
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

pub(super) fn action(action: types::Action) -> api::Action {
    match action {
        types::Action::Continue => api::Action::Continue,
        types::Action::Deny(reason) => api::Action::Deny(reason.into()),
        types::Action::Server(server) => api::Action::Server(server),
        types::Action::Message(text) => api::Action::Message(text.into()),
        types::Action::Queue(server) => api::Action::Queue(server),
        types::Action::LeaveQueue => api::Action::LeaveQueue,
    }
}

/// The `decisions` export's answer. Fails for a `-json` case whose text is
/// not a valid text component; the host then treats the decision like any
/// other invalid one.
pub(super) fn decision(decision: types::Decision) -> Result<api::Action, String> {
    use crate::text::Text;
    Ok(match decision {
        types::Decision::Continue => api::Action::Continue,
        types::Decision::Deny(reason) => api::Action::Deny(reason.into()),
        types::Decision::Server(server) => api::Action::Server(server),
        types::Decision::Message(text) => api::Action::Message(text.into()),
        types::Decision::Queue(server) => api::Action::Queue(server),
        types::Decision::LeaveQueue => api::Action::LeaveQueue,
        types::Decision::DenyJson(reason) => api::Action::Deny(Text::json(&reason, "deny reason")?),
        types::Decision::MessageJson(text) => api::Action::Message(Text::json(&text, "message")?),
    })
}

pub(super) fn suggestions(suggestions: Vec<types::Suggestion>) -> Vec<api::Suggestion> {
    suggestions
        .into_iter()
        .map(|suggestion| api::Suggestion {
            text: suggestion.text,
            tooltip: suggestion.tooltip.map(crate::text::Text::Plain),
        })
        .collect()
}

pub(super) fn status_override(status: types::StatusOverride) -> api::StatusOverride {
    api::StatusOverride {
        motd_json: status.motd_json,
        version: status.version.map(crate::text::Text::Plain),
        protocol: status.protocol,
        online: status.online,
        max_players: status.max_players,
        sample: status
            .sample
            .map(|lines| lines.into_iter().map(crate::text::Text::Plain).collect()),
        favicon: status.favicon,
    }
}

/// The `styled` export's suggestions. An empty or blank `tooltip-json` is no
/// tooltip; an invalid one fails the whole answer.
pub(super) fn rich_suggestions(
    suggestions: Vec<types::RichSuggestion>,
) -> Result<Vec<api::Suggestion>, String> {
    suggestions
        .into_iter()
        .map(|suggestion| {
            let tooltip = match suggestion.tooltip_json {
                Some(json) if !json.trim().is_empty() => {
                    Some(crate::text::Text::json(&json, "suggestion tooltip")?)
                }
                _ => None,
            };
            Ok(api::Suggestion {
                text: suggestion.text,
                tooltip,
            })
        })
        .collect()
}

/// The `styled` export's status override. Fails for a version or sample
/// line that is not a valid text component.
pub(super) fn rich_status_override(
    status: types::RichStatusOverride,
) -> Result<api::StatusOverride, String> {
    use crate::text::Text;
    Ok(api::StatusOverride {
        motd_json: status.motd_json,
        version: status
            .version_json
            .map(|version| Text::json(&version, "status version"))
            .transpose()?,
        protocol: status.protocol,
        online: status.online,
        max_players: status.max_players,
        sample: status
            .sample_json
            .map(|lines| {
                lines
                    .iter()
                    .map(|line| Text::json(line, "status sample line"))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?,
        favicon: status.favicon,
    })
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

/// A dialog click: `context.dialog` holds the action, values and payload.
pub(super) fn dialog_response(mut context: api::Context) -> types::DialogResponse {
    let dialog = context
        .dialog
        .take()
        .unwrap_or_else(|| crate::dialog::Response {
            action: String::new(),
            values: serde_json::Map::new(),
            payload: None,
        });
    types::DialogResponse {
        ctx: self::context(context),
        action: dialog.action,
        values_json: serde_json::Value::Object(dialog.values).to_string(),
        payload_json: dialog.payload.map(|payload| payload.to_string()),
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

    fn title_json(&mut self, target: String, spec: types::TitleJsonSpec) -> Result<bool, String> {
        self.services.players().title_json(
            &target,
            &spec.title_json,
            spec.subtitle_json.as_deref(),
            [spec.fade_in, spec.stay, spec.fade_out],
        )
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

    fn dialog(&mut self, target: String, dialog_json: String) -> Result<bool, String> {
        self.services.players().dialog(&target, &dialog_json)
    }

    fn clear_dialog(&mut self, target: String) -> Result<bool, String> {
        self.services.players().clear_dialog(&target)
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
        // `dialog` has no WIT case, so 0.1.0 components keep loading.
        assert_eq!(names, api::Hook::ALL[..api::Hook::ALL.len() - 1]);
        assert_eq!(api::Hook::ALL.last(), Some(&api::Hook::Dialog));
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

    #[test]
    fn styled_answers_convert_their_rich_text() {
        use crate::text::Text;
        let suggestions = rich_suggestions(vec![
            types::RichSuggestion {
                text: "a".into(),
                tooltip_json: Some(r#"{"text":"Tip","color":"red"}"#.into()),
            },
            types::RichSuggestion {
                text: "b".into(),
                tooltip_json: Some("  ".into()),
            },
            types::RichSuggestion {
                text: "c".into(),
                tooltip_json: Some("as written".into()),
            },
            types::RichSuggestion {
                text: "d".into(),
                tooltip_json: None,
            },
        ])
        .unwrap();
        assert_eq!(
            suggestions[0].tooltip,
            Some(Text::Component(
                serde_json::json!({"text": "Tip", "color": "red"})
            ))
        );
        assert_eq!(suggestions[1].tooltip, None);
        assert!(suggestions[2].tooltip.is_some());
        assert_eq!(suggestions[3].tooltip, None);
        let error = rich_suggestions(vec![types::RichSuggestion {
            text: "x".into(),
            tooltip_json: Some("{nope".into()),
        }])
        .unwrap_err();
        assert!(error.contains("suggestion tooltip"), "{error}");

        let status = rich_status_override(types::RichStatusOverride {
            motd_json: Some("\"motd\"".into()),
            version_json: Some(r#"{"text":"Rift","color":"gold"}"#.into()),
            protocol: Some(774),
            online: Some(1),
            max_players: Some(2),
            sample_json: Some(vec!["\"one\"".into(), "two".into()]),
            favicon: None,
        })
        .unwrap();
        assert_eq!(status.motd_json.as_deref(), Some("\"motd\""));
        assert_eq!(
            status.version,
            Some(Text::Component(
                serde_json::json!({"text": "Rift", "color": "gold"})
            ))
        );
        assert_eq!(status.sample.as_ref().map(Vec::len), Some(2));
        assert_eq!(
            (status.protocol, status.online, status.max_players),
            (Some(774), Some(1), Some(2))
        );
        let error = rich_status_override(types::RichStatusOverride {
            motd_json: None,
            version_json: None,
            protocol: None,
            online: None,
            max_players: None,
            sample_json: Some(vec!["[1,".into()]),
            favicon: None,
        })
        .unwrap_err();
        assert!(error.contains("status sample line"), "{error}");
    }
}
