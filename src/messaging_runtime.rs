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
        let mut identities = Vec::with_capacity(settings.principals.len());
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
            });
        }
        Ok((
            tls,
            quic::AuthConfig {
                identities,
                max_connections: settings.max_connections,
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
            let broker = self.broker.clone();
            let reply = reply.to_owned();
            let (metrics, commands) = (metrics.clone(), commands.clone());
            let work = async move {
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
                        admin::execute(&args, snapshot, &metrics, &commands),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err("control request timed out; inspect status before retrying".into())
                    }),
                    Err(error) => Err(error),
                };
                respond(&broker, &reply, &command, result);
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
        "status" | "reload" | "servers" | "groups" | "templates" => &[],
        "maintenance" => &["enabled"],
        "drain" => &["backend", "enabled"],
        "transfer" => &["connection_id", "backend"],
        "create" => &["group"],
        "remove" | "start" | "stop" => &["name"],
        "download" => &["template", "refresh"],
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
            ("rift.control.templates", "{}", &["templates"]),
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
        let task = tokio::spawn(controls.run(current, Arc::new(Metrics::default()), commands));
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
        task.abort();
        let _ = task.await;
    }
}
