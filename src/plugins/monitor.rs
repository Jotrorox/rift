//! What operators see of persistent plugins: a bounded log tail, call
//! counters and latency histograms per hook, restart counts, and lifecycle
//! notifications for the audit log.
//!
//! One [`Monitor`] lives as long as the process and is shared by every
//! reload generation and every instance of a plugin, so counters and the log
//! tail survive restarts and replacements. Entries exist only for configured
//! plugins: [`Monitor::retain`] drops the rest at each activation, which keeps
//! every label set bounded by the configuration (hooks are a fixed set).
//!
//! Lifecycle notifications travel on an in-process channel rather than the
//! bus: any bus client allowed to publish could forge a `rift.events.plugins`
//! message, but nothing outside the host can reach this channel.
use super::api::{Hook, Level};
use serde::Serialize;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::broadcast;

/// Log lines kept per plugin.
pub const LOG_LINES: usize = 200;
/// Upper bucket bounds for `rift_plugin_call_seconds`, in seconds. Default
/// deadlines range from 50 ms to 5 s.
pub const CALL_BUCKETS: [f64; 14] = [
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];
/// Characters kept per log line.
const MAX_LINE: usize = 4096;
/// Undelivered lifecycle notifications per subscriber.
const LIFECYCLE_BACKLOG: usize = 256;

/// How one call to a plugin ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// An awaited hook answered with a valid reply.
    Ok,
    /// An awaited hook failed, the plugin was unavailable or the reply was
    /// invalid: `on_failure` applied.
    Error,
    /// An awaited hook missed its deadline or the queue was full:
    /// `on_timeout` applied.
    Timeout,
    /// An observer, timer tick or bus message entered the plugin's queue.
    Queued,
    /// An observer, timer tick or bus message was dropped: the queue was
    /// full or the plugin was unavailable.
    Dropped,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Timeout => "timeout",
            Self::Queued => "queued",
            Self::Dropped => "dropped",
        }
    }
}

/// One line of a plugin's log tail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogLine {
    pub timestamp_unix_ms: u64,
    pub level: Level,
    pub message: String,
}

/// A change in a plugin's lifecycle: `started`, `failed`, `restarting`
/// (an automatic restart after a failure), `stopped`, `restart_requested`
/// (an operator restart), `reloaded` or `reload_failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lifecycle {
    pub event: String,
    pub plugin: String,
    /// Who asked for it; `None` when Rift itself acted.
    pub actor: Option<String>,
}

/// Cumulative latency of one hook.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Histogram {
    /// Cumulative counts per [`CALL_BUCKETS`] bound.
    pub buckets: [u64; CALL_BUCKETS.len()],
    pub count: u64,
    pub seconds: f64,
}

impl Histogram {
    fn observe(&mut self, elapsed: Duration) {
        let seconds = elapsed.as_secs_f64();
        self.count += 1;
        self.seconds += seconds;
        for (bucket, bound) in self.buckets.iter_mut().zip(CALL_BUCKETS) {
            if seconds <= bound {
                *bucket += 1;
            }
        }
    }
}

#[derive(Default)]
struct Entry {
    calls: BTreeMap<(Hook, Outcome), u64>,
    latency: BTreeMap<Hook, Histogram>,
    restarts: u64,
    log: VecDeque<LogLine>,
}

/// Counters for one plugin, for the metrics endpoint.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Counters {
    pub calls: BTreeMap<(Hook, Outcome), u64>,
    pub latency: BTreeMap<Hook, Histogram>,
    pub restarts: u64,
}

impl Counters {
    pub fn total_calls(&self) -> u64 {
        self.calls.values().sum()
    }
}

pub struct Monitor {
    plugins: Mutex<BTreeMap<String, Entry>>,
    lifecycle: broadcast::Sender<Lifecycle>,
}

impl Default for Monitor {
    fn default() -> Self {
        Self {
            plugins: Mutex::default(),
            lifecycle: broadcast::channel(LIFECYCLE_BACKLOG).0,
        }
    }
}

impl std::fmt::Debug for Monitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set()
            .entries(self.plugins.lock().unwrap().keys())
            .finish()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

impl Monitor {
    /// Keep entries for `names` (creating missing ones) and drop the rest.
    pub fn retain<'a>(&self, names: impl IntoIterator<Item = &'a str>) {
        let names: Vec<&str> = names.into_iter().collect();
        let mut plugins = self.plugins.lock().unwrap();
        plugins.retain(|name, _| names.contains(&name.as_str()));
        for name in names {
            plugins.entry(name.to_owned()).or_default();
        }
    }

    /// Create entries for `names` that have none. A plugin's worker thread
    /// may log from `init` as soon as it starts, so its entry has to exist
    /// before then or those lines are lost.
    pub fn track<'a>(&self, names: impl IntoIterator<Item = &'a str>) {
        let mut plugins = self.plugins.lock().unwrap();
        for name in names {
            plugins.entry(name.to_owned()).or_default();
        }
    }

    /// Count one call; `elapsed` is the time an awaited hook took.
    pub fn call(&self, plugin: &str, hook: Hook, outcome: Outcome, elapsed: Option<Duration>) {
        let mut plugins = self.plugins.lock().unwrap();
        let Some(entry) = plugins.get_mut(plugin) else {
            return;
        };
        *entry.calls.entry((hook, outcome)).or_default() += 1;
        if let Some(elapsed) = elapsed {
            entry.latency.entry(hook).or_default().observe(elapsed);
        }
    }

    /// Append a line to the plugin's log tail. Control characters become
    /// spaces so a line cannot pose as several.
    pub fn log(&self, plugin: &str, level: Level, message: String) {
        let message = if message.len() > MAX_LINE || message.chars().any(char::is_control) {
            message
                .chars()
                .take(MAX_LINE)
                .map(|c| if c.is_control() && c != '\t' { ' ' } else { c })
                .collect()
        } else {
            message
        };
        let mut plugins = self.plugins.lock().unwrap();
        let Some(entry) = plugins.get_mut(plugin) else {
            return;
        };
        if entry.log.len() >= LOG_LINES {
            entry.log.pop_front();
        }
        entry.log.push_back(LogLine {
            timestamp_unix_ms: now_ms(),
            level,
            message,
        });
    }

    /// Record a lifecycle change: a log line, the restart count, and a
    /// notification for [`Monitor::subscribe`]rs.
    pub fn lifecycle(&self, plugin: &str, event: &str, actor: Option<&str>, error: Option<&str>) {
        let level = match event {
            "failed" | "reload_failed" => Level::Error,
            "restarting" | "restart_requested" => Level::Warn,
            _ => Level::Info,
        };
        let mut message = format!("lifecycle: {event}");
        if let Some(actor) = actor {
            message.push_str(&format!(" by {actor}"));
        }
        if let Some(error) = error {
            message.push_str(": ");
            message.extend(error.chars().take(2048));
        }
        if matches!(event, "restarting" | "restart_requested")
            && let Some(entry) = self.plugins.lock().unwrap().get_mut(plugin)
        {
            entry.restarts += 1;
        }
        self.log(plugin, level, message);
        // Nobody listening (tests, `rift check`) is fine.
        let _ = self.lifecycle.send(Lifecycle {
            event: event.to_owned(),
            plugin: plugin.to_owned(),
            actor: actor.map(str::to_owned),
        });
    }

    /// Lifecycle notifications from now on. A subscriber that falls more
    /// than 256 notifications behind loses the oldest ones.
    pub fn subscribe(&self) -> broadcast::Receiver<Lifecycle> {
        self.lifecycle.subscribe()
    }

    /// The plugin's retained log lines, oldest first.
    pub fn logs(&self, plugin: &str) -> Option<Vec<LogLine>> {
        let plugins = self.plugins.lock().unwrap();
        plugins
            .get(plugin)
            .map(|entry| entry.log.iter().cloned().collect())
    }

    pub fn counters(&self, plugin: &str) -> Counters {
        self.plugins
            .lock()
            .unwrap()
            .get(plugin)
            .map(|entry| Counters {
                calls: entry.calls.clone(),
                latency: entry.latency.clone(),
                restarts: entry.restarts,
            })
            .unwrap_or_default()
    }

    /// Counters for every configured plugin, by name.
    pub fn all_counters(&self) -> BTreeMap<String, Counters> {
        self.plugins
            .lock()
            .unwrap()
            .iter()
            .map(|(name, entry)| {
                (
                    name.clone(),
                    Counters {
                        calls: entry.calls.clone(),
                        latency: entry.latency.clone(),
                        restarts: entry.restarts,
                    },
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_tail_is_bounded_and_entries_follow_the_configuration() {
        let monitor = Monitor::default();
        monitor.log("ghost", Level::Info, "ignored".into());
        assert_eq!(monitor.logs("ghost"), None);
        monitor.retain(["a", "b"]);
        for index in 0..LOG_LINES + 5 {
            monitor.log("a", Level::Info, format!("line {index}"));
        }
        let lines = monitor.logs("a").unwrap();
        assert_eq!(lines.len(), LOG_LINES);
        assert_eq!(lines[0].message, "line 5");
        assert_eq!(
            lines.last().unwrap().message,
            format!("line {}", LOG_LINES + 4)
        );
        monitor.retain(["b"]);
        assert_eq!(monitor.logs("a"), None);
        assert_eq!(monitor.logs("b"), Some(Vec::new()));
        // Tracking adds entries without touching existing ones.
        monitor.log("b", Level::Info, "kept".into());
        monitor.track(["b", "c"]);
        assert_eq!(monitor.logs("b").unwrap().len(), 1);
        assert_eq!(monitor.logs("c"), Some(Vec::new()));
    }

    #[test]
    fn calls_histograms_and_restarts_accumulate() {
        let monitor = Monitor::default();
        monitor.retain(["a"]);
        let mut events = monitor.subscribe();
        monitor.call(
            "a",
            Hook::Login,
            Outcome::Ok,
            Some(Duration::from_millis(3)),
        );
        monitor.call(
            "a",
            Hook::Login,
            Outcome::Timeout,
            Some(Duration::from_secs(1)),
        );
        monitor.call("a", Hook::Join, Outcome::Queued, None);
        monitor.lifecycle("a", "failed", None, Some("trap\nforged line"));
        monitor.lifecycle("a", "restarting", None, None);
        monitor.lifecycle("a", "restart_requested", Some("alice"), None);
        let counters = monitor.counters("a");
        assert_eq!(counters.total_calls(), 3);
        assert_eq!(counters.restarts, 2);
        assert_eq!(counters.calls[&(Hook::Login, Outcome::Timeout)], 1);
        let login = &counters.latency[&Hook::Login];
        assert_eq!(login.count, 2);
        assert_eq!(login.buckets[2], 0); // 2.5 ms
        assert_eq!(login.buckets[3], 1); // 5 ms
        assert_eq!(login.buckets[10], 2); // 1 s
        assert!(!counters.latency.contains_key(&Hook::Join));
        let lines = monitor.logs("a").unwrap();
        assert_eq!(lines[0].message, "lifecycle: failed: trap forged line");
        assert_eq!(lines[0].level, Level::Error);
        assert_eq!(lines[2].message, "lifecycle: restart_requested by alice");
        let first = events.try_recv().unwrap();
        assert_eq!(
            (first.event.as_str(), first.plugin.as_str()),
            ("failed", "a")
        );
        events.try_recv().unwrap();
        assert_eq!(events.try_recv().unwrap().actor.as_deref(), Some("alice"));
    }
}
