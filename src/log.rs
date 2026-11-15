//! Levelled diagnostics on stderr.
//!
//! Every line is `rift: <text>`; warnings and errors say so after the prefix
//! (`rift: warning: ...`, `rift: error: ...`), informational lines carry no
//! tag. `RIFT_LOG_LEVEL` picks the least severe level that is still written
//! and `RIFT_LOG_FORMAT=journal` puts the priority prefix journald reads in
//! front of each line. Writing never panics: see the `eprintln!` shadow in
//! lib.rs for why a diagnostic may not.
//!
//! Output that answers a command (`rift check`, `rift admin`, a fatal startup
//! error) is not logging and does not pass through here.

use std::{
    backtrace::{Backtrace, BacktraceStatus},
    env, fmt,
    io::{self, Write as _},
    panic,
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
    thread,
};

/// Least severe level written: `error`, `warn`, `info` (default) or `debug`.
pub const LEVEL_ENV: &str = "RIFT_LOG_LEVEL";
/// `plain` (default) or `journal`.
pub const FORMAT_ENV: &str = "RIFT_LOG_FORMAT";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    /// Work was lost or a guarantee no longer holds; someone should look.
    Error = 0,
    /// Something was refused, dropped or retried, and Rift carried on.
    Warn = 1,
    /// Lifecycle: listeners, reloads, servers and plugins coming and going.
    Info = 2,
    /// Detail for tracing one problem; off unless asked for.
    Debug = 3,
}

impl Level {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "error" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            _ => None,
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Self::Error => "error: ",
            Self::Warn => "warning: ",
            Self::Info => "",
            Self::Debug => "debug: ",
        }
    }

    /// The `<N>` syslog priority systemd strips from a line and stores as
    /// its `PRIORITY`.
    fn journal(self) -> &'static str {
        match self {
            Self::Error => "<3>",
            Self::Warn => "<4>",
            Self::Info => "<6>",
            Self::Debug => "<7>",
        }
    }
}

static THRESHOLD: AtomicU8 = AtomicU8::new(Level::Info as u8);
static JOURNAL: AtomicBool = AtomicBool::new(false);

/// Apply [`LEVEL_ENV`] and [`FORMAT_ENV`]. An unknown value is an error
/// rather than a silent default: a typo should not hide the lines an
/// operator asked for.
pub fn init() -> Result<(), String> {
    if let Some(value) = variable(LEVEL_ENV)? {
        let level = Level::parse(&value)
            .ok_or_else(|| format!("{LEVEL_ENV} must be error, warn, info or debug"))?;
        set_level(level);
    }
    if let Some(value) = variable(FORMAT_ENV)? {
        match value.as_str() {
            "plain" => JOURNAL.store(false, Ordering::Relaxed),
            "journal" => JOURNAL.store(true, Ordering::Relaxed),
            _ => return Err(format!("{FORMAT_ENV} must be plain or journal")),
        }
    }
    Ok(())
}

/// A set, non-empty variable in lower case.
fn variable(name: &str) -> Result<Option<String>, String> {
    match env::var(name) {
        Ok(value) if value.is_empty() => Ok(None),
        Ok(value) => Ok(Some(value.to_ascii_lowercase())),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(format!("{name} is not valid UTF-8")),
    }
}

pub fn set_level(level: Level) {
    THRESHOLD.store(level as u8, Ordering::Relaxed);
}

pub fn enabled(level: Level) -> bool {
    level as u8 <= THRESHOLD.load(Ordering::Relaxed)
}

/// One `rift: ` line with the level's tag. Use the `log_*!` macros.
pub fn write(level: Level, message: fmt::Arguments<'_>) {
    if enabled(level) {
        line(level, format_args!("rift: {}{message}", level.tag()));
    }
}

/// A line that already says where it comes from and how severe it is, such
/// as a plugin's own log call: filtered by `level`, written as given.
pub fn raw(level: Level, message: fmt::Arguments<'_>) {
    if enabled(level) {
        line(level, message);
    }
}

fn line(level: Level, message: fmt::Arguments<'_>) {
    let prefix = if JOURNAL.load(Ordering::Relaxed) {
        level.journal()
    } else {
        ""
    };
    let _ = writeln!(io::stderr().lock(), "{prefix}{message}");
}

/// Report panics as error lines in the log's own format.
///
/// A panic on a task or worker thread ends only that task; without this the
/// one trace of it is Rust's multi-line default message, which carries no
/// level and which journald files as ordinary output. The process keeps
/// running either way: the control loop still has connections to drain and
/// managed servers to supervise. With `RUST_BACKTRACE` set, the backtrace
/// follows line by line.
pub fn install_panic_hook() {
    panic::set_hook(Box::new(|info| {
        let thread = thread::current();
        let name = thread.name().unwrap_or("<unnamed>");
        let payload = info.payload();
        let reason = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("no message");
        // One line per panic, whatever the message contains.
        let reason: String = reason
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        match info.location() {
            Some(location) => write(
                Level::Error,
                format_args!(
                    "panic in thread {name} at {}:{}: {reason}",
                    location.file(),
                    location.line()
                ),
            ),
            None => write(
                Level::Error,
                format_args!("panic in thread {name}: {reason}"),
            ),
        }
        let backtrace = Backtrace::capture();
        if backtrace.status() == BacktraceStatus::Captured {
            for frame in backtrace.to_string().lines() {
                raw(
                    Level::Error,
                    format_args!("rift: error:   {}", frame.trim_end()),
                );
            }
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_parse_and_order_by_severity() {
        assert_eq!(Level::parse("error"), Some(Level::Error));
        assert_eq!(Level::parse("warn"), Some(Level::Warn));
        assert_eq!(Level::parse("warning"), Some(Level::Warn));
        assert_eq!(Level::parse("info"), Some(Level::Info));
        assert_eq!(Level::parse("debug"), Some(Level::Debug));
        assert_eq!(Level::parse("trace"), None);
        assert_eq!(Level::parse(""), None);
        assert!(Level::Error < Level::Warn && Level::Warn < Level::Info);
        assert!(Level::Info < Level::Debug);
    }

    #[test]
    fn only_warnings_and_errors_are_tagged() {
        assert_eq!(Level::Error.tag(), "error: ");
        assert_eq!(Level::Warn.tag(), "warning: ");
        assert_eq!(Level::Info.tag(), "");
        assert_eq!(Level::Debug.tag(), "debug: ");
    }
}
