//! `conformance-rs`: the WebAssembly build of the conformance guest that the
//! engine-parameterised suite drives (`src/plugins/conformance.rs` documents
//! the configuration keys; `src/plugins/lua/conformance.lua` is the
//! reference implementation).
//!
//! Two behaviours differ in mechanism, not in what the host observes:
//! - `fail = "fatal"` panics, which traps the instance.
//! - `fail = "handler"`: hooks have no error result in `rift:plugin`, so the
//!   guest answers with a decision the hook does not accept, carrying the
//!   text `database down`. The host rejects it as an invalid reply, applies
//!   `on_failure` and keeps the instance running, as for a Lua error.
use rift_plugin::{
    Action, BusMessage, CommandSpec, Connection, Context, Hook, HttpRequest, HttpResponse, Ping,
    Plugin, Registration, RouteDecision, StatusOverride, SuggestKind, Suggestion, bus, meta,
    permissions, store, timers,
};
use serde_json::Value;

#[derive(Default)]
struct Guest {
    config: Value,
}

fn report(text: &str) {
    let _ = bus::publish(&format!("conformance.{}", meta::name()), text.as_bytes(), None);
}

fn tag(config: &Value) -> String {
    match &config["tag"] {
        Value::Null => "-".into(),
        Value::String(tag) => tag.clone(),
        other => other.to_string(),
    }
}

fn parse(config: &str) -> Result<Value, String> {
    serde_json::from_str(config).map_err(|error| format!("invalid configuration: {error}"))
}

impl Guest {
    /// Report the hook, then wait or fail as configured. `Err` is the
    /// "handler" failure for hooks that can express it.
    fn behave(&self, hook: &str) -> Result<(), ()> {
        report(hook);
        let slow = self.config["slow"]
            .as_array()
            .is_some_and(|hooks| hooks.iter().any(|entry| entry == hook));
        if slow {
            let started = meta::clock();
            while meta::clock() - started < 0.3 {}
        }
        match self.config["fail"][hook].as_str() {
            Some("handler") => Err(()),
            Some("fatal") => panic!("conformance guest: fatal {hook}"),
            _ => Ok(()),
        }
    }

    fn decide(&self, hook: &str) -> Action {
        if self.behave(hook).is_err() {
            // No hook accepts `message` except `command`.
            return Action::Message("database down".into());
        }
        match &self.config["decide"][hook] {
            Value::Object(action) => match action.iter().next() {
                Some((kind, Value::String(text))) if kind == "deny" => Action::Deny(text.clone()),
                Some((kind, Value::String(text))) if kind == "server" => {
                    Action::Server(text.clone())
                }
                Some((kind, Value::String(text))) if kind == "message" => {
                    Action::Message(text.clone())
                }
                Some((kind, Value::String(text))) if kind == "queue" => Action::Queue(text.clone()),
                _ => Action::Continue,
            },
            Value::String(kind) if kind == "leave_queue" => Action::LeaveQueue,
            _ => Action::Continue,
        }
    }

    fn observe(&self, hook: &str) {
        let _ = self.behave(hook);
    }
}

impl Plugin for Guest {
    fn init(&mut self, config: &str) -> Result<Registration, String> {
        self.config = parse(config)?;
        report(&format!("init {}", tag(&self.config)));
        let mut registration = Registration::new().hooks([
            Hook::Route,
            Hook::Status,
            Hook::Http,
            Hook::Login,
            Hook::InitialServer,
            Hook::BeforeTransfer,
            Hook::Join,
            Hook::AfterTransfer,
            Hook::Disconnect,
        ]);
        let mut dynamic = false;
        for command in self.config["commands"].as_array().into_iter().flatten() {
            let suggest = match &command["suggest"] {
                Value::String(kind) if kind == "players" => SuggestKind::Players,
                Value::String(kind) if kind == "servers" => SuggestKind::Servers,
                Value::String(kind) if kind == "dynamic" => {
                    dynamic = true;
                    SuggestKind::Dynamic
                }
                Value::Array(entries) => SuggestKind::Fixed(
                    entries
                        .iter()
                        .filter_map(|entry| entry.as_str().map(str::to_owned))
                        .collect(),
                ),
                _ => SuggestKind::None,
            };
            registration = registration.command(
                CommandSpec::new(command["name"].as_str().unwrap_or_default())
                    .permission(command["permission"].as_str().unwrap_or_default())
                    .suggest(suggest),
            );
        }
        if !registration.commands.is_empty() {
            registration = registration.hook(Hook::Command);
        }
        if dynamic {
            registration = registration.hook(Hook::Suggest);
        }
        for grant in self.config["grant"].as_array().into_iter().flatten() {
            if let (Some(uuid), Some(node)) = (grant[0].as_str(), grant[1].as_str()) {
                permissions::set(uuid, node, Some(true))?;
            }
        }
        if let Some(delta) = self.config["increment"].as_i64() {
            store::increment("counters", "hits", delta)?;
        }
        if let Some(ms) = self.config["timer_ms"].as_u64() {
            timers::every(ms as u32);
            registration = registration.hook(Hook::Timer);
        }
        if let Some(subject) = self.config["subscribe"].as_str() {
            bus::subscribe(subject, None)?;
            registration = registration.hook(Hook::Message);
        }
        Ok(registration)
    }

    fn reload(&mut self, config: &str) -> Result<(), String> {
        self.config = parse(config)?;
        report(&format!("reload {}", tag(&self.config)));
        Ok(())
    }

    fn shutdown(&mut self, reason: &str) {
        report(&format!("shutdown {reason}"));
    }

    fn route(&mut self, connection: Connection) -> RouteDecision {
        self.observe("route");
        let reject = &self.config["route_reject"];
        if reject["ip"].as_str() == Some(connection.peer_ip.as_str()) {
            return RouteDecision::Reject(reject["reason"].as_str().map(str::to_owned));
        }
        RouteDecision::Default
    }

    fn login(&mut self, _ctx: Context) -> Action {
        self.decide("login")
    }

    fn initial_server(&mut self, _ctx: Context) -> Action {
        self.decide("initial_server")
    }

    fn before_transfer(&mut self, _ctx: Context) -> Action {
        self.decide("before_transfer")
    }

    fn command(&mut self, ctx: Context) -> Action {
        if self.behave("command").is_err() {
            // `command` accepts every decision; an empty message is invalid.
            return Action::Message(String::new());
        }
        Action::Message(format!("hi {}", ctx.args))
    }

    fn suggest(&mut self, _ctx: Context) -> Vec<Suggestion> {
        ["alpha", "beta", "gamma"]
            .into_iter()
            .map(|text| Suggestion {
                text: text.into(),
                tooltip: None,
            })
            .collect()
    }

    fn status(&mut self, _ping: Ping) -> Option<StatusOverride> {
        self.observe("status");
        let motd = self.config["status_motd"].as_str()?;
        Some(StatusOverride {
            motd_json: Some(Value::from(motd).to_string()),
            version: None,
            protocol: None,
            online: None,
            max_players: None,
            sample: None,
            favicon: None,
        })
    }

    fn http(&mut self, request: HttpRequest) -> Option<HttpResponse> {
        self.observe("http");
        let http = &self.config["http"];
        (http["path"].as_str() == Some(request.path.as_str())).then(|| HttpResponse {
            status: 200,
            content_type: "text/plain".into(),
            body: http["body"].as_str().unwrap_or_default().into(),
        })
    }

    fn join(&mut self, _ctx: Context) {
        self.observe("join");
    }

    fn after_transfer(&mut self, _ctx: Context) {
        self.observe("after_transfer");
    }

    fn disconnect(&mut self, _ctx: Context) {
        self.observe("disconnect");
    }

    fn timer(&mut self, handle: u64) {
        timers::cancel(handle);
        let hits = store::get("counters", "hits")
            .ok()
            .flatten()
            .map(|value| String::from_utf8_lossy(&value).into_owned())
            .unwrap_or_default();
        report(&format!("timer {hits}"));
    }

    fn message(&mut self, message: BusMessage) {
        report(&format!(
            "message {}",
            String::from_utf8_lossy(&message.payload)
        ));
    }
}

rift_plugin::export!(Guest);
