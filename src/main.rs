mod admin;
mod admission;
mod control;
mod events;
mod health;
mod messaging_runtime;
mod metrics;
mod network;
mod runtime;
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

const USAGE: &str = "Usage: rift [<listen-ip:port> <backend-ip:port>]\n\
    Backend may also be a DNS hostname with a port.\n\
    rift --config <path>\n\
    rift init [path] (generate configuration; default ./rift.lua)\n\
    rift check [path] (validate configuration; default ./rift.lua)\n\
    rift --check <path> (validate configuration and scripts without binding)\n\
    rift admin [--address <loopback-ip:port>] <command> (run an authenticated administrator command)\n\
    rift --version (print package version)\n\
    rift --license (print project license and third-party notices)\n\
    Routing: rift <listen-ip:port> [--route <hostname=backend:port>]... [--default <backend:port>]\n\
    Routes: exact hostname, '*.example.com', or '*' (default).\n\
    Priority: exact, longest wildcard suffix, default. Unmatched clients get a message.\n\
    No arguments: load ./rift.lua if present, otherwise use defaults.\n\
    Defaults: 0.0.0.0:25565 127.0.0.1:25566\n\
    Explicit addresses and routing options override ./rift.lua.\n\
    Reload: SIGHUP (Unix), Ctrl-Break (Windows); new connections use new routes, existing sessions continue.\n\
    Invalid reloads retain the working configuration. Stop and drain: Ctrl-C or SIGTERM.\n\
    IPv6: rift '[::]:25565' '[::1]:25566'";

#[tokio::main]
async fn main() -> ExitCode {
    match start().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rift: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn start() -> io::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "admin") {
        return crate::admin::client(&args[1..]).await;
    }
    if args
        .first()
        .is_some_and(|arg| arg == "init" || arg == "check")
    {
        if args.len() > 2 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE));
        }
        let path = args.get(1).map(String::as_str).unwrap_or("rift.lua");
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
            Config::load(Path::new(path))?;
            println!("rift: configuration valid: {path}");
        }
        return Ok(());
    }
    let (config, path) = match args.as_slice() {
        [] => match Config::load(Path::new("rift.lua")) {
            Ok(config) => (config, Some(PathBuf::from("rift.lua"))),
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
        [flag, path] if flag == "--check" => {
            Config::load(Path::new(path))?;
            println!("rift: configuration valid: {path}");
            return Ok(());
        }
        [flag, path] if flag == "--config" => {
            (Config::load(Path::new(path))?, Some(PathBuf::from(path)))
        }
        [listen, backend] => (Config::from_addresses(listen, backend)?, None),
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
