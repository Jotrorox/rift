//! `probe-rs`: a test-only guest for what `hello-rs` cannot show: traps,
//! deadline overruns, stop requests, capabilities, captured output,
//! background work and the player directory.
//!
//! - `init(config)`: fails with `config.fail` when set; otherwise registers
//!   the commands `probe` (permission `probe.use`, dynamic suggestions),
//!   `slow` (alias `sl`), `whois` (also for the console), `lang` and
//!   `tempgroup`, and the hooks `login`, `command`, `join`, `suggest` and
//!   `status`.
//! - `reload(config)`: fails when `config.fail` is set.
//! - `login(ctx)`: name `Spin` loops forever, name `Trap` panics (a trap
//!   after the panic message on stderr), name `Dialog` is shown a dialog with
//!   a text input `answer` and a `dynamic/custom` action `reply` (additions
//!   `{"round": 1}`) and continues, name `Title` is shown a title (markup)
//!   and subtitle (legacy codes) for 5/40/10 ticks and continues, name
//!   `Locale` is denied with `locale=<the player's player-info locale>`
//!   (`locale=unknown` for `none` and for a player who is not online),
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
//! - `job_done(handle, outcome)`: resolves the ticket of a `slow` command
//!   with the message `done: <result as text>` or `failed: <error>`.
//! - `work_init(config)` (worker): logs `worker ready` at info level.
//! - `work(payload)` (worker): the payload in ASCII upper case; payload
//!   `fail` fails with `worker says no`; payload `sleep` sleeps 300 ms and
//!   answers `SLEPT`.
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
        let config: Config = rift_plugin::config(config)?;
        if let Some(reason) = config.fail {
            return Err(reason);
        }
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
            .hooks([
                Hook::Login,
                Hook::Command,
                Hook::Join,
                Hook::Suggest,
                Hook::Status,
            ]))
    }

    fn reload(&mut self, config: &str) -> Result<(), String> {
        let config: Config = rift_plugin::config(config)?;
        config.fail.map_or(Ok(()), Err)
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
