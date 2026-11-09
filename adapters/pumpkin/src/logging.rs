//! Route the client crate's `log` records into Pumpkin's log output.
//!
//! A native plugin links its own copies of `log` and `tracing`, whose global
//! loggers start empty. `Context::init_log` points this copy of `tracing` at
//! the server's console; this bridge forwards `log` records to it.

use log::{Level, LevelFilter, Log, Metadata, Record};

struct Bridge;

static BRIDGE: Bridge = Bridge;

impl Log for Bridge {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = record.args();
        match record.level() {
            Level::Error => tracing::error!(target: "rift_permissions", "{message}"),
            Level::Warn => tracing::warn!(target: "rift_permissions", "{message}"),
            Level::Info => tracing::info!(target: "rift_permissions", "{message}"),
            Level::Debug => tracing::debug!(target: "rift_permissions", "{message}"),
            Level::Trace => tracing::trace!(target: "rift_permissions", "{message}"),
        }
    }

    fn flush(&self) {}
}

/// Install the bridge, keeping records up to `level`. A no-op when a logger
/// is already installed in this copy of `log`.
pub fn install(level: LevelFilter) {
    if log::set_logger(&BRIDGE).is_ok() {
        log::set_max_level(level);
    }
}
