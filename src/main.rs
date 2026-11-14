// Every `eprintln!` in this crate resolves to this macro instead of the
// standard one, because it is defined before any `mod`. The standard macro
// panics when stderr cannot be written: a log pipe whose reader died, or a
// full disk behind a redirect. A diagnostic is never worth that. In the
// control loop such a panic would skip the connection drain and leave managed
// servers running without their supervisor. Keep in step with lib.rs.
macro_rules! eprintln {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::writeln!(::std::io::stderr().lock(), $($arg)*);
    }};
}

mod admin;
mod admission;
mod bungee_runtime;
mod check;
mod control;
mod dev;
mod events;
mod groups;
mod health;
mod messaging_runtime;
mod metrics;
mod network;
mod operator;
mod runtime;
mod scaling;
mod status;
mod web;
mod web_transport;

use rift::{
    config::{Config, Route},
    routing::{Backend, Routes},
};
use std::{
    collections::BTreeMap,
    env, fs, io,
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
};

const DEFAULT_CONFIG_PATH: &str = "rift.lua";

const USAGE: &str = "Usage: rift [<listen-ip:port> <backend-ip:port> [--translation <on|off>]]\n\
    Backend may also be a DNS hostname with a port.\n\
    rift --config <path>\n\
    rift init [path] (generate configuration; default ./rift.lua)\n\
    rift check [--strict] [path] (validate configuration; default ./rift.lua)\n\
    rift --check [--strict] [path] (alias for rift check; default ./rift.lua)\n\
    --strict also checks environment secrets, the instance database and extension store,\n\
    port binds, and whether a running Rift could reload to this configuration\n\
    rift admin [--address <loopback-ip:port>] <command> (run an authenticated administrator command)\n\
    rift admin perms <command> (the /perms grammar; rift admin perms help lists it)\n\
    rift dev [options] (development proxy for plugin authors; see rift dev --help)\n\
    rift node --controller <host:port> --name <node> --root <dir> --ca <pem> (run servers for a controller; see rift node --help)\n\
    rift --version (print package version)\n\
    rift --license (print project license and third-party notices)\n\
    Routing: rift <listen-ip:port> [--route <hostname=backend:port>]... [--default <backend:port>] [--translation <on|off>]\n\
    Routes: exact hostname, '*.example.com', or '*' (default).\n\
    Translation between Minecraft versions is on unless --translation off; see docs/translation.md.\n\
    Priority: exact, longest wildcard suffix, default. Unmatched clients get a message.\n\
    No arguments: load ./rift.lua if present, otherwise use defaults.\n\
    Defaults: 0.0.0.0:25565 127.0.0.1:25566\n\
    Explicit addresses and routing options override ./rift.lua.\n\
    Reload: SIGHUP (Unix), Ctrl-Break (Windows); new connections use new routes, existing sessions continue.\n\
    Invalid reloads retain the working configuration. Stop and drain: Ctrl-C or SIGTERM.\n\
    IPv6: rift '[::]:25565' '[::1]:25566'";

fn main() -> ExitCode {
    // First, while this is the only thread and before any secret is read.
    rift::environment::harden_process();
    let args: Vec<String> = env::args().skip(1).collect();
    // `rift dev` sets its token variables before any thread exists.
    let dev = match args.first().map(String::as_str) {
        Some("dev")
            if args
                .get(1)
                .is_some_and(|arg| arg == "--help" || arg == "-h") =>
        {
            println!("{}", dev::USAGE);
            return ExitCode::SUCCESS;
        }
        Some("dev") => match dev::prepare(&args[1..]) {
            Ok(dev) => Some(dev),
            Err(error) => {
                eprintln!("rift: {error}");
                return ExitCode::FAILURE;
            }
        },
        _ => None,
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("rift: cannot start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async move {
        match dev {
            Some(dev) => dev::run(dev).await,
            None => start(args).await,
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rift: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn start(args: Vec<String>) -> io::Result<()> {
    if args.first().is_some_and(|arg| arg == "admin") {
        return crate::admin::client(&args[1..]).await;
    }
    if args.first().is_some_and(|arg| arg == "node") {
        if args
            .get(1)
            .is_some_and(|arg| arg == "--help" || arg == "-h")
        {
            println!("{}", rift::node::agent::USAGE);
            return Ok(());
        }
        let options = rift::node::agent::Options::parse(&args[1..])?;
        return rift::node::agent::run(options).await;
    }
    if args
        .first()
        .is_some_and(|arg| arg == "init" || arg == "check" || arg == "--check")
    {
        if args[1..].iter().any(|arg| arg == "--help" || arg == "-h") {
            println!("{USAGE}");
            return Ok(());
        }
        let strict = args[0] != "init" && args[1..].iter().any(|arg| arg == "--strict");
        let operands: Vec<&String> = args[1..]
            .iter()
            .filter(|arg| !(strict && *arg == "--strict"))
            .collect();
        // An unknown option is not a path; `./-name` still names such a file.
        if operands.len() > 1 || operands.iter().any(|arg| arg.starts_with('-')) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE));
        }
        let path = operands
            .first()
            .map(|arg| arg.as_str())
            .unwrap_or(DEFAULT_CONFIG_PATH);
        if args[0] == "init" {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|error| {
                    let detail = if error.kind() == io::ErrorKind::AlreadyExists {
                        "already exists; choose a new path or edit the existing configuration"
                            .to_owned()
                    } else {
                        error.to_string()
                    };
                    io::Error::new(error.kind(), format!("{path}: {detail}"))
                })?;
            file.write_all(include_bytes!("../examples/rift.lua"))?;
            println!("rift: created {path}; edit your backends, then run: rift check {path}");
        } else {
            let (config, cost) = Config::check(Path::new(path))?;
            println!("rift: configuration valid: {path}");
            // Every Lua callback evaluates the configuration again before it
            // runs, so this cost is paid in time on each connection and hook.
            let limit = rift::config::LUA_INSTRUCTION_LIMIT;
            println!(
                "rift: configuration evaluation uses about {cost} of {limit} Lua instructions ({}%); each callback re-evaluates it and then gets its own {limit}",
                u64::from(cost) * 100 / u64::from(limit)
            );
            if u64::from(cost) * 2 >= u64::from(limit) {
                eprintln!(
                    "rift: warning: configuration evaluation is heavy; move expensive work into callbacks that need it, or callbacks may miss their 50 ms deadline"
                );
            }
            if let Some(warning) = check::offline_mode(&config) {
                eprintln!("rift: warning: {warning}");
            }
            for warning in check::instance_logins(&config) {
                eprintln!("rift: warning: {warning}");
            }
            for line in check::isolation(&config) {
                println!("rift: {line}");
            }
            let (plugins, warnings) = check::plugins(&config);
            for line in &plugins {
                println!("rift: {line}");
            }
            // Strict check reports a missing engine as an error instead.
            for warning in warnings
                .iter()
                .filter(|warning| !strict || !warning.contains("is not available"))
            {
                eprintln!("rift: warning: {warning}");
            }
            if strict {
                let path = fs::canonicalize(path)?;
                let report = check::strict(&config, &path);
                for note in &report.notes {
                    println!("rift: strict: {note}");
                }
                for error in &report.errors {
                    eprintln!("rift: strict: error: {error}");
                }
                if !report.errors.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "strict check failed with {} error(s): {path}",
                            report.errors.len(),
                            path = path.display()
                        ),
                    ));
                }
                println!("rift: strict checks passed: {}", path.display());
            }
        }
        return Ok(());
    }
    let (config, path) = match args.as_slice() {
        [] => match Config::load_unrestored(Path::new(DEFAULT_CONFIG_PATH)) {
            Ok(config) => (config, Some(PathBuf::from(DEFAULT_CONFIG_PATH))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (Config::default(), None),
            Err(error) => return Err(error),
        },
        [help] if help == "--help" || help == "-h" => {
            println!("{USAGE}");
            return Ok(());
        }
        [version] if version == "--version" || version == "-V" => {
            println!("rift {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        [license] if license == "--license" => {
            print!(
                "{}\n{}",
                include_str!("../LICENSE"),
                include_str!("../THIRD_PARTY_NOTICES")
            );
            return Ok(());
        }
        [flag, path] if flag == "--config" => (
            Config::load_unrestored(Path::new(path))?,
            Some(PathBuf::from(path)),
        ),
        [listen, backend] => (Config::from_addresses(listen, backend)?, None),
        [listen, backend, flag, value] if flag == "--translation" => {
            let mut config = Config::from_addresses(listen, backend)?;
            config.translation.enabled = match value.as_str() {
                "on" => true,
                "off" => false,
                _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE)),
            };
            (config, None)
        }
        [listen, options @ ..] if !options.is_empty() && options.len().is_multiple_of(2) => {
            let mut config = Config::from_addresses(listen, "127.0.0.1:0")?;
            config.backends.clear();
            let mut checked = Routes::default();
            let mut patterns = BTreeMap::new();
            for (index, option) in options.as_chunks::<2>().0.iter().enumerate() {
                let (pattern, target) = match option[0].as_str() {
                    "--route" => option[1].split_once('=').ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "route requires hostname=backend:port",
                        )
                    })?,
                    "--default" => ("*", option[1].as_str()),
                    "--translation" => {
                        config.translation.enabled = match option[1].as_str() {
                            "on" => true,
                            "off" => false,
                            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE)),
                        };
                        continue;
                    }
                    _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE)),
                };
                let backend = Backend::parse(target)?;
                checked.add_pattern(pattern, backend.clone())?;
                let name = format!("backend_{index}");
                config.backends.insert(name.clone(), backend);
                patterns.insert(pattern.to_owned(), name);
            }
            config
                .routes
                .insert("default".into(), Route::Hostnames(patterns));
            config.validate()?;
            (config, None)
        }
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE)),
    };
    runtime::serve(config, path).await
}

#[cfg(test)]
use {
    std::time::Duration,
    tokio::{
        net::{TcpListener, TcpStream},
        time::timeout,
    },
};

#[cfg(test)]
mod tests;
