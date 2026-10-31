//! Hooks answered by a bus subscriber. Rust sends the request and waits
//! asynchronously, so a slow responder never holds a Lua worker or an
//! extension slot. Replies follow the same rules as Lua results.
use super::{Action, Context, fields, invalid};
use crate::messaging::{self, Broker};
use mlua::{Table, Value};
use std::{collections::BTreeMap, io, sync::Arc, time::Duration};
use tokio::sync::Semaphore;

/// Hooks whose outcome a subscriber can decide.
const HOOKS: &[&str] = &["login", "initial_server", "before_transfer"];
const DEFAULT_TIMEOUT_MS: u64 = 1000;
const MAX_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_IN_FLIGHT: usize = 256;
const MAX_IN_FLIGHT: usize = 65_536;
const DEFAULT_DENY: &str = "Unable to check your access, please try again later.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Policy {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Decision {
    subject: String,
    timeout: Duration,
    on_timeout: Policy,
    on_no_responders: Policy,
    deny_message: String,
    pub(super) max_in_flight: usize,
}

fn error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::runtime(message.into())
}

fn policy(table: &Table, hook: &str, field: &str) -> mlua::Result<Policy> {
    match table.raw_get::<Option<String>>(field)?.as_deref() {
        None | Some("deny") => Ok(Policy::Deny),
        Some("allow") => Ok(Policy::Allow),
        Some(_) => Err(error(format!(
            "extensions.decisions.{hook}.{field} must be \"allow\" or \"deny\""
        ))),
    }
}

/// `extensions.decisions`, keyed by hook name.
pub(super) fn parse(value: Value) -> mlua::Result<BTreeMap<String, Decision>> {
    let table = match value {
        Value::Nil => return Ok(BTreeMap::new()),
        Value::Table(table) => table,
        _ => return Err(error("extensions.decisions must be a table")),
    };
    let mut decisions = BTreeMap::new();
    for pair in table.pairs::<String, Value>() {
        let (hook, value) = pair?;
        if !HOOKS.contains(&hook.as_str()) {
            return Err(error(format!(
                "extensions.decisions.{hook}: only login, initial_server and before_transfer can be decided"
            )));
        }
        let Value::Table(entry) = value else {
            return Err(error(format!(
                "extensions.decisions.{hook} must be a table"
            )));
        };
        fields(
            &entry,
            &[
                "subject",
                "timeout_ms",
                "on_timeout",
                "on_no_responders",
                "deny_message",
                "max_in_flight",
            ],
        )?;
        let subject: String = entry
            .raw_get::<Option<String>>("subject")?
            .ok_or_else(|| error(format!("extensions.decisions.{hook}.subject is required")))?;
        if messaging::validate_subject(&subject, false, 1024).is_err()
            || reserved(&subject, "rift")
            || reserved(&subject, "_INBOX")
        {
            return Err(error(format!(
                "extensions.decisions.{hook}.subject must be a literal subject outside rift.* and _INBOX.*"
            )));
        }
        let timeout = entry
            .raw_get::<Option<u64>>("timeout_ms")?
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        if !(1..=MAX_TIMEOUT_MS).contains(&timeout) {
            return Err(error(format!(
                "extensions.decisions.{hook}.timeout_ms must be 1..={MAX_TIMEOUT_MS}"
            )));
        }
        let deny_message = entry
            .raw_get::<Option<String>>("deny_message")?
            .unwrap_or_else(|| DEFAULT_DENY.into());
        if deny_message.is_empty() || deny_message.len() > 1024 {
            return Err(error(format!(
                "extensions.decisions.{hook}.deny_message must contain 1..=1024 bytes"
            )));
        }
        let max_in_flight = entry
            .raw_get::<Option<usize>>("max_in_flight")?
            .unwrap_or(DEFAULT_IN_FLIGHT);
        if !(1..=MAX_IN_FLIGHT).contains(&max_in_flight) {
            return Err(error(format!(
                "extensions.decisions.{hook}.max_in_flight must be 1..={MAX_IN_FLIGHT}"
            )));
        }
        decisions.insert(
            hook.clone(),
            Decision {
                subject,
                timeout: Duration::from_millis(timeout),
                on_timeout: policy(&entry, &hook, "on_timeout")?,
                on_no_responders: policy(&entry, &hook, "on_no_responders")?,
                deny_message,
                max_in_flight,
            },
        );
    }
    Ok(decisions)
}

fn reserved(subject: &str, prefix: &str) -> bool {
    subject
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

impl Decision {
    fn apply(&self, policy: Policy) -> Action {
        match policy {
            Policy::Allow => Action::Continue,
            Policy::Deny => Action::Deny(self.deny_message.clone()),
        }
    }

    /// Ask the subscribers on `subject`. A full in-flight window applies the
    /// timeout policy at once instead of queueing behind a login flood. A
    /// reply that arrives after the timeout is dropped with the inbox.
    pub(super) async fn decide(
        &self,
        hook: &str,
        input: &Context,
        broker: &Broker,
        in_flight: &Arc<Semaphore>,
    ) -> io::Result<Action> {
        let Ok(_permit) = in_flight.try_acquire() else {
            return Ok(self.apply(self.on_timeout));
        };
        match broker
            .request(&self.subject, request(hook, input), self.timeout)
            .await
        {
            Ok(reply) => parse_reply(hook, &reply.payload),
            Err(messaging::Error::Timeout) => Ok(self.apply(self.on_timeout)),
            Err(messaging::Error::NoResponders) => Ok(self.apply(self.on_no_responders)),
            Err(other) => Err(invalid(format!("extensions.decisions.{hook}: {other}"))),
        }
    }
}

fn request(hook: &str, input: &Context) -> bytes::Bytes {
    serde_json::json!({
        "hook": hook,
        "uuid": input.uuid,
        "name": input.name,
        "connection_id": input.id.to_string(),
        "peer_ip": input.peer_ip,
        "hostname": input.hostname,
        "protocol": input.protocol,
        "default_server": input.default_server,
        "server": input.server,
        "target": input.target,
        "reason": input.reason,
    })
    .to_string()
    .into()
}

/// `{"action":"continue"}`, `{"action":"deny","reason":…}` or, for
/// `initial_server` only, `{"action":"server","server":…}`. Anything else is
/// an error, exactly like an invalid Lua result.
fn parse_reply(hook: &str, payload: &[u8]) -> io::Result<Action> {
    let fail = |message: &str| invalid(format!("extensions.decisions.{hook}: {message}"));
    let value: serde_json::Value =
        serde_json::from_slice(payload).map_err(|_| fail("reply must be a JSON object"))?;
    let object = value
        .as_object()
        .ok_or_else(|| fail("reply must be a JSON object"))?;
    let field = match object.get("action").and_then(serde_json::Value::as_str) {
        Some("continue") => None,
        Some("deny") => Some("reason"),
        Some("server") if hook == "initial_server" => Some("server"),
        Some(_) => return Err(fail("invalid decision for this hook")),
        None => return Err(fail("reply requires a string action")),
    };
    if object.len() != 1 + usize::from(field.is_some())
        || field.is_some_and(|field| !object.contains_key(field))
    {
        return Err(fail(match field {
            None => "a continue reply has no other fields",
            Some("reason") => "a deny reply requires exactly a reason",
            Some(_) => "a server reply requires exactly a server",
        }));
    }
    let Some(field) = field else {
        return Ok(Action::Continue);
    };
    let text = object[field]
        .as_str()
        .filter(|text| !text.is_empty() && text.len() <= 1024)
        .ok_or_else(|| fail("decision string must contain 1..=1024 bytes"))?;
    Ok(if field == "reason" {
        Action::Deny(text.into())
    } else {
        Action::Server(text.into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_follow_lua_result_rules() {
        for (hook, reply, expected) in [
            ("login", r#"{"action":"continue"}"#, Action::Continue),
            (
                "login",
                r#"{"action":"deny","reason":"banned"}"#,
                Action::Deny("banned".into()),
            ),
            (
                "initial_server",
                r#"{"action":"server","server":"lobby"}"#,
                Action::Server("lobby".into()),
            ),
            (
                "before_transfer",
                r#"{"action":"deny","reason":"no"}"#,
                Action::Deny("no".into()),
            ),
        ] {
            assert_eq!(parse_reply(hook, reply.as_bytes()).unwrap(), expected);
        }
        let long = format!(r#"{{"action":"deny","reason":"{}"}}"#, "x".repeat(1025));
        for (hook, reply) in [
            ("login", r#"{"action":"server","server":"lobby"}"#),
            ("before_transfer", r#"{"action":"server","server":"lobby"}"#),
            ("login", r#"{"action":"deny"}"#),
            ("login", r#"{"action":"deny","reason":""}"#),
            ("login", r#"{"action":"deny","reason":7}"#),
            ("login", long.as_str()),
            ("login", r#"{"action":"continue","reason":"x"}"#),
            ("login", r#"{"action":"allow"}"#),
            ("login", r#"{"reason":"x"}"#),
            ("login", r#"["continue"]"#),
            ("login", "continue"),
            ("login", ""),
        ] {
            assert!(parse_reply(hook, reply.as_bytes()).is_err(), "{reply}");
        }
    }

    #[test]
    fn requests_carry_the_session_context() {
        let input = Context {
            id: 7,
            uuid: "0a".repeat(16),
            name: "Alice".into(),
            peer_ip: "203.0.113.4".into(),
            hostname: "play.example".into(),
            protocol: 772,
            default_server: Some("lobby".into()),
            ..Context::default()
        };
        let value: serde_json::Value = serde_json::from_slice(&request("login", &input)).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "hook": "login", "uuid": "0a".repeat(16), "name": "Alice",
                "connection_id": "7", "peer_ip": "203.0.113.4",
                "hostname": "play.example", "protocol": 772,
                "default_server": "lobby", "server": null, "target": null, "reason": null,
            })
        );
    }
}
