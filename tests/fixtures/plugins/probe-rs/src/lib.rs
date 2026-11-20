//! `probe-rs`: a test-only guest for what `hello-rs` cannot show: traps,
//! deadline overruns, stop requests, capabilities, captured output,
//! background work and the player directory.
//!
//! - `init(config)`: fails with `config.fail` when set, and with the error of
//!   `text.render` when `config.check` (a JSON text) does not render;
//!   otherwise registers the commands `probe` (permission `probe.use`,
//!   dynamic suggestions), `slow` (alias `sl`), `whois` (also for the
//!   console), `lang`, `tempgroup` and `text`, and the hooks `login`,
//!   `command`, `join`, `suggest` and `status`.
//! - `reload(config)`: fails when `config.fail` is set or `config.check`
//!   does not render.
//! - `login(ctx)`: name `Spin` loops forever, name `Trap` panics (a trap
//!   after the panic message on stderr), name `Dialog` is shown a dialog with
//!   a text input `answer` and a `dynamic/custom` action `reply` (additions
//!   `{"round": 1}`) and continues, name `Title` is shown a title (markup)
//!   and subtitle (legacy codes) for 5/40/10 ticks and continues, name
//!   `Locale` is denied with `locale=<the player's player-info locale>`
//!   (`locale=unknown` for `none` and for a player who is not online), name
//!   `Menu` is denied with what the menu functions answered (see `menus`),
//!   anyone else continues.
//! - `suggest(ctx)`: `rich` with the tooltip markup `<red>Hi <p>` (argument
//!   `p` the player's name) and `plain` without a tooltip; for the word
//!   `bad`, a JSON tooltip that is not JSON.
//! - `status(ping)`: for hostname `rich.example` a markup version and the
//!   sample lines `<green>one` (markup) and `two` (plain); for `bad.example`
//!   a JSON version that is not JSON; otherwise nothing.
//! - `dialog(response)`: messages the player `<action> <values> <payload>`.
//!   The registration does not list it: dialog clicks need none.
//! - `join(ctx)`: name `Spin` loops forever.
//! - `command(ctx)`, by `ctx.command`:
//!   - `probe`: prints `probe stdout <args>` and a partial line to stdout,
//!     then replies with a JSON report of what the guest can see: whether
//!     `/data` is writable, `/host` readable and `/host` writable,
//!     `meta.data-dir` and `meta.config-dir`, the environment's variable
//!     names and the outcome of a TCP connection to 127.0.0.1:9.
//!   - `slow` (alias `sl`): defers the answer with `background.defer`,
//!     queues the argument bytes with `background.run`, remembers the job's
//!     ticket and returns `continue` (ignored). A failing `defer` answers
//!     with its error as the message; a failing `run` resolves the ticket
//!     with its error as the message.
//!   - `whois`: `players.lookup(args)` and the message `<uuid> <name>
//!     online=<bool> language=<language or -> server=<last-server>
//!     ip=<last-ip>`, `unknown` for a player Rift never saw, or the host's
//!     error. From the console (`ctx.console`) the message starts with
//!     `console: `.
//!   - `lang`: `players.set-language(ctx.uuid, args)` (`none` for empty
//!     arguments) and the message `language <true|false>`, or the host's
//!     error.
//!   - `tempgroup`: arguments `<group> <expires-at>`;
//!     `permissions.add-group(ctx.uuid, group, expires-at)` and the message
//!     `added <true|false>`, or the host's error; `usage: tempgroup <group>
//!     <expires-at>` for anything else.
//!   - `text`: arguments `<language or -> <json text>`; the message
//!     `<render> | <plain> | <to-legacy> | <to-mini>` of the `text`
//!     interface, each `error: <the host's error>` when it fails.
//! - `menus(ctx)`: shows the player the hopper `probe` (slot 0 a stone
//!   button `s` with payload `{"a":1}`, state `"st"`) through the builders,
//!   tries a menu with an unknown item and a patch whose `slots` is not a
//!   list, patches the title of `probe`, closes it, and shows and closes a
//!   menu for the offline player `Nobody`; the deny text is
//!   `shown=<r> updated=<r> closed=<r> offline=<r>,<r>,<r> item=<error>
//!   patch=<error>`, each `<r>` the call's result as `Ok(true)`, `Ok(false)`
//!   or `Err(..)`.
//! - `job_done(handle, outcome)`: resolves the ticket of a `slow` command
//!   with the message `done: <result as text>` or `failed: <error>`.
//! - `work_init(config)` (worker): logs `worker ready` at info level.
//! - `work(payload)` (worker): the payload in ASCII upper case; payload
//!   `fail` fails with `worker says no`; payload `sleep` sleeps 300 ms and
//!   answers `SLEPT`; payload `text <language>` answers the plugin's message
//!   `denied` without formatting in that language (`text.plain`).
use rift_plugin::menu::{Button, Menu, MenuPatch};
use rift_plugin::{
    Action, CommandSpec, Context, DialogResponse, Hook, Ping, Plugin, Registration, StatusOverride,
    SuggestKind, Suggestion, Text, TitleSpec, background, meta, permissions, players, text,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

#[derive(Default, Deserialize)]
struct Config {
    #[serde(default)]
    fail: Option<String>,
    /// A JSON text the plugin checks with `text.render`.
    #[serde(default)]
    check: Option<String>,
}

impl Config {
    fn check(self) -> Result<(), String> {
        if let Some(reason) = self.fail {
            return Err(reason);
        }
        if let Some(template) = self.check {
            text::render(Text::json(template), None)?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct Probe {
    /// Job handle to the ticket of the `slow` command that waits for it.
    waiting: HashMap<u64, u64>,
}

fn spin() -> ! {
    let mut counter = 0u64;
    loop {
        counter = std::hint::black_box(counter.wrapping_add(1));
    }
}

impl Plugin for Probe {
    fn init(&mut self, config: &str) -> Result<Registration, String> {
        rift_plugin::config::<Config>(config)?.check()?;
        Ok(Registration::new()
            .command(
                CommandSpec::new("probe")
                    .permission("probe.use")
                    .suggest(SuggestKind::Dynamic),
            )
            .command(CommandSpec::new("slow").alias("sl"))
            .command(CommandSpec::new("whois").console())
            .command(CommandSpec::new("lang"))
            .command(CommandSpec::new("tempgroup"))
            .command(CommandSpec::new("text"))
            .hooks([
                Hook::Login,
                Hook::Command,
                Hook::Join,
                Hook::Suggest,
                Hook::Status,
            ]))
    }

    fn reload(&mut self, config: &str) -> Result<(), String> {
        rift_plugin::config::<Config>(config)?.check()
    }

    fn login(&mut self, ctx: Context) -> Action {
        match ctx.name.as_str() {
            "Spin" => spin(),
            "Trap" => panic!("probe trap"),
            "Dialog" => {
                let shown = players::show_dialog(
                    &ctx.uuid,
                    &serde_json::json!({
                        "type": "notice",
                        "title": "Probe",
                        "inputs": [{"type": "text", "key": "answer", "label": "Answer"}],
                        "action": {"label": "Reply", "action": {
                            "type": "dynamic/custom", "id": "reply", "additions": {"round": 1},
                        }},
                    }),
                );
                assert_eq!(shown, Ok(true));
                // Invalid dialogs are the plugin's error, not the player's.
                let invalid = players::dialog(&ctx.uuid, r#"{"type":"notice"}"#);
                assert!(invalid.unwrap_err().contains("title is required"));
                Action::Continue
            }
            "Title" => {
                let title = TitleSpec::new(text::mini("<gold>Welcome"))
                    .subtitle(text::legacy("&7to the probe"))
                    .times(5, 40, 10);
                assert_eq!(players::title(&ctx.uuid, &title), Ok(true));
                Action::Continue
            }
            "Menu" => menus(&ctx),
            "Locale" => {
                let locale = players::get(&ctx.uuid)
                    .expect("players.get")
                    .and_then(|player| player.locale);
                Action::deny(format!("locale={}", locale.as_deref().unwrap_or("unknown")))
            }
            _ => Action::Continue,
        }
    }

    fn suggest(&mut self, ctx: Context) -> Vec<Suggestion> {
        if ctx.word == "bad" {
            return vec![Suggestion::new("bad").tooltip(Text::json("{nope"))];
        }
        vec![
            Suggestion::new("rich").tooltip(text::mini("<red>Hi <p>").arg("p", ctx.name)),
            Suggestion::new("plain"),
        ]
    }

    fn status(&mut self, ping: Ping) -> Option<StatusOverride> {
        match ping.hostname.as_str() {
            "rich.example" => Some(StatusOverride {
                version: Some(text::mini("<gold>Probe").into()),
                sample: Some(vec![text::mini("<green>one").into(), "two".into()]),
                ..Default::default()
            }),
            "bad.example" => Some(StatusOverride {
                version: Some(Text::json("[1,")),
                ..Default::default()
            }),
            _ => None,
        }
    }

    fn dialog(&mut self, response: DialogResponse) {
        let text = format!(
            "{} {} {}",
            response.action,
            response.values_json,
            response.payload_json.as_deref().unwrap_or("-")
        );
        let _ = players::message(&response.ctx.uuid, text);
    }

    fn join(&mut self, ctx: Context) {
        if ctx.name == "Spin" {
            spin();
        }
    }

    fn command(&mut self, ctx: Context) -> Action {
        match ctx.command.as_deref() {
            Some("slow") => self.slow(&ctx),
            Some("whois") => whois(&ctx),
            Some("lang") => lang(&ctx),
            Some("tempgroup") => tempgroup(&ctx),
            Some("text") => convert(&ctx),
            _ => report(&ctx),
        }
    }

    fn job_done(&mut self, handle: u64, outcome: Result<Vec<u8>, String>) {
        let Some(ticket) = self.waiting.remove(&handle) else {
            return;
        };
        let action = match outcome {
            Ok(result) => Action::message(format!("done: {}", String::from_utf8_lossy(&result))),
            Err(error) => Action::message(format!("failed: {error}")),
        };
        let _ = background::resolve(ticket, action);
    }

    fn work_init(&mut self, _config: &str) -> Result<(), String> {
        rift_plugin::log::info!("worker ready");
        Ok(())
    }

    fn work(&mut self, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        match payload.as_slice() {
            b"fail" => Err("worker says no".into()),
            [b't', b'e', b'x', b't', b' ', language @ ..] => {
                let language = String::from_utf8_lossy(language);
                text::plain(text::message("denied"), Some(&*language)).map(String::into_bytes)
            }
            b"sleep" => {
                std::thread::sleep(Duration::from_millis(300));
                Ok(b"SLEPT".to_vec())
            }
            _ => Ok(payload.to_ascii_uppercase()),
        }
    }
}

impl Probe {
    /// `slow`: the answer comes from `job_done`.
    fn slow(&mut self, ctx: &Context) -> Action {
        let ticket = match background::defer() {
            Ok(ticket) => ticket,
            Err(error) => return Action::message(error),
        };
        match background::run(ctx.args.as_bytes()) {
            Ok(handle) => {
                self.waiting.insert(handle, ticket);
            }
            Err(error) => {
                let _ = background::resolve(ticket, Action::message(error));
            }
        }
        Action::Continue
    }
}

/// `Menu`: every menu function, and the errors of an invalid menu and patch.
fn menus(ctx: &Context) -> Action {
    let hopper = Menu::hopper("probe")
        .slot(
            0,
            Button::new("stone")
                .id("s")
                .payload(serde_json::json!({"a": 1})),
        )
        .state("st");
    let shown = players::show_menu(&ctx.uuid, &hopper);
    let item = players::menu(
        &ctx.uuid,
        r#"{"id":"probe","slots":[{"slot":0,"item":"no_such_item"}]}"#,
    )
    .unwrap_err();
    let patch = players::update_menu_json(&ctx.uuid, "probe", r#"{"slots":1}"#).unwrap_err();
    let updated = players::update_menu(&ctx.uuid, "probe", &MenuPatch::new().title("Patched"));
    let closed = players::close_menu(&ctx.uuid, Some("probe"));
    let offline = (
        players::show_menu("Nobody", &hopper),
        players::update_menu("Nobody", "probe", &MenuPatch::new()),
        players::close_menu("Nobody", None),
    );
    Action::deny(format!(
        "shown={shown:?} updated={updated:?} closed={closed:?} offline={:?},{:?},{:?} item={item} patch={patch}",
        offline.0, offline.1, offline.2
    ))
}

/// `whois`: what Rift remembers about a player.
fn whois(ctx: &Context) -> Action {
    let text = match players::lookup(&ctx.args) {
        Ok(Some(record)) => format!(
            "{} {} online={} language={} server={} ip={}",
            record.uuid,
            record.name,
            record.online,
            record.language.as_deref().unwrap_or("-"),
            record.last_server,
            record.last_ip
        ),
        Ok(None) => "unknown".to_owned(),
        Err(error) => error,
    };
    if ctx.console {
        Action::message(format!("console: {text}"))
    } else {
        Action::message(text)
    }
}

/// `lang`: set or clear the player's language.
fn lang(ctx: &Context) -> Action {
    let language = (!ctx.args.is_empty()).then_some(ctx.args.as_str());
    match players::set_language(&ctx.uuid, language) {
        Ok(known) => Action::message(format!("language {known}")),
        Err(error) => Action::message(error),
    }
}

/// `tempgroup <group> <expires-at>`: a group membership that expires.
fn tempgroup(ctx: &Context) -> Action {
    let mut words = ctx.args.split_whitespace();
    let (Some(group), Some(Ok(expires)), None) = (
        words.next(),
        words.next().map(str::parse::<u64>),
        words.next(),
    ) else {
        return Action::message("usage: tempgroup <group> <expires-at>");
    };
    match permissions::add_group(&ctx.uuid, group, Some(expires)) {
        Ok(added) => Action::message(format!("added {added}")),
        Err(error) => Action::message(error),
    }
}

/// `text <language or -> <json text>`: the four functions of the `text`
/// interface.
fn convert(ctx: &Context) -> Action {
    let (language, json) = ctx.args.split_once(' ').unwrap_or((ctx.args.as_str(), ""));
    let language = (language != "-").then_some(language);
    let source = Text::json(json);
    let results = [
        text::render(source.clone(), language),
        text::plain(source.clone(), language),
        text::to_legacy(source.clone(), language),
        text::to_mini(source, language),
    ];
    let results: Vec<String> = results
        .into_iter()
        .map(|result| result.unwrap_or_else(|error| format!("error: {error}")))
        .collect();
    Action::message(results.join(" | "))
}

/// `probe`: what the guest can see.
fn report(ctx: &Context) -> Action {
    println!("probe stdout {}", ctx.args);
    print!("partial");
    let _ = std::io::stdout().flush();
    let data = std::fs::write("/data/probe.txt", b"written")
        .and_then(|()| std::fs::read("/data/probe.txt"))
        .is_ok_and(|contents| contents == b"written");
    let host = std::fs::read_dir("/host").is_ok();
    let host_writable = std::fs::write("/host/probe-write.txt", b"written").is_ok();
    let network = match std::net::TcpStream::connect("127.0.0.1:9") {
        Ok(_) => "connected".to_owned(),
        Err(error) => format!("{:?}", error.kind()),
    };
    let report = serde_json::json!({
        "data": data,
        "host": host,
        "host_writable": host_writable,
        "meta_data": meta::data_dir(),
        "meta_config": meta::config_dir(),
        "env": std::env::vars().map(|(name, _)| name).collect::<Vec<_>>(),
        "network": network,
    });
    Action::message(report.to_string())
}

rift_plugin::export!(Probe);
