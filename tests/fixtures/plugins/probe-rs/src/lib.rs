//! `probe-rs`: a test-only guest for what `hello-rs` cannot show: traps,
//! deadline overruns, stop requests, capabilities and captured output.
//!
//! - `init(config)`: fails with `config.fail` when set; otherwise registers
//!   command `probe` (permission `probe.use`) and hooks `login`, `command`
//!   and `join`.
//! - `reload(config)`: fails when `config.fail` is set.
//! - `login(ctx)`: name `Spin` loops forever, name `Trap` panics (a trap
//!   after the panic message on stderr), name `Dialog` is shown a dialog with
//!   a text input `answer` and a `dynamic/custom` action `reply` (additions
//!   `{"round": 1}`) and continues, anyone else continues.
//! - `dialog(response)`: messages the player `<action> <values> <payload>`.
//!   The registration does not list it: dialog clicks need none.
//! - `join(ctx)`: name `Spin` loops forever.
//! - `command(ctx)`: prints `probe stdout <args>` and a partial line to
//!   stdout, then replies with a JSON report of what the guest can see:
//!   whether `/data` is writable, `/host` readable and `/host` writable,
//!   `meta.data-dir` and
//!   `meta.config-dir`, the environment's variable names and the outcome of
//!   a TCP connection to 127.0.0.1:9.
use rift_plugin::{
    Action, CommandSpec, Context, DialogResponse, Hook, Plugin, Registration, meta, players,
};
use serde::Deserialize;
use std::io::Write;

#[derive(Default, Deserialize)]
struct Config {
    #[serde(default)]
    fail: Option<String>,
}

#[derive(Default)]
struct Probe;

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
            .command(CommandSpec::new("probe").permission("probe.use"))
            .hooks([Hook::Login, Hook::Command, Hook::Join]))
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
            _ => Action::Continue,
        }
    }

    fn dialog(&mut self, response: DialogResponse) {
        let text = format!(
            "{} {} {}",
            response.action,
            response.values_json,
            response.payload_json.as_deref().unwrap_or("-")
        );
        let _ = players::message(&response.ctx.uuid, &text);
    }

    fn join(&mut self, ctx: Context) {
        if ctx.name == "Spin" {
            spin();
        }
    }

    fn command(&mut self, ctx: Context) -> Action {
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
}

rift_plugin::export!(Probe);
