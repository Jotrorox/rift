//! Shared messaging listener and adapters for Rift's existing control plane.
use crate::{admin, events, metrics::Metrics, runtime::Snapshot};
use bytes::Bytes;
use rift::messaging::{Broker, Message, Stream, Subscription, quic};
use serde_json::{Value, json};
use std::{collections::HashMap, io, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, watch},
    time::timeout,
};

pub async fn open_streams(config: &rift::config::Config) -> io::Result<HashMap<String, Stream>> {
    let mut streams = HashMap::new();
    if let Some(settings) = &config.messaging {
        for (name, stream) in &settings.streams {
            let store = match &stream.storage_path {
                Some(path) => Stream::open(path, stream.config.clone()).await,
                None => Stream::memory(stream.config.clone()),
            }
            .map_err(|error| io::Error::other(format!("messaging.streams.{name}: {error}")))?;
            streams.insert(name.clone(), store);
        }
    }
    Ok(streams)
}

/// Bind only after certificates and all environment credentials have validated.
pub async fn listen(snapshot: &Snapshot) -> io::Result<Option<quic::Server>> {
    let Some(settings) = snapshot.config.messaging.clone() else {
        return Ok(None);
    };
    let Some(address) = settings.listen else {
        return Ok(None);
    };
    let nodes = snapshot.config.nodes.clone();
    let (tls, auth) = tokio::task::spawn_blocking(move || -> io::Result<_> {
        let tls = quic::tls_server_config(
            settings
                .certificate
                .as_ref()
                .ok_or_else(|| io::Error::other("missing messaging certificate"))?,
            settings
                .private_key
                .as_ref()
                .ok_or_else(|| io::Error::other("missing messaging private key"))?,
        )
        .map_err(io::Error::other)?;
        let mut identities = Vec::with_capacity(settings.principals.len() + nodes.len());
        for (name, node) in nodes {
            let token = node_token(&name, &node.token_env)?;
            let (publish, subscribe) = rift::node::protocol::principal_grants(&name);
            identities.push(quic::Identity {
                name: rift::config::NodeConfig::principal(&name),
                token,
                publish,
                subscribe,
                control: false,
                players: false,
            });
        }
        for principal in settings.principals {
            let token = std::env::var(&principal.token_env).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "messaging: set {} to a secret of at least 32 bytes",
                        principal.token_env
                    ),
                )
            })?;
            if !(32..=1024).contains(&token.len()) || token.chars().any(char::is_control) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "messaging token must contain 32..=1024 bytes without control characters",
                ));
            }
            identities.push(quic::Identity {
                name: principal.name,
                token,
                publish: principal.publish,
                subscribe: principal.subscribe,
                control: principal.control,
                players: principal.players,
            });
        }
        Ok((
            tls,
            quic::AuthConfig {
                identities,
                max_connections: settings.max_connections,
                max_connections_per_address: settings.max_connections_per_address,
                max_connections_per_identity: settings.max_connections_per_principal,
                max_streams_per_connection: settings.max_streams_per_connection,
                handshake_timeout: settings.handshake_timeout,
                io_timeout: settings.io_timeout,
            },
        ))
    })
    .await
    .map_err(io::Error::other)??;
    quic::Server::bind_with_streams(
        address,
        tls,
        snapshot.messaging.clone(),
        auth,
        (*snapshot.messaging_streams).clone(),
    )
    .map(Some)
    .map_err(io::Error::other)
}

/// A node agent's token, shared by its QUIC principal and template downloads.
pub fn node_token(name: &str, variable: &str) -> io::Result<String> {
    let token = std::env::var(variable).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("nodes.{name}: set {variable} to a secret of at least 32 bytes"),
        )
    })?;
    if !(32..=1024).contains(&token.len()) || token.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("nodes.{name}: token must contain 32..=1024 bytes without control characters"),
        ));
    }
    Ok(token)
}

/// Subscribing before returning prevents a readiness race for control requests.
pub struct Controls {
    broker: Broker,
    subscription: Subscription,
    long_running: Arc<tokio::sync::Semaphore>,
}

impl Controls {
    pub fn new(broker: Broker) -> io::Result<Self> {
        let subscription = broker
            .subscribe("rift.control.*", None)
            .map_err(io::Error::other)?;
        Ok(Self {
            broker,
            subscription,
            long_running: Arc::new(tokio::sync::Semaphore::new(16)),
        })
    }

    pub async fn run(
        mut self,
        current: watch::Receiver<Arc<Snapshot>>,
        metrics: Arc<Metrics>,
        commands: mpsc::Sender<admin::Command>,
        audit: Arc<crate::operator::Store>,
    ) {
        loop {
            let message = match self.subscription.recv().await {
                Ok(message) => message,
                Err(error) => {
                    // A burst must not permanently disable the control service.
                    events::admin("messaging_control", "rejected", "control", &error);
                    match self.broker.subscribe("rift.control.*", None) {
                        Ok(subscription) => {
                            self.subscription = subscription;
                            continue;
                        }
                        Err(_) => return,
                    }
                }
            };
            // Control channels are request/reply only. Refuse arbitrary reply
            // subjects so a caller cannot trick Rift into publishing commands.
            let Some(reply) = message
                .reply
                .as_deref()
                .filter(|s| s.starts_with("_INBOX."))
            else {
                continue;
            };
            let snapshot = current.borrow().clone();
            let command = message
                .subject
                .strip_prefix("rift.control.")
                .unwrap_or("unknown")
                .to_owned();
            let args = arguments(&message);
            let actor = rift::players_service::actor(message.sender.as_deref(), CONTROL_ACTOR);
            let broker = self.broker.clone();
            let reply = reply.to_owned();
            let (metrics, commands, audit) = (metrics.clone(), commands.clone(), audit.clone());
            let work = async move {
                // Reads such as status, which dashboards poll, stay out of
                // the audit log; everything that changes something is in it.
                let audited = args.as_ref().ok().filter(|args| mutates(args)).cloned();
                let (args, deadline) = match args {
                    // Instance and download work is serialized by Rift itself
                    // and may wait for a download; keep answering other controls.
                    Ok(args) if LONG_RUNNING.contains(&command.as_str()) => {
                        (Ok(args), CONTROL_DEADLINE + admin::DOWNLOAD_DEADLINE)
                    }
                    args => (args, CONTROL_DEADLINE),
                };
                let result = match args {
                    Ok(args) => timeout(
                        deadline,
                        admin::execute(&args, snapshot, &metrics, &commands, &actor),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err("control request timed out; inspect status before retrying".into())
                    }),
                    Err(error) => Err(error),
                };
                let outcome = if result.is_ok() { "ok" } else { "rejected" };
                respond(&broker, &reply, &command, result);
                if let Some(args) = audited {
                    let (outcome, status) = (outcome, if outcome == "ok" { 200 } else { 400 });
                    let target = args[1..].join(" ");
                    if let Err(error) = audit
                        .audit_async(
                            &actor,
                            &format!("control.{command}"),
                            &target,
                            outcome,
                            status,
                        )
                        .await
                    {
                        eprintln!("rift: audit of rift.control.{command} failed: {error}");
                    }
                }
            };
            if LONG_RUNNING.contains(&message.subject.trim_start_matches("rift.control.")) {
                match self.long_running.clone().try_acquire_owned() {
                    Ok(permit) => {
                        tokio::spawn(async move {
                            work.await;
                            drop(permit);
                        });
                    }
                    Err(_) => respond(
                        &self.broker,
                        message.reply.as_deref().unwrap_or_default(),
                        message.subject.trim_start_matches("rift.control."),
                        Err(
                            "too many instance or download requests in progress; retry later"
                                .into(),
                        ),
                    ),
                }
            } else {
                work.await;
            }
        }
    }
}

const CONTROL_DEADLINE: Duration = Duration::from_secs(40);
/// Who a `rift.control.*` request without a sender acts as: one from the
/// configuration script. Remote callers act as `bus:<login>` from
/// [`Message::sender`] and persistent plugins as `plugin:<name>` (see
/// [`rift::players_service::actor`]). The dot keeps it apart from every
/// `web.operators` name, which may not contain one.
const CONTROL_ACTOR: &str = "rift.control";

/// Whether a decoded control command changes something.
fn mutates(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some("status" | "servers" | "groups" | "nodes" | "templates") | None => false,
        Some("plugins") => args.get(1).is_some_and(|action| action == "restart"),
        Some(_) => true,
    }
}

/// How many `rift.players.*` requests run at once. Each is a quick,
/// non-blocking directory operation; the bound only keeps a flood from
/// queueing unbounded audit writes.
const PLAYER_REQUESTS: usize = 64;
/// Who a `rift.players.*` request without a sender acts as (see
/// [`CONTROL_ACTOR`]).
const PLAYERS_ACTOR: &str = "rift.players";

/// The `players` interface of the plugin contract on the bus, as
/// `rift.players.<op>` (see [`rift::players_service`]).
///
/// Not part of [`Controls`] on purpose: control arguments are positional
/// strings and its loop is serialized, while player calls are typed JSON
/// and independent of each other, so they run concurrently. Like control,
/// it answers only `_INBOX.` reply subjects, so a caller cannot make Rift
/// publish on a subject of its choosing; unlike control, it publishes no
/// `rift.events.control` event per action, because a ban wave or a friends
/// plugin would flood every subscriber with them.
pub struct PlayerRequests {
    broker: Broker,
    subscription: Subscription,
}

impl PlayerRequests {
    /// Subscribes at once, so requests are answered as soon as the bus is up.
    pub fn new(broker: Broker) -> io::Result<Self> {
        let subscription = broker
            .subscribe(&format!("{}.*", rift::players_service::PREFIX), None)
            .map_err(io::Error::other)?;
        Ok(Self {
            broker,
            subscription,
        })
    }

    pub async fn run(
        mut self,
        current: watch::Receiver<Arc<Snapshot>>,
        audit: Arc<crate::operator::Store>,
    ) {
        let permits = Arc::new(tokio::sync::Semaphore::new(PLAYER_REQUESTS));
        loop {
            let message = match self.subscription.recv().await {
                Ok(message) => message,
                Err(error) => {
                    events::admin("messaging_players", "rejected", "players", &error);
                    match self
                        .broker
                        .subscribe(&format!("{}.*", rift::players_service::PREFIX), None)
                    {
                        Ok(subscription) => {
                            self.subscription = subscription;
                            continue;
                        }
                        Err(_) => return,
                    }
                }
            };
            let Some(reply) = message
                .reply
                .as_deref()
                .filter(|reply| reply.starts_with("_INBOX."))
                .map(str::to_owned)
            else {
                continue;
            };
            let Ok(permit) = permits.clone().acquire_owned().await else {
                return;
            };
            let players = current.borrow().extensions.players().clone();
            let (broker, audit) = (self.broker.clone(), audit.clone());
            tokio::spawn(async move {
                let operation = message
                    .subject
                    .strip_prefix(rift::players_service::PREFIX)
                    .and_then(|rest| rest.strip_prefix('.'))
                    .unwrap_or_default();
                let request = rift::players_service::Request::decode(operation, &message.payload);
                let audited = request
                    .as_ref()
                    .ok()
                    .filter(|request| request.acts())
                    .map(|request| (request.operation(), request.target()));
                let result = request.and_then(|request| request.execute(&players));
                let response = match &result {
                    Ok(data) => json!({"ok": true, "data": data}),
                    Err(error) => {
                        events::admin("messaging_players", "rejected", operation, error);
                        json!({"ok": false, "error": error})
                    }
                };
                let _ = broker.publish(&reply, Bytes::from(response.to_string()));
                if let Some((operation, target)) = audited {
                    let actor =
                        rift::players_service::actor(message.sender.as_deref(), PLAYERS_ACTOR);
                    let (outcome, status) = match &result {
                        Ok(_) => ("ok", 200),
                        Err(_) => ("rejected", 400),
                    };
                    if let Err(error) = audit
                        .audit_async(
                            &actor,
                            &format!("players.{operation}"),
                            &target,
                            outcome,
                            status,
                        )
                        .await
                    {
                        eprintln!("rift: audit of rift.players.{operation} failed: {error}");
                    }
                }
                drop(permit);
            });
        }
    }
}
/// Subjects answered from their own task rather than the serialized loop.
const LONG_RUNNING: &[&str] = &["create", "remove", "download"];

fn respond(broker: &Broker, reply: &str, command: &str, result: Result<Value, String>) {
    let response = match result {
        Ok(data) => {
            events::admin("messaging_control", "ok", command, &"command completed");
            json!({"ok": true, "data": data})
        }
        Err(error) => {
            events::admin("messaging_control", "rejected", command, &error);
            json!({"ok": false, "error": error})
        }
    };
    let _ = broker.publish(reply, Bytes::from(response.to_string()));
    // Emit bounded operational results; status contains player details
    // and is deliberately returned only to its requester's inbox.
    if command != "status" {
        emit(
            broker,
            "rift.events.control",
            json!({"command":command,"ok":response["ok"]}),
        );
    }
}

pub fn emit(broker: &Broker, subject: &str, value: Value) {
    let _ = broker.publish(subject, Bytes::from(value.to_string()));
}

fn arguments(message: &Message) -> Result<Vec<String>, String> {
    if message.payload.len() > 8192 {
        return Err("control payload exceeds 8 KiB".into());
    }
    let value: Value =
        serde_json::from_slice(&message.payload).map_err(|_| "expected a JSON object")?;
    let object = value.as_object().ok_or("expected a JSON object")?;
    let command = message
        .subject
        .strip_prefix("rift.control.")
        .ok_or("invalid control subject")?;
    let fields: &[&str] = match command {
        "status" | "reload" | "servers" | "groups" | "nodes" | "templates" => &[],
        "maintenance" => &["enabled"],
        "drain" => &["backend", "enabled"],
        "transfer" => &["connection_id", "backend"],
        "create" => &["group"],
        "remove" | "start" | "stop" => &["name"],
        "download" => &["template", "refresh"],
        "plugins" => &["action", "name"],
        _ => return Err("unknown control subject".into()),
    };
    if object.keys().any(|key| !fields.contains(&key.as_str())) {
        return Err("unknown control payload field".into());
    }
    let enabled = || {
        value["enabled"]
            .as_bool()
            .map(|enabled| if enabled { "on" } else { "off" }.to_owned())
            .ok_or("enabled must be a boolean".to_owned())
    };
    let backend = || {
        value["backend"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 256)
            .map(str::to_owned)
            .ok_or("backend must be a nonempty string of at most 256 bytes".to_owned())
    };
    let name = |field: &str| {
        value[field]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 256)
            .map(str::to_owned)
            .ok_or(format!(
                "{field} must be a nonempty string of at most 256 bytes"
            ))
    };
    let mut args = vec![command.to_owned()];
    match command {
        "create" => args.push(name("group")?),
        "remove" | "start" | "stop" => args.push(name("name")?),
        "download" => {
            args.push(name("template")?);
            match value.get("refresh") {
                None | Some(Value::Bool(false)) => {}
                Some(Value::Bool(true)) => args.push("refresh".into()),
                Some(_) => return Err("refresh must be a boolean".into()),
            }
        }
        "plugins" => {
            let action = value["action"]
                .as_str()
                .filter(|action| ["list", "status", "logs", "restart"].contains(action))
                .ok_or("action must be list, status, logs or restart")?;
            args.push(action.to_owned());
            match (action, value.get("name")) {
                ("list", None) => {}
                ("list", Some(_)) => return Err("list takes no name".into()),
                (_, _) => args.push(name("name")?),
            }
        }
        "maintenance" => args.push(enabled()?),
        "drain" => {
            args.push(backend()?);
            args.push(enabled()?);
        }
        "transfer" => {
            args.push(
                value["connection_id"]
                    .as_u64()
                    .filter(|id| *id != 0)
                    .ok_or("connection_id must be a positive integer")?
                    .to_string(),
            );
            args.push(backend()?);
        }
        _ => {}
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(subject: &str, payload: &str) -> Message {
        Message {
            subject: subject.into(),
            reply: Some("_INBOX.test".into()),
            payload: Bytes::copy_from_slice(payload.as_bytes()),
            sender: None,
        }
    }

    #[test]
    fn control_payloads_are_typed_and_strict() {
        assert_eq!(
            arguments(&message(
                "rift.control.drain",
                r#"{"backend":"lobby","enabled":true}"#
            ))
            .unwrap(),
            ["drain", "lobby", "on"]
        );
        assert_eq!(
            arguments(&message(
                "rift.control.transfer",
                r#"{"backend":"lobby","connection_id":1}"#
            ))
            .unwrap(),
            ["transfer", "1", "lobby"]
        );
        for (subject, payload, expected) in [
            (
                "rift.control.create",
                r#"{"group":"lobby"}"#,
                &["create", "lobby"][..],
            ),
            (
                "rift.control.remove",
                r#"{"name":"lobby-1"}"#,
                &["remove", "lobby-1"],
            ),
            (
                "rift.control.start",
                r#"{"name":"lobby-1"}"#,
                &["start", "lobby-1"],
            ),
            (
                "rift.control.stop",
                r#"{"name":"lobby-1"}"#,
                &["stop", "lobby-1"],
            ),
            ("rift.control.servers", "{}", &["servers"]),
            ("rift.control.groups", "{}", &["groups"]),
            ("rift.control.nodes", "{}", &["nodes"]),
            ("rift.control.templates", "{}", &["templates"]),
            (
                "rift.control.plugins",
                r#"{"action":"list"}"#,
                &["plugins", "list"],
            ),
            (
                "rift.control.plugins",
                r#"{"action":"restart","name":"hello-lua"}"#,
                &["plugins", "restart", "hello-lua"],
            ),
            (
                "rift.control.download",
                r#"{"template":"paper"}"#,
                &["download", "paper"],
            ),
            (
                "rift.control.download",
                r#"{"template":"paper","refresh":true}"#,
                &["download", "paper", "refresh"],
            ),
        ] {
            assert_eq!(arguments(&message(subject, payload)).unwrap(), expected);
        }
        for (subject, payload) in [
            ("rift.control.create", r#"{"group":""}"#),
            ("rift.control.create", r#"{"name":"lobby"}"#),
            ("rift.control.remove", r#"{"name":7}"#),
            (
                "rift.control.download",
                r#"{"template":"paper","refresh":"yes"}"#,
            ),
            ("rift.control.status", r#"{"unexpected":1}"#),
            ("rift.control.plugins", "{}"),
            ("rift.control.plugins", r#"{"action":"stop","name":"a"}"#),
            ("rift.control.plugins", r#"{"action":"restart"}"#),
            ("rift.control.plugins", r#"{"action":"list","name":"a"}"#),
            ("rift.control.plugins", r#"{"action":"status","name":""}"#),
            ("rift.control.maintenance", r#"{"enabled":"true"}"#),
            (
                "rift.control.transfer",
                r#"{"connection_id":-1,"backend":"lobby"}"#,
            ),
            ("rift.control.shutdown", "{}"),
            ("rift.control.status", "[]"),
        ] {
            assert!(arguments(&message(subject, payload)).is_err());
        }
    }

    #[tokio::test]
    async fn controls_share_rift_state_and_refuse_unsafe_reply_subjects() {
        let snapshot = Arc::new(Snapshot::new(rift::config::Config::default(), None).unwrap());
        let broker = snapshot.messaging.clone();
        let controls = Controls::new(broker.clone()).unwrap();
        let (_, current) = watch::channel(snapshot.clone());
        let (commands, _) = mpsc::channel(2);
        let audit = Arc::new(crate::operator::Store::open(None).unwrap());
        let task = tokio::spawn(controls.run(
            current,
            Arc::new(Metrics::default()),
            commands,
            audit.clone(),
        ));
        let reply = broker
            .request(
                "rift.control.maintenance",
                Bytes::from_static(br#"{"enabled":true}"#),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&reply.payload).unwrap()["ok"],
            true
        );
        assert!(snapshot.control.maintenance(&snapshot.config));
        broker
            .publish_with_reply(
                "rift.control.maintenance",
                Some("rift.control.status"),
                Bytes::from_static(br#"{"enabled":false}"#),
            )
            .unwrap();
        let reply = broker
            .request(
                "rift.control.status",
                Bytes::from_static(b"{}"),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&reply.payload).unwrap()["data"]["maintenance"],
            true
        );
        for (payload, expected) in [
            (
                &br#"{"action":"list"}"#[..],
                json!({"ok":true,"data":{"plugins":[]}}),
            ),
            (
                br#"{"action":"restart","name":"ghost"}"#,
                json!({"ok":false,"error":"no persistent plugin named \"ghost\" is running"}),
            ),
        ] {
            let reply = broker
                .request(
                    "rift.control.plugins",
                    Bytes::copy_from_slice(payload),
                    Duration::from_secs(1),
                )
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&reply.payload).unwrap(),
                expected
            );
        }
        // Every caller is audited under its own name: the configuration
        // script (no sender), a plugin, and a remote login. Reads are not.
        for sender in ["plugin:bans", "ops"] {
            broker
                .request_as(
                    "rift.control.maintenance",
                    Bytes::from_static(br#"{"enabled":false}"#),
                    Duration::from_secs(1),
                    Some(sender),
                )
                .await
                .unwrap();
        }
        let records = audit.audits();
        let actors: Vec<_> = records["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| {
                format!(
                    "{} {} {}",
                    record["actor"].as_str().unwrap(),
                    record["action"].as_str().unwrap(),
                    record["target"].as_str().unwrap()
                )
            })
            .collect();
        for expected in [
            "rift.control control.maintenance on",
            "plugin:bans control.maintenance off",
            "bus:ops control.maintenance off",
            "rift.control control.plugins restart ghost",
        ] {
            assert!(
                actors.iter().any(|actor| actor == expected),
                "{expected}: {actors:?}"
            );
        }
        assert!(!actors.iter().any(|actor| actor.contains("control.status")));
        assert!(
            !actors
                .iter()
                .any(|actor| actor.contains("control.plugins list"))
        );
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn player_requests_answer_inboxes_concurrently_and_audit_actions_only() {
        let snapshot = Arc::new(Snapshot::new(rift::config::Config::default(), None).unwrap());
        let broker = snapshot.messaging.clone();
        let requests = PlayerRequests::new(broker.clone()).unwrap();
        let (_, current) = watch::channel(snapshot.clone());
        let audit = Arc::new(crate::operator::Store::open(None).unwrap());
        let task = tokio::spawn(requests.run(current, audit.clone()));
        let ask = |subject: &'static str, payload: &'static str, sender: Option<&'static str>| {
            let broker = broker.clone();
            async move {
                let reply = broker
                    .request_as(
                        subject,
                        Bytes::from_static(payload.as_bytes()),
                        Duration::from_secs(1),
                        sender,
                    )
                    .await
                    .unwrap();
                serde_json::from_slice::<Value>(&reply.payload).unwrap()
            }
        };
        // Many at once: none waits for another.
        let mut replies = tokio::task::JoinSet::new();
        for _ in 0..100 {
            replies.spawn(ask("rift.players.list", "{}", None));
        }
        while let Some(reply) = replies.join_next().await {
            assert_eq!(reply.unwrap(), json!({"ok":true,"data":[]}));
        }
        assert_eq!(
            ask(
                "rift.players.kick-json",
                r#"{"target":"Nobody","reason":{"text":"x","color":"red"}}"#,
                Some("bans")
            )
            .await,
            json!({"ok":true,"data":false})
        );
        assert_eq!(
            ask(
                "rift.players.transfer",
                r#"{"target":"Nobody","server":"missing"}"#,
                Some("plugin:hub")
            )
            .await["ok"],
            false
        );
        assert!(
            ask("rift.players.kick", r#"{"target":"Nobody"}"#, None).await["error"]
                .as_str()
                .unwrap()
                .contains("reason")
        );
        // No reply to a subject outside `_INBOX.`, so Rift cannot be made
        // to publish elsewhere.
        let mut elsewhere = broker.subscribe("game.victim", None).unwrap();
        broker
            .publish_with_reply(
                "rift.players.list",
                Some("game.victim"),
                Bytes::from_static(b"{}"),
            )
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), elsewhere.recv())
                .await
                .is_err()
        );
        let records = audit.audits();
        let records: Vec<_> = records["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| {
                format!(
                    "{} {} {} {}",
                    record["actor"].as_str().unwrap(),
                    record["action"].as_str().unwrap(),
                    record["target"].as_str().unwrap(),
                    record["outcome"].as_str().unwrap()
                )
            })
            .collect();
        assert!(
            records.contains(&"bus:bans players.kick Nobody ok".to_owned()),
            "{records:?}"
        );
        assert!(
            records.contains(&"plugin:hub players.transfer Nobody -> missing rejected".to_owned()),
            "{records:?}"
        );
        // Reads and undecodable requests are not audited.
        assert_eq!(records.len(), 2, "{records:?}");
        task.abort();
        let _ = task.await;
    }
}
