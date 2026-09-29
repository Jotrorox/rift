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
}

impl Controls {
    pub fn new(broker: Broker) -> io::Result<Self> {
        let subscription = broker
            .subscribe("rift.control.*", None)
            .map_err(io::Error::other)?;
        Ok(Self {
            broker,
            subscription,
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
            let result = match arguments(&message) {
                Ok(args) => timeout(
                    Duration::from_secs(40),
                    admin::execute(&args, snapshot, &metrics, &commands),
                )
                .await
                .unwrap_or_else(|_| {
                    Err("control request timed out; inspect status before retrying".into())
                }),
                Err(error) => Err(error),
            };
            let command = message
                .subject
                .strip_prefix("rift.control.")
                .unwrap_or("unknown");
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
            let _ = self
                .broker
                .publish(reply, Bytes::from(response.to_string()));
            // Emit bounded operational results; status contains player details
            // and is deliberately returned only to its requester's inbox.
            if command != "status" {
                emit(
                    &self.broker,
                    "rift.events.control",
                    json!({"command":command,"ok":response["ok"]}),
                );
            }
        }
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
        "status" | "reload" => &[],
        "maintenance" => &["enabled"],
        "drain" => &["backend", "enabled"],
        "transfer" => &["connection_id", "backend"],
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
    let mut args = vec![command.to_owned()];
    match command {
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
        for (subject, payload) in [
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
