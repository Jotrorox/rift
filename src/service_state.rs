//! What a backend reports about itself on `rift.server.state`: a free-form
//! state label such as `LOBBY` or `INGAME`, string properties such as the
//! map or the round, and whether it drains itself.
//!
//! A report belongs to one run of a server. Managed servers forget it each
//! time they start ([`ServiceStates::clear`] from the supervisor), so a
//! restarted game server comes back unlabelled and accepting players.
//! Removing an instance forgets it too. Unmanaged backends keep their report
//! until they change it or the proxy restarts.
use serde::Serialize;
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, PoisonError, RwLock},
};
use tokio::sync::broadcast;

/// The subject a backend publishes its report on.
pub const SUBJECT: &str = "rift.server.state";
/// Where every change is announced, including a report forgotten on restart.
pub const EVENTS: &str = "rift.events.servers";
/// Most properties one server may hold.
pub const MAX_PROPERTIES: usize = 32;
/// Longest property key, in bytes.
pub const MAX_KEY: usize = 64;
/// Longest property value, in bytes.
pub const MAX_VALUE: usize = 256;
/// Longest state label, in bytes.
pub const MAX_STATE: usize = 32;
/// Largest accepted report payload.
pub const MAX_PAYLOAD: usize = 16 * 1024;

/// Labels Rift itself reports for lifecycle or routing, which a backend may
/// not claim: a reader could not tell them from Rift's own.
const RESERVED_STATES: [&str; 7] = [
    "running", "draining", "starting", "stopping", "stopped", "failed", "unknown",
];

/// One server's current report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    /// The reported label, as sent.
    pub state: Option<String>,
    pub properties: BTreeMap<String, String>,
    /// The server asked to accept no new players.
    pub draining: bool,
}

impl Report {
    fn is_empty(&self) -> bool {
        self.state.is_none() && self.properties.is_empty() && !self.draining
    }
}

/// A change to a report. Absent fields stay as they are.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Update {
    /// The server to report for; only in-process publishers may name one.
    pub server: Option<String>,
    /// `Some(None)` clears the label.
    pub state: Option<Option<String>>,
    /// Merged into the properties; `None` removes a key.
    pub properties: BTreeMap<String, Option<String>>,
    /// Replace every property instead of merging.
    pub replace: bool,
    pub draining: Option<bool>,
}

impl Update {
    /// Parse a strict JSON object:
    /// `{"state":"INGAME","properties":{"map":"Desert","round":null},"draining":true}`.
    pub fn decode(payload: &[u8]) -> Result<Self, String> {
        if payload.len() > MAX_PAYLOAD {
            return Err(format!("report exceeds {} KiB", MAX_PAYLOAD / 1024));
        }
        let value: Value =
            serde_json::from_slice(payload).map_err(|_| "expected a JSON object".to_owned())?;
        let Value::Object(object) = value else {
            return Err("expected a JSON object".into());
        };
        let mut update = Self::default();
        for (field, value) in object {
            match (field.as_str(), value) {
                ("server", Value::String(server)) if !server.is_empty() => {
                    update.server = Some(server)
                }
                ("state", Value::Null) => update.state = Some(None),
                ("state", Value::String(state)) => update.state = Some(Some(label(state)?)),
                ("properties", Value::Object(properties)) => {
                    update.properties = entries(properties)?
                }
                ("replace", Value::Bool(replace)) => update.replace = replace,
                ("draining", Value::Bool(draining)) => update.draining = Some(draining),
                ("server" | "state" | "properties" | "replace" | "draining", _) => {
                    return Err(format!("invalid {field}"));
                }
                _ => return Err(format!("unknown field {field:?}")),
            }
        }
        Ok(update)
    }
}

fn label(state: String) -> Result<String, String> {
    let valid = (1..=MAX_STATE).contains(&state.len())
        && state
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if !valid {
        return Err(format!(
            "state must be 1..={MAX_STATE} characters of A-Z, a-z, 0-9, '_' or '-'"
        ));
    }
    if RESERVED_STATES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(&state))
    {
        return Err(format!("state {state:?} is reserved for Rift"));
    }
    Ok(state)
}

fn entries(properties: Map<String, Value>) -> Result<BTreeMap<String, Option<String>>, String> {
    if properties.len() > MAX_PROPERTIES {
        return Err(format!("at most {MAX_PROPERTIES} properties"));
    }
    properties
        .into_iter()
        .map(|(key, value)| {
            let valid_key = (1..=MAX_KEY).contains(&key.len())
                && key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
            if !valid_key {
                return Err(format!(
                    "property keys must be 1..={MAX_KEY} characters of A-Z, a-z, 0-9, '_', '-' or '.'"
                ));
            }
            let value = match value {
                Value::Null => None,
                Value::String(value)
                    if value.len() <= MAX_VALUE && !value.chars().any(char::is_control) =>
                {
                    Some(value)
                }
                _ => {
                    return Err(format!(
                        "property {key:?} must be a string of at most {MAX_VALUE} bytes without control characters, or null"
                    ));
                }
            };
            Ok((key, value))
        })
        .collect()
}

/// A change to one server's report, as announced on [`EVENTS`].
#[derive(Debug, Clone)]
pub struct Change {
    pub server: String,
    pub report: Report,
}

/// Every server's report, shared by all configuration generations.
#[derive(Debug, Clone)]
pub struct ServiceStates(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    reports: RwLock<BTreeMap<String, Report>>,
    changes: broadcast::Sender<Change>,
}

impl Default for ServiceStates {
    fn default() -> Self {
        Self(Arc::new(Inner {
            reports: RwLock::default(),
            changes: broadcast::channel(256).0,
        }))
    }
}

impl ServiceStates {
    /// `server`'s report; empty when it has reported nothing.
    pub fn get(&self, server: &str) -> Report {
        self.0
            .reports
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(server)
            .cloned()
            .unwrap_or_default()
    }

    /// Whether `server` drains itself.
    pub fn draining(&self, server: &str) -> bool {
        self.0
            .reports
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(server)
            .is_some_and(|report| report.draining)
    }

    /// Apply `update` to `server`'s report and return the result.
    pub fn apply(&self, server: &str, update: Update) -> Result<Report, String> {
        let report = {
            let mut reports = self
                .0
                .reports
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            let mut report = reports.get(server).cloned().unwrap_or_default();
            if let Some(state) = update.state {
                report.state = state;
            }
            if update.replace {
                report.properties.clear();
            }
            for (key, value) in update.properties {
                match value {
                    Some(value) => {
                        report.properties.insert(key, value);
                    }
                    None => {
                        report.properties.remove(&key);
                    }
                }
            }
            if report.properties.len() > MAX_PROPERTIES {
                return Err(format!("at most {MAX_PROPERTIES} properties"));
            }
            if let Some(draining) = update.draining {
                report.draining = draining;
            }
            if reports.get(server) == Some(&report) {
                return Ok(report);
            }
            if report.is_empty() {
                reports.remove(server);
            } else {
                reports.insert(server.to_owned(), report.clone());
            }
            report
        };
        self.announce(server, report.clone());
        Ok(report)
    }

    /// Withdraw only the server's own drain, as an operator's `drain`
    /// command does.
    pub fn undrain(&self, server: &str) {
        if self.draining(server) {
            let _ = self.apply(
                server,
                Update {
                    draining: Some(false),
                    ..Update::default()
                },
            );
        }
    }

    /// Forget `server`'s report, when it starts again or is removed.
    pub fn clear(&self, server: &str) {
        let removed = self
            .0
            .reports
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(server)
            .is_some();
        if removed {
            self.announce(server, Report::default());
        }
    }

    /// Forget the reports of servers `keep` rejects.
    pub fn retain(&self, keep: impl Fn(&str) -> bool) {
        let removed: Vec<String> = {
            let mut reports = self
                .0
                .reports
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            let removed = reports.keys().filter(|name| !keep(name)).cloned().collect();
            reports.retain(|name, _| keep(name));
            removed
        };
        for server in removed {
            self.announce(&server, Report::default());
        }
    }

    /// Every change from now on; a lagging receiver misses some.
    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.0.changes.subscribe()
    }

    fn announce(&self, server: &str, report: Report) {
        let _ = self.0.changes.send(Change {
            server: server.to_owned(),
            report,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_strictly() {
        let update = Update::decode(
            br#"{"state":"INGAME","properties":{"map":"Desert","round":null},"draining":true}"#,
        )
        .unwrap();
        assert_eq!(update.state, Some(Some("INGAME".into())));
        assert_eq!(update.properties["map"].as_deref(), Some("Desert"));
        assert_eq!(update.properties["round"], None);
        assert_eq!(update.draining, Some(true));
        assert_eq!(
            Update::decode(br#"{"state":null}"#).unwrap().state,
            Some(None)
        );
        for bad in [
            &br#"[]"#[..],
            br#"{"state":"in game"}"#,
            br#"{"state":"Running"}"#,
            br#"{"state":""}"#,
            br#"{"state":7}"#,
            br#"{"draining":"yes"}"#,
            br#"{"properties":{"":"x"}}"#,
            br#"{"properties":{"map":1}}"#,
            br#"{"properties":{"map":"a\nb"}}"#,
            br#"{"server":""}"#,
            br#"{"extra":1}"#,
        ] {
            assert!(
                Update::decode(bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        let long = format!(
            r#"{{"properties":{{"map":"{}"}}}}"#,
            "x".repeat(MAX_VALUE + 1)
        );
        assert!(Update::decode(long.as_bytes()).is_err());
    }

    #[test]
    fn merges_replaces_clears_and_announces() {
        let states = ServiceStates::default();
        let mut changes = states.subscribe();
        let report = states
            .apply(
                "game-1",
                Update::decode(br#"{"state":"LOBBY","properties":{"map":"Desert","mode":"solo"}}"#)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(report.state.as_deref(), Some("LOBBY"));
        assert_eq!(changes.try_recv().unwrap().report, report);
        let report = states
            .apply(
                "game-1",
                Update::decode(br#"{"state":"INGAME","properties":{"mode":null},"draining":true}"#)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(report.properties.len(), 1);
        assert!(states.draining("game-1"));
        // An unchanged report announces nothing.
        changes.try_recv().unwrap();
        states
            .apply("game-1", Update::decode(br#"{"draining":true}"#).unwrap())
            .unwrap();
        assert!(changes.try_recv().is_err());
        states.undrain("game-1");
        assert!(!states.draining("game-1"));
        assert_eq!(states.get("game-1").state.as_deref(), Some("INGAME"));
        let report = states
            .apply(
                "game-1",
                Update::decode(br#"{"properties":{"round":"3"},"replace":true}"#).unwrap(),
            )
            .unwrap();
        assert_eq!(report.properties.keys().collect::<Vec<_>>(), vec!["round"]);
        changes.try_recv().unwrap();
        changes.try_recv().unwrap();
        states.clear("game-1");
        assert_eq!(changes.try_recv().unwrap().report, Report::default());
        assert_eq!(states.get("game-1"), Report::default());
        // Clearing nothing announces nothing.
        states.clear("game-1");
        assert!(changes.try_recv().is_err());
    }

    #[test]
    fn caps_merged_properties() {
        let states = ServiceStates::default();
        for batch in 0..2 {
            let properties: Map<String, Value> = (0..MAX_PROPERTIES / 2 + 1)
                .map(|i| (format!("k{batch}-{i}"), Value::String("v".into())))
                .collect();
            let payload = serde_json::json!({ "properties": properties }).to_string();
            let result = states.apply("s", Update::decode(payload.as_bytes()).unwrap());
            assert_eq!(result.is_ok(), batch == 0);
        }
        assert_eq!(states.get("s").properties.len(), MAX_PROPERTIES / 2 + 1);
    }
}
