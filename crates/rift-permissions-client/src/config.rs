use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use rift_permissions::{
    CompiledSubject, Group, User, Uuid, compile, subject::normalize_group, unix_now,
};
use serde::{Deserialize, Deserializer};

use crate::Error;

/// How to reach the proxy and what to do without it. Deserialises from the
/// JSON (or any serde format) a plugin keeps in its configuration file:
///
/// ```json
/// {"address": "proxy.internal:7443", "server_name": "proxy.internal", "ca": "certs/ca.pem",
///  "token_env": "RIFT_SURVIVAL_1_TOKEN", "server": "survival-1",
///  "join_timeout_ms": 2000, "request_timeout_ms": 5000,
///  "offline_policy": {"mode": "last_snapshot", "path": "replica.json"}}
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `host:port` of the proxy's messaging listener.
    pub address: String,
    /// The name the proxy's certificate is verified against.
    pub server_name: String,
    /// PEM file with the CA or the proxy's own certificate.
    pub ca: PathBuf,
    /// Environment variable that holds the bus token of this backend's
    /// principal (conventionally `server.<name>`).
    pub token_env: String,
    /// This backend's name: sent in `hello` and filled in as the `server`
    /// context when a check does not name one.
    pub server: String,
    /// How long [`Client::join`](crate::Client::join) waits for a snapshot.
    #[serde(
        rename = "join_timeout_ms",
        default = "default_join_timeout",
        deserialize_with = "millis"
    )]
    pub join_timeout: Duration,
    /// How long other requests (`apply`, commands, groups) wait for a reply.
    #[serde(
        rename = "request_timeout_ms",
        default = "default_request_timeout",
        deserialize_with = "millis"
    )]
    pub request_timeout: Duration,
    #[serde(default)]
    pub offline_policy: OfflinePolicy,
}

impl Config {
    /// A configuration with the default timeouts (2 s to join, 5 s for
    /// requests) and the [`OfflinePolicy::Deny`] policy.
    pub fn new(
        address: impl Into<String>,
        server_name: impl Into<String>,
        ca: impl Into<PathBuf>,
        token_env: impl Into<String>,
        server: impl Into<String>,
    ) -> Self {
        Self {
            address: address.into(),
            server_name: server_name.into(),
            ca: ca.into(),
            token_env: token_env.into(),
            server: server.into(),
            join_timeout: default_join_timeout(),
            request_timeout: default_request_timeout(),
            offline_policy: OfflinePolicy::Deny,
        }
    }
}

fn default_join_timeout() -> Duration {
    Duration::from_secs(2)
}

fn default_request_timeout() -> Duration {
    Duration::from_secs(5)
}

fn millis<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    u64::deserialize(deserializer).map(Duration::from_millis)
}

/// What a player without a snapshot from the proxy gets, when the proxy is
/// unreachable or does not answer within the join timeout. Players whose
/// snapshot is already in the replica keep being served from it either way.
///
/// JSON: `{"mode": "deny"}`, `{"mode": "last_snapshot", "path": "replica.json"}`
/// or `{"mode": "fallback_file", "path": "fallback.json"}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(tag = "mode", content = "path", rename_all = "snake_case")]
pub enum OfflinePolicy {
    /// Every check is `Undefined`, so plugin defaults and op levels apply.
    #[default]
    Deny,
    /// Persist the replica to this file on [`Client::shutdown`] and load it
    /// on start, so players known before a restart keep their permissions.
    ///
    /// [`Client::shutdown`]: crate::Client::shutdown
    LastSnapshot(PathBuf),
    /// Compile players from a static file the operator wrote for
    /// emergencies: groups and users in the typed node format of
    /// `docs/permissions-protocol.md`, compiled locally exactly as the proxy
    /// would. A player the file does not list gets the `default` group. The
    /// file is read and validated on start.
    ///
    /// ```json
    /// {"groups": [{"name": "default", "nodes": [{"kind": "permission", "key": "essentials.spawn", "value": true}]},
    ///             {"name": "staff", "weight": 100, "nodes": [{"kind": "permission", "key": "essentials.*", "value": true},
    ///                                                       {"kind": "prefix", "priority": 100, "text": "&c[Staff] "}]}],
    ///  "users": [{"uuid": "0123456789abcdef0123456789abcdef", "nodes": [{"kind": "inherit", "group": "staff", "value": true}]}]}
    /// ```
    FallbackFile(PathBuf),
}

/// A parsed [`OfflinePolicy::FallbackFile`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fallback {
    groups: BTreeMap<String, Group>,
    users: BTreeMap<Uuid, User>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FallbackFile {
    #[serde(default)]
    groups: Vec<Group>,
    #[serde(default)]
    users: Vec<User>,
}

impl Fallback {
    /// Parse and validate a fallback file's JSON: names are normalised and
    /// every listed user must compile.
    pub(crate) fn parse(json: &str) -> Result<Self, Error> {
        let bad = |error: &dyn std::fmt::Display| Error::Config(format!("fallback file: {error}"));
        let file: FallbackFile = serde_json::from_str(json).map_err(|e| bad(&e))?;
        let mut groups = BTreeMap::new();
        for mut group in file.groups {
            group.name = normalize_group(&group.name).map_err(|e| bad(&e))?;
            if groups.insert(group.name.clone(), group).is_some() {
                return Err(bad(&"a group is listed twice"));
            }
        }
        let users: BTreeMap<Uuid, User> = file.users.into_iter().map(|u| (u.uuid, u)).collect();
        let fallback = Self { groups, users };
        for uuid in fallback.users.keys() {
            fallback.subject(*uuid).map_err(|e| bad(&e))?;
        }
        Ok(fallback)
    }

    /// The player's compiled subject at revision 0, so any snapshot from the
    /// proxy replaces it.
    pub(crate) fn subject(&self, uuid: Uuid) -> Result<CompiledSubject, rift_permissions::Error> {
        let user = self
            .users
            .get(&uuid)
            .cloned()
            .unwrap_or_else(|| User::new(uuid));
        compile(&user, &self.groups, unix_now(), 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_permissions::{ContextSet, Tristate};

    #[test]
    fn config_json_defaults() {
        let config: Config = serde_json::from_str(
            r#"{"address": "127.0.0.1:7443", "server_name": "localhost", "ca": "ca.pem",
                "token_env": "TOKEN", "server": "lobby"}"#,
        )
        .unwrap();
        assert_eq!(
            config,
            Config::new("127.0.0.1:7443", "localhost", "ca.pem", "TOKEN", "lobby")
        );
        let policy: OfflinePolicy =
            serde_json::from_str(r#"{"mode": "fallback_file", "path": "f.json"}"#).unwrap();
        assert_eq!(policy, OfflinePolicy::FallbackFile("f.json".into()));
        assert_eq!(
            serde_json::from_str::<OfflinePolicy>(r#"{"mode": "deny"}"#).unwrap(),
            OfflinePolicy::Deny
        );
    }

    #[test]
    fn fallback_file_compiles_like_the_proxy() {
        let fallback = Fallback::parse(
            r#"{"groups": [
                  {"name": "default", "nodes": [{"kind": "permission", "key": "spawn", "value": true}]},
                  {"name": "Staff", "weight": 100, "nodes": [
                     {"kind": "permission", "key": "essentials.*", "value": true},
                     {"kind": "prefix", "priority": 100, "text": "[Staff] "}]}],
                "users": [{"uuid": "00000000000000000000000000000001",
                           "nodes": [{"kind": "inherit", "group": "staff", "value": true}]}]}"#,
        )
        .unwrap();
        let none = ContextSet::new();
        let staff = fallback.subject(Uuid::from_u128(1)).unwrap();
        assert_eq!(staff.revision, 0);
        assert_eq!(staff.check("essentials.fly", &none, 0), Tristate::True);
        assert_eq!(staff.check("spawn", &none, 0), Tristate::True);
        assert_eq!(staff.meta(&none, 0).prefix.as_deref(), Some("[Staff] "));
        let guest = fallback.subject(Uuid::from_u128(2)).unwrap();
        assert_eq!(guest.check("essentials.fly", &none, 0), Tristate::Undefined);
        assert!(Fallback::parse(r#"{"groups": [{"name": "a b"}]}"#).is_err());
        assert!(Fallback::parse(r#"{"users": [{"uuid": "x"}]}"#).is_err());
        assert!(Fallback::parse(r#"{"extra": 1}"#).is_err());
    }
}
