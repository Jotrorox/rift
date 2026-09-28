mod admission;
mod handshake;
mod health;
mod metrics;
mod runtime;
mod status;

use rift::{
    config::{Config, Route},
    routing::{Backend, Routes},
};
use std::{
    collections::BTreeMap,
    env, io,
    path::{Path, PathBuf},
    process::ExitCode,
};

const USAGE: &str = "Usage: rift [<listen-ip:port> <backend-ip:port>]\n\
    Backend may also be a DNS hostname with a port.\n\
    rift --config <path>\n\
    rift --check <path> (validate configuration and scripts without binding)\n\
    Routing: rift <listen-ip:port> [--route <hostname=backend:port>]... [--default <backend:port>]\n\
    Routes: exact hostname, '*.example.com', or '*' (default).\n\
    Priority: exact, longest wildcard suffix, default. Unmatched clients close.\n\
    No arguments: load ./rift.lua if present, otherwise use defaults.\n\
    Defaults: 0.0.0.0:25565 127.0.0.1:25566\n\
    Explicit addresses and routing options override ./rift.lua.\n\
    Reload: SIGHUP (Unix), Ctrl-Break (Windows). Stop and drain: Ctrl-C or SIGTERM.\n\
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
    rift::{config::Limits, routing::Mode},
    std::{net::SocketAddr, time::Duration},
    tokio::{
        net::{TcpListener, TcpStream},
        time::{sleep, timeout},
    },
};

#[cfg(test)]
async fn handle(
    mut client: TcpStream,
    mode: &Mode,
    listeners: &[SocketAddr],
    limits: Limits,
) -> io::Result<(u64, u64)> {
    let Mode::Direct(backend) = mode else {
        unreachable!("relay test helper expects a direct backend")
    };
    let mut config = Config::from_addresses("127.0.0.1:0", "127.0.0.1:1")?;
    config.backends.insert("default".into(), backend.clone());
    config.limits = limits;
    let snapshot = runtime::Snapshot::new(config, None)?;
    runtime::handle(
        &mut client,
        "default",
        std::sync::Arc::new(snapshot),
        listeners,
        std::sync::Arc::new(metrics::Metrics::default()),
    )
    .await
}

#[cfg(test)]
mod tests;
