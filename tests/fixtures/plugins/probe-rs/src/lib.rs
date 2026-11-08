//! `probe-rs`: a test-only guest for what `hello-rs` cannot show: traps,
//! deadline overruns, stop requests, capabilities and captured output.
//!
//! - `init(config)`: fails with `config.fail` when set; otherwise registers
//!   command `probe` (permission `probe.use`) and hooks `login`, `command`
//!   and `join`.
//! - `reload(config)`: fails when `config.fail` is set.
//! - `login(ctx)`: name `Spin` loops forever, name `Trap` panics (a trap
//!   after the panic message on stderr), anyone else continues.
//! - `join(ctx)`: name `Spin` loops forever.
//! - `command(ctx)`: prints `probe stdout <args>` and a partial line to
//!   stdout, then replies with a JSON report of what the guest can see:
//!   whether `/data` is writable and `/host` readable, `meta.data-dir` and
//!   `meta.config-dir`, the environment's variable names and the outcome of
//!   a TCP connection to 127.0.0.1:9.
use rift_plugin::{Action, CommandSpec, Context, Hook, Plugin, Registration, meta};
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
            _ => Action::Continue,
        }
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
        let network = match std::net::TcpStream::connect("127.0.0.1:9") {
            Ok(_) => "connected".to_owned(),
            Err(error) => format!("{:?}", error.kind()),
        };
        let report = serde_json::json!({
            "data": data,
            "host": host,
            "meta_data": meta::data_dir(),
            "meta_config": meta::config_dir(),
            "env": std::env::vars().map(|(name, _)| name).collect::<Vec<_>>(),
            "network": network,
        });
        Action::message(report.to_string())
    }
}

rift_plugin::export!(Probe);
