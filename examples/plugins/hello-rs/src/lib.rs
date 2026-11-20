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
//!   - command `servers`, open to every player, no suggestions;
//!   - hooks `login`, `command`, `join`, `message`, `timer`, so the host skips
//!     every other hook.
//! - `login(ctx)` returns `deny("You are not welcome here.")` when `ctx.name`
//!   equals the configured `deny` name exactly, otherwise `continue`.
//! - `command(ctx)` for `hello` returns the message `greeting` with the
//!   argument `name` set to `ctx.name`, which Rift renders from
//!   `messages.toml` in the player's language (`Hello, <ctx.name>!` by
//!   default). For `servers` it shows the player the server selector (see
//!   `selector` below) with `players::show_menu` and returns `continue`.
//!   Host errors are ignored.
//! - `menu(event)`: a click on a button with the action `join` transfers the
//!   player to the server named by the button's payload. Host errors are
//!   ignored; every other event is ignored.
//! - `join(ctx)` calls `players::message(ctx.uuid, "Welcome, <ctx.name>!")`,
//!   then `store::increment("hello", "joins", 1)`. Host errors are ignored.
//! - `message(msg)`: when `msg.reply` is set, publishes `msg.payload`
//!   unchanged to that subject (no reply subject of its own); otherwise does
//!   nothing. Publish errors are ignored.
//! - `timer(handle)` logs `tick` at debug level.
//!
//! No other hook is implemented and nothing else is logged.

use rift_plugin::menu::{Button, Menu, MenuEvent, MenuKind};
use rift_plugin::{
    Action, BusMessage, CommandSpec, Context, Hook, Plugin, Registration, SuggestKind, bus,
    players, store, text, timers,
};
use serde::Deserialize;

#[derive(Default, Deserialize)]
struct Config {
    #[serde(default)]
    deny: Option<String>,
}

/// The server selector: a chest of three rows framed in gray glass, with a
/// button per server (action `join`, the server's name as payload) and one
/// that closes the menu.
fn selector() -> Menu {
    let server = |item: &str, name: &str, server: &str| {
        Button::new(item)
            .name(text::mini(name))
            .lore(["Click to join"])
            .id("join")
            .payload(server)
    };
    Menu::chest("servers")
        .title(text::mini("<dark_gray>Choose a server"))
        .layout(["#########", "# S C M #", "####X####"])
        .key('#', Button::new("gray_stained_glass_pane").name(" "))
        .key('S', server("grass_block", "<green>Survival", "survival"))
        .key('C', server("bricks", "<gold>Creative", "creative"))
        .key('M', server("bow", "<red>Minigames", "minigames"))
        .key(
            'X',
            Button::new("barrier")
                .name(text::mini("<red>Close"))
                .close(),
        )
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
            .command(CommandSpec::new("servers"))
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
        if ctx.command.as_deref() == Some("servers") {
            let _ = players::show_menu(&ctx.uuid, &selector());
            return Action::Continue;
        }
        // The text lives in messages.toml, translated per player.
        Action::message(text::message("greeting").arg("name", ctx.name))
    }

    fn menu(&mut self, event: MenuEvent) {
        if let MenuKind::Click { action: "join", .. } = event.kind()
            && let Ok(Some(server)) = event.payload::<String>()
        {
            let _ = players::transfer(&event.ctx.uuid, &server);
        }
    }

    fn join(&mut self, ctx: Context) {
        let _ = players::message(&ctx.uuid, format!("Welcome, {}!", ctx.name));
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
