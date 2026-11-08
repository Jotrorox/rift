//! `hello-rs`: the Rust example plugin and a host conformance fixture.
//!
//! The C and Go examples behave identically, so keep every observable effect
//! below exact and deterministic.
//!
//! - `init(config)` decodes the JSON configuration (`{"deny": "<name>"}`,
//!   `deny` optional; invalid JSON fails `init`), starts one repeating
//!   1000 ms timer with `timers::every(1000)`, subscribes to the bus subject
//!   `hello.ping` without a queue group (a failed subscription fails `init`)
//!   and returns this registration:
//!   - command `hello`, permission `hello.use`, suggest `players`;
//!   - hooks `login`, `command`, `join`, `message`, `timer`, so the host skips
//!     every other hook.
//! - `login(ctx)` returns `deny("You are not welcome here.")` when `ctx.name`
//!   equals the configured `deny` name exactly, otherwise `continue`.
//! - `command(ctx)` returns `message("Hello, <ctx.name>!")`.
//! - `join(ctx)` calls `players::message(ctx.uuid, "Welcome, <ctx.name>!")`,
//!   then `store::increment("hello", "joins", 1)`. Host errors are ignored.
//! - `message(msg)`: when `msg.reply` is set, publishes `msg.payload`
//!   unchanged to that subject (no reply subject of its own); otherwise does
//!   nothing. Publish errors are ignored.
//! - `timer(handle)` logs `tick` at debug level.
//!
//! No other hook is implemented and nothing else is logged.

use rift_plugin::{
    Action, BusMessage, CommandSpec, Context, Hook, Plugin, Registration, SuggestKind, bus,
    players, store, timers,
};
use serde::Deserialize;

#[derive(Default, Deserialize)]
struct Config {
    #[serde(default)]
    deny: Option<String>,
}

#[derive(Default)]
struct Hello {
    deny: Option<String>,
}

impl Plugin for Hello {
    fn init(&mut self, config: &str) -> Result<Registration, String> {
        let config: Config = rift_plugin::config(config)?;
        self.deny = config.deny;
        timers::every(1000);
        bus::subscribe("hello.ping", None)?;
        Ok(Registration::new()
            .command(
                CommandSpec::new("hello")
                    .permission("hello.use")
                    .suggest(SuggestKind::Players),
            )
            .hooks([
                Hook::Login,
                Hook::Command,
                Hook::Join,
                Hook::Message,
                Hook::Timer,
            ]))
    }

    fn login(&mut self, ctx: Context) -> Action {
        if self.deny.as_deref() == Some(ctx.name.as_str()) {
            Action::deny("You are not welcome here.")
        } else {
            Action::Continue
        }
    }

    fn command(&mut self, ctx: Context) -> Action {
        Action::message(format!("Hello, {}!", ctx.name))
    }

    fn join(&mut self, ctx: Context) {
        let _ = players::message(&ctx.uuid, &format!("Welcome, {}!", ctx.name));
        let _ = store::increment("hello", "joins", 1);
    }

    fn message(&mut self, message: BusMessage) {
        let _ = bus::reply(&message, &message.payload);
    }

    fn timer(&mut self, _handle: u64) {
        rift_plugin::log::debug!("tick");
    }
}

rift_plugin::export!(Hello);
