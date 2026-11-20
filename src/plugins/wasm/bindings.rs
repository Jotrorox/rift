//! Host bindings for `rift:plugin@0.2.1`: generated types and traits, the
//! trait implementations that forward every import to
//! [`Services`](crate::plugins::services::Services), and conversions between
//! the generated types and the engine-neutral [`api`] types. WASI is linked
//! separately (see [`super::wasi`]).
use super::State;
use crate::{plugins::api, text::Text};

wasmtime::component::bindgen!({
    path: "wit",
    world: "rift:plugin/core",
});

use self::rift::plugin::{
    background, bus, log, meta, permissions, players, servers, store, text as text_api, timers,
    types,
};

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

/// Guest text, checked: plain text as written, or a JSON text component
/// (markup objects included, and messages resolved for the recipient in
/// scope; see [`crate::text::rendering`]).
pub(super) fn text(text: types::Text, label: &str) -> Result<Text, String> {
    text_as_written(text, label)?.checked(label)
}

/// Guest text parsed but not converted, for text with several recipients
/// (see [`crate::text::Unrendered`]).
fn text_as_written(text: types::Text, label: &str) -> Result<Text, String> {
    match text {
        types::Text::Plain(text) => Text::plain(&text, label),
        types::Text::Json(json) => {
            let trimmed = json.trim_start();
            if !trimmed.starts_with(['{', '[', '"']) {
                return Err(format!(
                    "{label}: json text must be a JSON text component (an object, an array or a string)"
                ));
            }
            Text::json_as_written(&json, label)
        }
    }
}

/// Guest text for the host functions that still take JSON: a plain string
/// becomes a JSON string, which is shown as written.
fn text_json(text: types::Text) -> String {
    match text {
        types::Text::Plain(text) => serde_json::Value::String(text).to_string(),
        types::Text::Json(json) => json,
    }
}

/// A title: an absent text is an empty plain one, which is left out.
fn title(title: types::TitleSpec) -> Result<api::TitleSpec, String> {
    Ok(api::TitleSpec {
        title: match title.title {
            Some(title) => text(title, "title")?,
            None => Text::Plain(String::new()),
        },
        subtitle: title
            .subtitle
            .map(|subtitle| text(subtitle, "subtitle"))
            .transpose()?,
        fade_in: title.fade_in,
        stay: title.stay,
        fade_out: title.fade_out,
    })
}

fn boss_bar(bar: types::BossBarSpec) -> api::BossBarSpec {
    api::BossBarSpec {
        text_json: text_json(bar.text),
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
        types::Hook::Chat => api::Hook::Chat,
        types::Hook::Kick => api::Hook::Kick,
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
    let hooks: Vec<_> = registration.hooks.into_iter().map(hook).collect();
    api::Registration {
        commands: registration
            .commands
            .into_iter()
            .map(|command| api::CommandSpec {
                name: command.name,
                aliases: command.aliases,
                permission: command.permission,
                suggest: suggest_kind(command.suggest),
                console: command.console,
            })
            .collect(),
        // Never implied by an empty list, so listing them is the claim.
        chat: hooks.contains(&api::Hook::Chat),
        kick: hooks.contains(&api::Hook::Kick),
        hooks,
    }
}

pub(super) fn route_decision(decision: types::RouteDecision) -> api::RouteDecision {
    match decision {
        types::RouteDecision::Default => api::RouteDecision::Default,
        types::RouteDecision::Backend(backend) => api::RouteDecision::Backend(backend),
        types::RouteDecision::Reject(reason) => api::RouteDecision::Reject { reason },
    }
}

/// A decision. Fails for text that is not valid; the host then treats the
/// decision like any other invalid one.
pub(super) fn decision(decision: types::Decision) -> Result<api::Action, String> {
    Ok(match decision {
        types::Decision::Continue => api::Action::Continue,
        types::Decision::Deny(reason) => api::Action::Deny(text(reason, "deny reason")?),
        types::Decision::Server(server) => api::Action::Server(server),
        types::Decision::Message(message) => api::Action::Message(text(message, "message")?),
        types::Decision::Queue(server) => api::Action::Queue(server),
        types::Decision::LeaveQueue => api::Action::LeaveQueue,
    })
}

/// Suggestions. An invalid tooltip fails the whole answer.
pub(super) fn suggestions(
    suggestions: Vec<types::Suggestion>,
) -> Result<Vec<api::Suggestion>, String> {
    suggestions
        .into_iter()
        .map(|suggestion| {
            Ok(api::Suggestion {
                text: suggestion.text,
                tooltip: suggestion
                    .tooltip
                    .map(|tooltip| text(tooltip, "suggestion tooltip"))
                    .transpose()?,
            })
        })
        .collect()
}

/// A status override. Fails for a version or sample line that is not valid
/// text.
pub(super) fn status_override(
    status: types::StatusOverride,
) -> Result<api::StatusOverride, String> {
    Ok(api::StatusOverride {
        motd_json: status.motd.map(text_json),
        version: status
            .version
            .map(|version| text(version, "status version"))
            .transpose()?,
        protocol: status.protocol,
        online: status.online,
        max_players: status.max_players,
        sample: status
            .sample
            .map(|lines| {
                lines
                    .into_iter()
                    .map(|line| text(line, "status sample line"))
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
        ip: player.ip,
        properties: player
            .properties
            .into_iter()
            .map(|property| types::ProfileProperty {
                name: property.name,
                value: property.value,
                signature: property.signature,
            })
            .collect(),
        visible_to: player.visible_to,
        locale: player.locale,
        language: player.language,
    }
}

fn player_record(record: api::PlayerRecord) -> types::PlayerRecord {
    types::PlayerRecord {
        uuid: record.uuid,
        name: record.name,
        first_seen: record.first_seen,
        last_seen: record.last_seen,
        last_ip: record.last_ip,
        last_server: record.last_server,
        online: record.online,
        language: record.language,
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
        language: context.language,
        console: context.console,
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

/// A chat message or backend command: `context.chat` holds it.
pub(super) fn chat_attempt(mut context: api::Context) -> types::ChatAttempt {
    let chat = context.chat.take().unwrap_or(api::ChatInput {
        kind: api::ChatKind::Message,
        text: String::new(),
    });
    types::ChatAttempt {
        ctx: self::context(context),
        kind: match chat.kind {
            api::ChatKind::Message => types::ChatKind::Message,
            api::ChatKind::Command => types::ChatKind::Command,
        },
        text: chat.text,
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

    fn message(&mut self, target: String, message: types::Text) -> Result<bool, String> {
        let players = self.services.players();
        let message = players.for_target(&target, || text(message, "message"))?;
        players.message_text(&target, message)
    }

    fn action_bar(&mut self, target: String, message: types::Text) -> Result<bool, String> {
        let players = self.services.players();
        let message = players.for_target(&target, || text(message, "action bar text"))?;
        players.action_bar_text(&target, message)
    }

    fn title(&mut self, target: String, spec: types::TitleSpec) -> Result<bool, String> {
        let players = self.services.players();
        let spec = players.for_target(&target, || title(spec))?;
        players.title(&target, &spec)
    }

    fn tab_list(
        &mut self,
        target: String,
        header: Option<types::Text>,
        footer: Option<types::Text>,
    ) -> Result<bool, String> {
        let (header, footer) = (header.map(text_json), footer.map(text_json));
        self.services
            .players()
            .tab_list(&target, header.as_deref(), footer.as_deref())
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

    fn kick(&mut self, target: String, reason: types::Text) -> Result<bool, String> {
        let players = self.services.players();
        let reason = players.for_target(&target, || text(reason, "kick reason"))?;
        players.kick_text(&target, reason)
    }

    fn transfer(&mut self, target: String, server: String) -> Result<bool, String> {
        self.services.players().transfer(&target, &server)
    }

    fn broadcast(&mut self, message: types::Text) -> Result<u32, String> {
        self.services
            .players()
            .broadcast_text(text_as_written(message, "message")?, None)
    }

    fn lookup(&mut self, target: String) -> Result<Option<types::PlayerRecord>, String> {
        Ok(self.services.players().lookup(&target)?.map(player_record))
    }

    fn search(&mut self, prefix: String, limit: u32) -> Result<Vec<types::PlayerRecord>, String> {
        let records = self.services.players().search(&prefix, limit)?;
        Ok(records.into_iter().map(player_record).collect())
    }

    fn set_language(&mut self, target: String, language: Option<String>) -> Result<bool, String> {
        self.services
            .players()
            .set_language(&target, language.as_deref())
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

    fn add_group(
        &mut self,
        uuid: String,
        group: String,
        expires_at: Option<u64>,
    ) -> Result<bool, String> {
        self.services
            .permissions()
            .add_group_until(&uuid, &group, expires_at)
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

    fn meta(&mut self, uuid: String, key: String) -> Option<String> {
        self.services.permissions().meta_value(&uuid, &key)
    }

    fn set_meta(
        &mut self,
        uuid: String,
        key: String,
        value: Option<String>,
        contexts_json: Option<String>,
        expires_at: Option<u64>,
    ) -> Result<bool, String> {
        let contexts = match contexts_json {
            None => rift_permissions::ContextSet::new(),
            Some(json) => {
                serde_json::from_str(&json).map_err(|error| format!("contexts-json: {error}"))?
            }
        };
        self.services
            .permissions()
            .set_meta(&uuid, &key, value.as_deref(), contexts, expires_at)
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

impl background::Host for State {
    fn defer(&mut self) -> Result<u64, String> {
        self.services.background().defer()
    }

    fn resolve(&mut self, ticket: u64, answer: types::Decision) -> Result<bool, String> {
        let background = self.services.background();
        let action = background
            .for_ticket(ticket, || decision(answer))
            .map_err(|error| format!("resolve: {error}"))?;
        background.resolve(ticket, action)
    }

    fn run(&mut self, payload: Vec<u8>) -> Result<u64, String> {
        self.services.background().run(payload)
    }

    fn is_worker(&mut self) -> bool {
        self.services.is_worker()
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

    fn api(&mut self) -> String {
        self.services.meta().api().to_owned()
    }
}

impl State {
    /// One function of the `text` interface (see [`Services::text`]).
    ///
    /// [`Services::text`]: crate::plugins::services::Services::text
    fn write_text(
        &mut self,
        text: types::Text,
        language: Option<String>,
        form: crate::text::Form,
        label: &str,
    ) -> Result<String, String> {
        let text = text_as_written(text, label)?;
        self.services.text(&text, language.as_deref(), form, label)
    }
}

impl text_api::Host for State {
    fn render(&mut self, text: types::Text, language: Option<String>) -> Result<String, String> {
        self.write_text(text, language, crate::text::Form::Json, "text.render")
    }

    fn plain(&mut self, text: types::Text, language: Option<String>) -> Result<String, String> {
        self.write_text(text, language, crate::text::Form::Plain, "text.plain")
    }

    fn to_legacy(&mut self, text: types::Text, language: Option<String>) -> Result<String, String> {
        self.write_text(text, language, crate::text::Form::Legacy, "text.to-legacy")
    }

    fn to_mini(&mut self, text: types::Text, language: Option<String>) -> Result<String, String> {
        self.write_text(text, language, crate::text::Form::Mini, "text.to-mini")
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
        // `dialog`, `job_done` and `menu` have no WIT case: they need no
        // listing.
        assert_eq!(names, api::Hook::ALL[..14]);
        assert_eq!(hook(types::Hook::Chat), api::Hook::Chat);
        assert_eq!(hook(types::Hook::Kick), api::Hook::Kick);
        let listed = registration(types::Registration {
            commands: vec![types::CommandSpec {
                name: "ban".into(),
                aliases: vec!["b".into()],
                permission: String::new(),
                suggest: types::SuggestKind::Players,
                console: true,
            }],
            hooks: vec![types::Hook::Chat],
        });
        assert!(listed.chat && !listed.kick);
        assert!(listed.commands[0].console);
        assert!(listed.commands[0].answers_to("b"));
        let attempt = chat_attempt(api::Context {
            id: 9,
            command: Some("msg".into()),
            args: "Alex hi".into(),
            chat: Some(api::ChatInput {
                kind: api::ChatKind::Command,
                text: "msg Alex hi".into(),
            }),
            ..Default::default()
        });
        assert_eq!(attempt.kind, types::ChatKind::Command);
        assert_eq!(attempt.text, "msg Alex hi");
        assert_eq!(attempt.ctx.command.as_deref(), Some("msg"));
        assert_eq!(attempt.ctx.args, "Alex hi");
        let converted = context(api::Context {
            id: 9,
            name: "Alex".into(),
            position: 2,
            server: Some("lobby".into()),
            language: Some("de".into()),
            console: true,
            ..Default::default()
        });
        assert_eq!(converted.connection_id, 9);
        assert_eq!(converted.position, 2);
        assert_eq!(converted.server.as_deref(), Some("lobby"));
        assert_eq!(converted.language.as_deref(), Some("de"));
        assert!(converted.console);
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
            context: serde_json::json!({
                "operator": {"name": "viewer", "permissions": ["read"], "groups": null},
            }),
        });
        assert_eq!(request.headers, [("a".to_owned(), "b".to_owned())]);
        assert_eq!(
            request.context_json,
            r#"{"operator":{"groups":null,"name":"viewer","permissions":["read"]}}"#
        );
    }

    #[test]
    fn text_is_plain_or_a_checked_component() {
        // Plain text is never parsed, whatever it looks like.
        assert_eq!(
            text(types::Text::Plain("{not json".into()), "message").unwrap(),
            Text::Plain("{not json".into())
        );
        assert_eq!(
            text(
                types::Text::Json(r#"{"text":"Tip","color":"red"}"#.into()),
                "message"
            )
            .unwrap(),
            Text::Component(serde_json::json!({"text": "Tip", "color": "red"}))
        );
        let error = text(types::Text::Json("{nope".into()), "suggestion tooltip").unwrap_err();
        assert!(error.contains("suggestion tooltip"), "{error}");
        let error = text(types::Text::Json("as written".into()), "message").unwrap_err();
        assert!(error.contains("JSON text component"), "{error}");
        assert!(text(types::Text::Plain(String::new()), "message").is_err());

        assert_eq!(
            decision(types::Decision::Deny(types::Text::Plain("no".into()))).unwrap(),
            api::Action::Deny("no".into())
        );
        assert!(decision(types::Decision::Message(types::Text::Json("[1,".into()))).is_err());

        let status = status_override(types::StatusOverride {
            motd: Some(types::Text::Plain("motd".into())),
            version: Some(types::Text::Json(
                r#"{"text":"Rift","color":"gold"}"#.into(),
            )),
            protocol: Some(774),
            online: Some(1),
            max_players: Some(2),
            sample: Some(vec![
                types::Text::Plain("one".into()),
                types::Text::Json("\"two\"".into()),
            ]),
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
        let title = title(types::TitleSpec {
            title: None,
            subtitle: Some(types::Text::Plain("sub".into())),
            fade_in: 1,
            stay: 2,
            fade_out: 3,
        })
        .unwrap();
        assert_eq!(title.title, Text::Plain(String::new()));
        assert_eq!(title.subtitle, Some(Text::Plain("sub".into())));
    }
}
