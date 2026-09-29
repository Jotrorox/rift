use super::*;
use std::path::PathBuf;

/// Optional external QUIC endpoint and limits for the shared in-process bus.
/// An absent listener keeps the broker available locally without binding UDP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagingConfig {
    pub listen: Option<SocketAddr>,
    pub certificate: Option<PathBuf>,
    pub private_key: Option<PathBuf>,
    pub principals: Vec<MessagingPrincipal>,
    pub subscription_capacity: usize,
    pub max_payload_bytes: usize,
    pub max_subject_bytes: usize,
    pub max_subscriptions: usize,
    pub max_connections: usize,
    pub max_streams_per_connection: usize,
    pub handshake_timeout: Duration,
    pub io_timeout: Duration,
    pub subscriptions: Vec<MessagingSubscription>,
    pub streams: BTreeMap<String, MessagingStream>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagingPrincipal {
    pub name: String,
    /// Environment variable name; configurations and diagnostics never hold tokens.
    pub token_env: String,
    pub publish: Vec<String>,
    pub subscribe: Vec<String>,
    pub control: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagingSubscription {
    pub subject: String,
    pub queue: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagingStream {
    /// None selects bounded memory storage; a path selects persistent storage.
    pub storage_path: Option<PathBuf>,
    pub config: crate::messaging::StreamConfig,
}

impl Default for MessagingConfig {
    fn default() -> Self {
        let broker = crate::messaging::BrokerConfig::default();
        Self {
            listen: None,
            certificate: None,
            private_key: None,
            principals: Vec::new(),
            subscription_capacity: broker.subscription_capacity,
            max_payload_bytes: broker.max_payload_bytes,
            max_subject_bytes: broker.max_subject_bytes,
            max_subscriptions: broker.max_subscriptions,
            max_connections: 1024,
            max_streams_per_connection: 128,
            handshake_timeout: Duration::from_secs(5),
            io_timeout: Duration::from_secs(30),
            subscriptions: Vec::new(),
            streams: BTreeMap::new(),
        }
    }
}

impl MessagingConfig {
    pub fn broker_config(&self) -> crate::messaging::BrokerConfig {
        crate::messaging::BrokerConfig {
            subscription_capacity: self.subscription_capacity,
            max_payload_bytes: self.max_payload_bytes,
            max_subject_bytes: self.max_subject_bytes,
            max_subscriptions: self.max_subscriptions,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.max_subject_bytes < 64 {
            return Err("messaging.max_subject_bytes: at least 64 bytes are required for Rift control and request inbox subjects".into());
        }
        if self.listen.is_some() {
            if self.max_payload_bytes > 1024 * 1024 {
                return Err(
                    "messaging.max_payload_bytes: QUIC supports at most 1048576 bytes".into(),
                );
            }
            if self.certificate.is_none() || self.private_key.is_none() {
                return Err(
                    "messaging.listen: certificate and private_key are required for QUIC TLS"
                        .into(),
                );
            }
            if self.principals.is_empty() {
                return Err("messaging.principals: grant at least one authenticated principal for a QUIC listener".into());
            }
        } else if self.certificate.is_some()
            || self.private_key.is_some()
            || !self.principals.is_empty()
        {
            return Err("messaging.listen: required when TLS or principals are configured".into());
        }
        for (key, value, max) in [
            (
                "subscription_capacity",
                self.subscription_capacity,
                1_000_000,
            ),
            (
                "max_payload_bytes",
                self.max_payload_bytes,
                16 * 1024 * 1024,
            ),
            ("max_subject_bytes", self.max_subject_bytes, 1024),
            ("max_subscriptions", self.max_subscriptions, 1_000_000),
            ("max_connections", self.max_connections, 1_000_000),
            (
                "max_streams_per_connection",
                self.max_streams_per_connection,
                65535,
            ),
        ] {
            if value == 0 || value > max {
                return Err(format!("messaging.{key}: expected an integer in 1..={max}"));
            }
        }
        for (key, timeout) in [
            ("handshake_timeout_ms", self.handshake_timeout),
            ("io_timeout_ms", self.io_timeout),
        ] {
            if !(Duration::from_millis(1)..=Duration::from_secs(86400)).contains(&timeout) {
                return Err(format!(
                    "messaging.{key}: expected 1..=86400000 milliseconds"
                ));
            }
        }
        for (key, value) in [
            ("certificate", &self.certificate),
            ("private_key", &self.private_key),
        ] {
            if value
                .as_ref()
                .is_some_and(|path| path.as_os_str().is_empty())
            {
                return Err(format!("messaging.{key}: path must not be empty"));
            }
        }
        let mut names = BTreeSet::new();
        let mut tokens = BTreeSet::new();
        for principal in &self.principals {
            if principal.name.trim().is_empty()
                || principal.name.len() > 96
                || !principal
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
                || !names.insert(&principal.name)
            {
                return Err(
                    "messaging.principals: names must be unique and contain 1..=96 letters, digits, dots, underscores or hyphens"
                        .into(),
                );
            }
            let mut bytes = principal.token_env.bytes();
            if !bytes
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || !tokens.insert(&principal.token_env)
            {
                return Err(
                    "messaging.principals.token_env: expected a unique environment variable name"
                        .into(),
                );
            }
            for pattern in principal.publish.iter().chain(&principal.subscribe) {
                pattern_valid(pattern, self.max_subject_bytes)?;
            }
        }
        let mut subscriptions = BTreeSet::new();
        for subscription in &self.subscriptions {
            pattern_valid(&subscription.subject, self.max_subject_bytes)?;
            if let Some(queue) = &subscription.queue
                && (queue.is_empty()
                    || queue.len() > 128
                    || !queue
                        .bytes()
                        .all(|byte| byte.is_ascii_graphic() && byte != b'*' && byte != b'>'))
            {
                return Err("messaging.subscriptions.queue: expected a nonempty queue name without wildcards or whitespace".into());
            }
            if !subscriptions.insert((&subscription.subject, &subscription.queue)) {
                return Err("messaging.subscriptions: duplicate subscription".into());
            }
        }
        if self.subscriptions.len() >= self.max_subscriptions {
            return Err(
                "messaging.subscriptions must leave one max_subscriptions slot for Rift control"
                    .into(),
            );
        }
        let mut paths = BTreeSet::new();
        for (name, stream) in &self.streams {
            if name.is_empty()
                || name.len() > 96
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err("messaging.streams: names must contain 1..=96 letters, digits, underscores or hyphens".into());
            }
            if let Some(path) = &stream.storage_path
                && (path.as_os_str().is_empty() || !paths.insert(path))
            {
                return Err(
                    "messaging.streams.storage_path: paths must be nonempty and unique".into(),
                );
            }
            let config = &stream.config;
            if config.subjects.is_empty() {
                return Err(format!(
                    "messaging.streams.{name}.subjects: at least one pattern is required"
                ));
            }
            for subject in &config.subjects {
                pattern_valid(subject, self.max_subject_bytes)?;
            }
            if config.max_messages == 0
                || config.max_bytes == 0
                || config.max_payload_bytes == 0
                || config.max_consumers == 0
            {
                return Err(format!("messaging.streams.{name}: limits must be positive"));
            }
            if config.max_payload_bytes > self.max_payload_bytes
                || config.max_payload_bytes > config.max_bytes
            {
                return Err(format!(
                    "messaging.streams.{name}.max_payload_bytes: must fit both broker and stream byte limits"
                ));
            }
        }
        Ok(())
    }
}

fn pattern_valid(pattern: &str, maximum: usize) -> Result<(), String> {
    let tokens: Vec<_> = pattern.split('.').collect();
    if pattern.is_empty()
        || pattern.len() > maximum
        || !pattern.bytes().all(|byte| byte.is_ascii_graphic())
        || tokens.iter().enumerate().any(|(i, token)| {
            token.is_empty()
                || (*token == ">" && i + 1 != tokens.len())
                || (*token != "*" && *token != ">" && (token.contains('*') || token.contains('>')))
        })
    {
        return Err(format!(
            "messaging: invalid subject pattern {pattern:?}; use tokens, * or terminal >"
        ));
    }
    Ok(())
}

fn optional_string(table: &Table, path: &str, key: &str) -> Result<Option<String>, String> {
    match table.raw_get::<Value>(key).map_err(|e| e.to_string())? {
        Value::Nil => Ok(None),
        value => string(value, &format!("{path}.{key}")).map(Some),
    }
}

fn array(table: &Table, path: &str, key: &str, max: usize) -> Result<Vec<Table>, String> {
    let value = table.raw_get::<Value>(key).map_err(|e| e.to_string())?;
    if value.is_nil() {
        return Ok(Vec::new());
    }
    let path = format!("{path}.{key}");
    let mut ordered = BTreeMap::new();
    for pair in super::table(value, &path)?.pairs::<Value, Value>() {
        let (index, value) = pair.map_err(|e| e.to_string())?;
        let Value::Integer(index) = index else {
            return Err(format!("{path}: expected a dense array"));
        };
        ordered.insert(index, super::table(value, &path)?);
    }
    if ordered.len() > max || ordered.keys().copied().ne(1..=ordered.len() as i64) {
        return Err(format!(
            "{path}: expected a dense array of at most {max} entries"
        ));
    }
    Ok(ordered.into_values().collect())
}

fn patterns(
    table: &Table,
    path: &str,
    key: &str,
    default: Vec<String>,
) -> Result<Vec<String>, String> {
    match table.raw_get::<Value>(key).map_err(|e| e.to_string())? {
        Value::Nil => Ok(default),
        value => string_list(value, &format!("{path}.{key}"), 1024),
    }
}

pub(super) fn parse(root: &Table) -> Result<Option<MessagingConfig>, String> {
    let Some(values) = service_options(
        root,
        "messaging",
        &[
            "listen",
            "certificate",
            "private_key",
            "principals",
            "subscription_capacity",
            "max_payload_bytes",
            "max_subject_bytes",
            "max_subscriptions",
            "max_connections",
            "max_streams_per_connection",
            "handshake_timeout_ms",
            "io_timeout_ms",
            "subscriptions",
            "streams",
        ],
    )?
    else {
        return Ok(None);
    };
    let defaults = MessagingConfig::default();
    let mut config = MessagingConfig {
        listen: optional_string(&values, "messaging", "listen")?
            .map(|value| address(&value, "messaging.listen").map_err(|e| e.to_string()))
            .transpose()?,
        certificate: optional_string(&values, "messaging", "certificate")?.map(PathBuf::from),
        private_key: optional_string(&values, "messaging", "private_key")?.map(PathBuf::from),
        subscription_capacity: integer(
            &values,
            "messaging",
            "subscription_capacity",
            defaults.subscription_capacity,
            1_000_000,
        )?,
        max_payload_bytes: integer(
            &values,
            "messaging",
            "max_payload_bytes",
            defaults.max_payload_bytes,
            16 * 1024 * 1024,
        )?,
        max_subject_bytes: integer(
            &values,
            "messaging",
            "max_subject_bytes",
            defaults.max_subject_bytes,
            1024,
        )?,
        max_subscriptions: integer(
            &values,
            "messaging",
            "max_subscriptions",
            defaults.max_subscriptions,
            1_000_000,
        )?,
        max_connections: integer(
            &values,
            "messaging",
            "max_connections",
            defaults.max_connections,
            1_000_000,
        )?,
        max_streams_per_connection: integer(
            &values,
            "messaging",
            "max_streams_per_connection",
            defaults.max_streams_per_connection,
            65535,
        )?,
        handshake_timeout: Duration::from_millis(integer(
            &values,
            "messaging",
            "handshake_timeout_ms",
            5000,
            86_400_000,
        )? as u64),
        io_timeout: Duration::from_millis(integer(
            &values,
            "messaging",
            "io_timeout_ms",
            30000,
            86_400_000,
        )? as u64),
        ..defaults
    };
    for principal in array(&values, "messaging", "principals", 1024)? {
        fields(
            &principal,
            &["name", "token_env", "publish", "subscribe", "control"],
            "messaging.principals",
        )?;
        config.principals.push(MessagingPrincipal {
            name: string(
                principal.raw_get("name").map_err(|e| e.to_string())?,
                "messaging.principals.name",
            )?,
            token_env: string(
                principal.raw_get("token_env").map_err(|e| e.to_string())?,
                "messaging.principals.token_env",
            )?,
            publish: patterns(&principal, "messaging.principals", "publish", Vec::new())?,
            subscribe: patterns(&principal, "messaging.principals", "subscribe", Vec::new())?,
            control: boolean(&principal, "messaging.principals", "control", false)?,
        });
    }
    for subscription in array(&values, "messaging", "subscriptions", 1024)? {
        fields(
            &subscription,
            &["subject", "queue"],
            "messaging.subscriptions",
        )?;
        config.subscriptions.push(MessagingSubscription {
            subject: string(
                subscription.raw_get("subject").map_err(|e| e.to_string())?,
                "messaging.subscriptions.subject",
            )?,
            queue: optional_string(&subscription, "messaging.subscriptions", "queue")?,
        });
    }
    if let Some(streams) = options_map(&values, "streams")? {
        for pair in streams.pairs::<Value, Value>() {
            let (name, value) = pair.map_err(|e| e.to_string())?;
            let name = string(name, "messaging.streams key")?;
            let path = format!("messaging.streams.{name}");
            let stream = table(value, &path)?;
            fields(
                &stream,
                &[
                    "storage_path",
                    "subjects",
                    "max_messages",
                    "max_bytes",
                    "max_payload_bytes",
                    "max_consumers",
                    "sync_on_write",
                ],
                &path,
            )?;
            let defaults = crate::messaging::StreamConfig::default();
            config.streams.insert(
                name,
                MessagingStream {
                    storage_path: optional_string(&stream, &path, "storage_path")?
                        .map(PathBuf::from),
                    config: crate::messaging::StreamConfig {
                        subjects: patterns(&stream, &path, "subjects", defaults.subjects)?,
                        max_messages: integer(
                            &stream,
                            &path,
                            "max_messages",
                            defaults.max_messages,
                            10_000_000,
                        )?,
                        max_bytes: integer(
                            &stream,
                            &path,
                            "max_bytes",
                            defaults.max_bytes,
                            16 * 1024 * 1024 * 1024usize,
                        )?,
                        max_payload_bytes: integer(
                            &stream,
                            &path,
                            "max_payload_bytes",
                            defaults.max_payload_bytes.min(config.max_payload_bytes),
                            16 * 1024 * 1024,
                        )?,
                        max_consumers: integer(
                            &stream,
                            &path,
                            "max_consumers",
                            defaults.max_consumers,
                            65536,
                        )?,
                        sync_on_write: boolean(&stream, &path, "sync_on_write", true)?,
                    },
                },
            );
        }
    }
    Ok(Some(config))
}
