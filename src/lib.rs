// Every `eprintln!` in this crate resolves to this macro instead of the
// standard one, because it is defined before any `mod`. The standard macro
// panics when stderr cannot be written: a log pipe whose reader died, or a
// full disk behind a redirect. A diagnostic is never worth that. In the
// control loop such a panic would skip the connection drain and leave managed
// servers running without their supervisor. Keep in step with main.rs.
//
// Diagnostics of the running proxy go through the `log_*!` macros below
// (see log.rs); `eprintln!` is for output that answers a command. The
// library has none of that outside its tests today, hence the allow: the
// shadow stays so that a later use cannot bring the panicking macro back.
#[allow(unused_macros)]
macro_rules! eprintln {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::writeln!(::std::io::stderr().lock(), $($arg)*);
    }};
}

/// `rift: error: ...`: work was lost or a guarantee no longer holds.
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::log::write($crate::log::Level::Error, ::std::format_args!($($arg)*))
    };
}

/// `rift: warning: ...`: refused, dropped or retried, and Rift carried on.
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::log::write($crate::log::Level::Warn, ::std::format_args!($($arg)*))
    };
}

/// `rift: ...`: lifecycle messages.
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::log::write($crate::log::Level::Info, ::std::format_args!($($arg)*))
    };
}

/// `rift: debug: ...`: written only with `RIFT_LOG_LEVEL=debug`.
#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        $crate::log::write($crate::log::Level::Debug, ::std::format_args!($($arg)*))
    };
}

pub mod auth;
pub mod backend_version;
pub mod backup;
pub mod bungeecord;
pub mod cluster;
pub mod config;
pub mod database;
pub mod dialog;
pub mod directory;
pub mod environment;
pub mod extensions;
pub mod files;
mod forwarding;
pub mod hooks;
pub mod http_script;
pub mod instance_login;
pub mod instance_store;
pub mod log;
pub mod managed;
pub mod message_script;
pub mod messaging;
pub mod network_service;
pub mod node;
pub mod permissions;
pub mod players;
pub mod players_service;
pub mod plugins;
pub mod preflight;
pub mod protocol;
pub mod provisioning;
pub mod proxy_protocol;
pub mod routing;
mod script;
pub mod server_list;
pub mod service_state;
pub mod session;
pub mod software;
pub mod tab_list;
pub mod template_source;
pub mod text;
