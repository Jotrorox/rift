//! C ABI (`librift_perms`) over the Rift permissions client. JSON at the edges
//! keeps the ABI stable while [`rift_permissions`] types evolve. The header is
//! `include/rift_perms.h`, generated with `cbindgen`; `docs/permissions-clients.md`
//! documents every function.
//!
//! Every function accepts null pointers and invalid input without crashing,
//! and no panic crosses the boundary. A handle owns a tokio runtime; the
//! blocking calls (`join`, `apply`) must not be made from a subscription
//! callback, which runs on that runtime.

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    sync::{Mutex, PoisonError},
    time::Duration,
};

use rift_permissions::{Change, ContextSet, Tristate, Uuid};
use rift_permissions_client::{Client, Config, Error, Listener, RiftPermissions};
use serde_json::json;

const TRUE: c_int = 1;
const FALSE: c_int = 0;
const UNDEFINED: c_int = -1;
const UNAVAILABLE: c_int = -2;
const APPLIED: c_int = 0;
const FAILED: c_int = -1;
const INVALID: c_int = -2;

/// A connection to the proxy with its replica. Opaque to C.
#[allow(non_camel_case_types)]
pub struct rift_perms {
    client: Client,
    listeners: Mutex<Vec<Listener>>,
    runtime: tokio::runtime::Runtime,
}

/// Run `body`, turning a panic into `fallback`.
fn guard<T>(fallback: T, body: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or(fallback)
}

/// # Safety
/// `text` is null or a NUL-terminated string.
unsafe fn text<'a>(text: *const c_char) -> Option<&'a str> {
    if text.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(text) }.to_str().ok()
}

/// # Safety
/// `uuid` is null or points to 16 readable bytes.
unsafe fn uuid(uuid: *const u8) -> Option<Uuid> {
    if uuid.is_null() {
        return None;
    }
    Some(Uuid::from_bytes(unsafe {
        ptr::read_unaligned(uuid.cast())
    }))
}

/// A context set from JSON; null or `""` is the empty set.
///
/// # Safety
/// `json` is null or a NUL-terminated string.
unsafe fn contexts(json: *const c_char) -> Option<ContextSet> {
    if json.is_null() {
        return Some(ContextSet::new());
    }
    match unsafe { text(json) }? {
        "" => Some(ContextSet::new()),
        json => serde_json::from_str(json).ok(),
    }
}

/// # Safety
/// `perms` is null or a live handle from [`rift_perms_connect`].
unsafe fn handle<'a>(perms: *mut rift_perms) -> Option<&'a rift_perms> {
    unsafe { perms.as_ref() }
}

/// A string the caller frees with [`rift_perms_free`].
fn owned(text: String) -> *mut c_char {
    CString::new(text.replace('\0', ""))
        .expect("NULs removed")
        .into_raw()
}

/// # Safety
/// `out` is null or writable.
unsafe fn put(out: *mut *mut c_char, value: *mut c_char) {
    if out.is_null() {
        unsafe { rift_perms_free(value) };
    } else {
        unsafe { out.write(value) };
    }
}

fn error_json(code: &str, error: impl ToString) -> String {
    json!({"error": error.to_string(), "code": code}).to_string()
}

/// Logs to stderr for hosts without a Rust logger, so `permissions
/// unavailable` and stale-replica warnings reach the operator.
struct Stderr;

impl log::Log for Stderr {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) && record.target().starts_with("rift_permissions") {
            eprintln!("{}", record.args());
        }
    }

    fn flush(&self) {}
}

/// Connect with a JSON configuration (the `rift_permissions_client::Config`
/// fields: `address`, `server_name`, `ca`, `token_env`, `server`,
/// `join_timeout_ms`, `request_timeout_ms`, `offline_policy`). Returns null
/// on unusable configuration and, when `err` is not null, stores a message
/// there for [`rift_perms_free`]. An unreachable proxy is not an error: the
/// handle retries in the background and applies the offline policy.
///
/// # Safety
/// `config_json` is null or a NUL-terminated string; `err` is null or
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_perms_connect(
    config_json: *const c_char,
    err: *mut *mut c_char,
) -> *mut rift_perms {
    let result = guard(Err("internal error".to_owned()), || {
        let json = unsafe { text(config_json) }.ok_or("config_json must be UTF-8 JSON")?;
        let config: Config = serde_json::from_str(json).map_err(|e| format!("config: {e}"))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("rift-perms")
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let client = {
            let _entered = runtime.enter();
            Client::start(config).map_err(|e| e.to_string())?
        };
        if log::set_logger(&Stderr).is_ok() {
            log::set_max_level(log::LevelFilter::Info);
        }
        Ok(Box::into_raw(Box::new(rift_perms {
            client,
            listeners: Mutex::new(Vec::new()),
            runtime,
        })))
    });
    match result {
        Ok(perms) => {
            unsafe { put(err, ptr::null_mut()) };
            perms
        }
        Err(message) => {
            unsafe { put(err, owned(message)) };
            ptr::null_mut()
        }
    }
}

/// Disconnect, write the last snapshot when that offline policy is set, and
/// free the handle. Null is ignored.
///
/// # Safety
/// `perms` is null or a live handle, not used again afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_perms_close(perms: *mut rift_perms) {
    if perms.is_null() {
        return;
    }
    let perms = unsafe { Box::from_raw(perms) };
    guard((), || {
        if let Err(error) = perms.client.shutdown() {
            log::warn!("rift-permissions: {error}");
        }
        let rift_perms {
            client,
            listeners,
            runtime,
        } = *perms;
        drop(listeners);
        drop(client);
        runtime.shutdown_timeout(Duration::from_secs(1));
    });
}

/// Check `node` for the player with these 16 UUID bytes. `ctx_json` is a
/// JSON context object (`{"world": "nether"}`), or null for none; the
/// handle's `server` is added when absent. Returns 1 (true), 0 (false), -1
/// (undefined: nothing matched, apply your default) or -2 (unavailable: no
/// data for this player, or invalid arguments).
///
/// # Safety
/// `perms` is null or a live handle; `uuid` is null or 16 readable bytes;
/// `node` and `ctx_json` are null or NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_perms_check(
    perms: *mut rift_perms,
    uuid: *const u8,
    node: *const c_char,
    ctx_json: *const c_char,
) -> c_int {
    guard(UNAVAILABLE, || {
        let (Some(perms), Some(uuid), Some(node), Some(contexts)) = (unsafe {
            (
                handle(perms),
                self::uuid(uuid),
                text(node),
                self::contexts(ctx_json),
            )
        }) else {
            return UNAVAILABLE;
        };
        if !perms.client.replica().contains(uuid) {
            return UNAVAILABLE;
        }
        match perms.client.check(uuid, node, &contexts) {
            Tristate::True => TRUE,
            Tristate::False => FALSE,
            Tristate::Undefined => UNDEFINED,
        }
    })
}

/// The player's prefix, suffix, meta and primary group as JSON
/// (`{"prefix", "suffix", "meta", "primary_group"}`), or null without data
/// or on invalid arguments. Free with [`rift_perms_free`].
///
/// # Safety
/// As [`rift_perms_check`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_perms_meta(
    perms: *mut rift_perms,
    uuid: *const u8,
    ctx_json: *const c_char,
) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let (Some(perms), Some(uuid), Some(contexts)) =
            (unsafe { (handle(perms), self::uuid(uuid), self::contexts(ctx_json)) })
        else {
            return ptr::null_mut();
        };
        perms
            .client
            .meta(uuid, &contexts)
            .map_or(ptr::null_mut(), |meta| {
                owned(serde_json::to_string(&meta).expect("meta serialises"))
            })
    })
}

/// The newest store revision the replica has seen; 0 for null.
///
/// # Safety
/// `perms` is null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_perms_revision(perms: *mut rift_perms) -> u64 {
    guard(0, || {
        unsafe { handle(perms) }.map_or(0, |perms| perms.client.revision())
    })
}

/// Apply a change (`change_json`, the protocol's change object) on the
/// proxy, blocking until it commits or the request timeout passes. `actor`
/// is the acting player's UUID as 32 hex digits, or null for this backend.
/// Returns 0 with the applied JSON (`{"revision", "ops", "changed_users",
/// "changed_groups"}`) in `*reply_json`, -1 when the proxy refused, was
/// unreachable or timed out, and -2 for invalid arguments; on failure
/// `*reply_json` is `{"error", "code"}` with a protocol error code,
/// `unavailable` or `timeout`. Free the reply with [`rift_perms_free`].
///
/// # Safety
/// `perms` is null or a live handle; `actor` and `change_json` are null or
/// NUL-terminated; `reply_json` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_perms_apply(
    perms: *mut rift_perms,
    actor: *const c_char,
    change_json: *const c_char,
    reply_json: *mut *mut c_char,
) -> c_int {
    let (status, reply) = guard((INVALID, error_json("internal", "panic")), || {
        let invalid = |error: &str| (INVALID, error_json("invalid_request", error));
        let Some(perms) = (unsafe { handle(perms) }) else {
            return invalid("null handle");
        };
        let actor = match unsafe { text(actor) } {
            None if actor.is_null() => None,
            None => return invalid("actor must be UTF-8"),
            Some(actor) => match Uuid::parse(actor) {
                Ok(uuid) => Some(uuid),
                Err(error) => return invalid(&error.to_string()),
            },
        };
        let change: Change = match unsafe { text(change_json) }.map(serde_json::from_str) {
            Some(Ok(change)) => change,
            Some(Err(error)) => return invalid(&error.to_string()),
            None => return invalid("change_json must be UTF-8 JSON"),
        };
        match perms.runtime.block_on(perms.client.apply(actor, change)) {
            Ok(applied) => (
                APPLIED,
                serde_json::to_string(&applied).expect("applied serialises"),
            ),
            Err(error) => {
                let code = match &error {
                    Error::Rejected(reply) => serde_json::to_value(reply.code)
                        .expect("codes serialise")
                        .as_str()
                        .unwrap_or("internal")
                        .to_owned(),
                    Error::Unavailable => "unavailable".into(),
                    Error::Timeout => "timeout".into(),
                    _ => "internal".into(),
                };
                (FAILED, error_json(&code, error))
            }
        }
    });
    unsafe { put(reply_json, owned(reply)) };
    status
}

/// Call `cb(uuid, revision, user)` whenever a player's data in the replica
/// changes, until the handle is closed. The callback runs on the handle's
/// thread: return quickly and do not call `rift_perms_join` or
/// `rift_perms_apply` from it. Null arguments are ignored.
///
/// # Safety
/// `perms` is null or a live handle; `cb` stays callable with `user` until
/// [`rift_perms_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_perms_subscribe(
    perms: *mut rift_perms,
    cb: Option<unsafe extern "C" fn(uuid: *const u8, revision: u64, user: *mut c_void)>,
    user: *mut c_void,
) {
    struct User(*mut c_void);
    impl User {
        fn get(&self) -> *mut c_void {
            self.0
        }
    }
    // The caller promises `user` may be used from the handle's thread.
    unsafe impl Send for User {}
    unsafe impl Sync for User {}
    guard((), || {
        let (Some(perms), Some(cb)) = (unsafe { handle(perms) }, cb) else {
            return;
        };
        let user = User(user);
        let listener = perms.client.on_snapshot(move |subject| unsafe {
            cb(
                subject.uuid.as_bytes().as_ptr(),
                subject.revision,
                user.get(),
            )
        });
        perms
            .listeners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(listener);
    });
}

/// A player is logging in: block up to the join timeout until their
/// snapshot is present. Returns 1 when it is, 0 when the offline policy
/// applies to them instead (fetching is retried every 5 s), -2 for invalid
/// arguments. Pair it with [`rift_perms_leave`], so the replica keeps the
/// players who are online and evicts the others.
///
/// # Safety
/// `perms` is null or a live handle; `uuid` is null or 16 readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_perms_join(perms: *mut rift_perms, uuid: *const u8) -> c_int {
    guard(UNAVAILABLE, || {
        let (Some(perms), Some(uuid)) = (unsafe { (handle(perms), self::uuid(uuid)) }) else {
            return UNAVAILABLE;
        };
        c_int::from(perms.runtime.block_on(perms.client.join(uuid)))
    })
}

/// A player left; their data is evicted 10 minutes later unless they join
/// again. Null arguments are ignored.
///
/// # Safety
/// As [`rift_perms_join`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_perms_leave(perms: *mut rift_perms, uuid: *const u8) {
    guard((), || {
        if let (Some(perms), Some(uuid)) = unsafe { (handle(perms), self::uuid(uuid)) } {
            perms.client.leave(uuid);
        }
    });
}

/// Free a string returned by this library. Null is ignored.
///
/// # Safety
/// `text` is null or a string from this library, not freed before.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_perms_free(text: *mut c_char) {
    if !text.is_null() {
        drop(unsafe { CString::from_raw(text) });
    }
}
