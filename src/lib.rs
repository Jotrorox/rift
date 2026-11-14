// Every `eprintln!` in this crate resolves to this macro instead of the
// standard one, because it is defined before any `mod`. The standard macro
// panics when stderr cannot be written: a log pipe whose reader died, or a
// full disk behind a redirect. A diagnostic is never worth that. In the
// control loop such a panic would skip the connection drain and leave managed
// servers running without their supervisor. Keep in step with main.rs.
macro_rules! eprintln {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::writeln!(::std::io::stderr().lock(), $($arg)*);
    }};
}

pub mod auth;
pub mod backend_version;
pub mod bungeecord;
pub mod config;
pub mod database;
pub mod dialog;
pub mod environment;
pub mod extensions;
pub mod files;
mod forwarding;
pub mod hooks;
pub mod http_script;
pub mod instance_login;
pub mod instance_store;
pub mod managed;
pub mod message_script;
pub mod messaging;
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
pub mod text;
