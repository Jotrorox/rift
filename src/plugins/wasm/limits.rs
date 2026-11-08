//! Bounds on one WebAssembly instance.
//!
//! Memory: `[wasm] memory_limit_mb` caps each instance's linear memories
//! through [`StoreLimits`]; a `memory.grow` past it fails inside the guest.
//!
//! Time: one process-wide thread advances the engine's epoch every
//! [`TICK`]. Every store checks at each tick whether Rift asked the plugin to
//! stop ([`Services::stop_requested`]) or the current call ran past its
//! deadline, and traps the guest if so. A deadline trap is fatal: the guest
//! may have been interrupted halfway through changing its own state, so the
//! worker discards the instance and restarts it per the manifest. Guests are
//! only interrupted while they run WebAssembly; a blocking host call (a bus
//! request, say) finishes first.
use super::State;
use crate::plugins::manifest::Manifest;
use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};
use wasmtime::{Engine, Store, StoreLimits, StoreLimitsBuilder, UpdateDeadline};

/// How often the epoch advances: the granularity of deadlines and stops.
pub(super) const TICK: Duration = Duration::from_millis(10);

pub(super) fn store_limits(manifest: &Manifest) -> StoreLimits {
    let megabytes = manifest
        .wasm
        .as_ref()
        .map_or(0, |wasm| wasm.memory_limit_mb);
    let mut limits = StoreLimitsBuilder::new();
    if megabytes > 0 {
        limits = limits.memory_size(megabytes as usize * 1024 * 1024);
    }
    limits.build()
}

/// Start the epoch thread once per process.
pub(super) fn start_ticker(engine: &Engine) {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let engine = engine.clone();
        std::thread::Builder::new()
            .name("rift-wasm-epoch".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(TICK);
                    engine.increment_epoch();
                }
            })
            .expect("cannot start the WebAssembly epoch thread");
    });
}

/// The running call's deadline, and why the last call was interrupted.
#[derive(Default)]
pub(super) struct Watch {
    deadline: Option<(Instant, Duration, &'static str)>,
    interrupted: Option<String>,
}

impl Watch {
    /// Why the guest was interrupted, once.
    pub(super) fn take_interrupted(&mut self) -> Option<String> {
        self.interrupted.take()
    }
}

/// Install the epoch check on a new store.
pub(super) fn watch(store: &mut Store<State>) {
    store.epoch_deadline_callback(|mut hook| {
        let state = hook.data_mut();
        let reason = if state.services.stop_requested() {
            Some("interrupted: Rift stopped the plugin".to_owned())
        } else {
            match state.watch.deadline {
                Some((until, limit, what)) if Instant::now() >= until => Some(format!(
                    "interrupted: {what} exceeded its {} ms deadline",
                    limit.as_millis()
                )),
                _ => None,
            }
        };
        match reason {
            Some(reason) => {
                state.watch.interrupted = Some(reason);
                Ok(UpdateDeadline::Interrupt)
            }
            None => Ok(UpdateDeadline::Continue(1)),
        }
    });
    store.set_epoch_deadline(1);
}

/// Bound the next guest call: `None` only checks for a stop request.
pub(super) fn arm(store: &mut Store<State>, what: &'static str, limit: Option<Duration>) {
    let watch = &mut store.data_mut().watch;
    watch.interrupted = None;
    watch.deadline = limit.map(|limit| (Instant::now() + limit, limit, what));
    store.set_epoch_deadline(1);
}
