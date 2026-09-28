mod config;

use config::{Config, Limits};
use std::{env, io, net::SocketAddr, path::Path, process::ExitCode, sync::Arc, time::Duration};
use tokio::{
    io::copy_bidirectional_with_sizes,
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    task::JoinSet,
    time::{sleep, timeout},
};

const USAGE: &str = "Usage: rift [<listen-ip:port> <backend-ip:port>]\n\
    rift --config <path>\n\
    No arguments: load ./rift.lua if present, otherwise use defaults.\n\
    Defaults: 0.0.0.0:25565 127.0.0.1:25566\n\
    Explicit addresses override ./rift.lua.\n\
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
    let config = match args.as_slice() {
        [] => match Config::load(Path::new("rift.lua")) {
            Ok(config) => config,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Config::default(),
            Err(error) => return Err(error),
        },
        [help] if help == "--help" || help == "-h" => {
            println!("{USAGE}");
            return Ok(());
        }
        [flag, path] if flag == "--config" => Config::load(Path::new(path))?,
        [listen, backend] => Config::from_addresses(listen, backend)?,
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE)),
    };
    serve(config).await
}

async fn serve(config: Config) -> io::Result<()> {
    // Bind everything before accepting clients, so partial startup fails cleanly.
    let mut listeners = Vec::new();
    for (name, address) in &config.listeners {
        let listener = TcpListener::bind(address).await.map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("listeners.{name} ({address}): {error}"),
            )
        })?;
        let backend = config.backends[&config.routes[name]];
        listeners.push((listener, backend));
    }
    // One shared limit bounds sockets, tasks and buffers across all listeners.
    let connections = Arc::new(Semaphore::new(config.limits.max_connections));
    let mut tasks = JoinSet::new();
    for (listener, backend) in listeners {
        eprintln!("rift: {} -> {backend}", listener.local_addr()?);
        tasks.spawn(accept(
            listener,
            backend,
            config.limits,
            connections.clone(),
        ));
    }
    if let Some(result) = tasks.join_next().await {
        result.map_err(io::Error::other)?;
    }
    Err(io::Error::other("listener stopped unexpectedly"))
}

async fn accept(
    listener: TcpListener,
    backend: SocketAddr,
    limits: Limits,
    connections: Arc<Semaphore>,
) {
    loop {
        let (client, peer) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("rift: accept: {error}");
                // Avoid spinning if the process runs out of file descriptors.
                sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = connections.clone().try_acquire_owned() else {
            // Reject overload immediately instead of queuing unbounded work.
            continue;
        };
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) = relay(client, backend, limits).await {
                eprintln!("rift: {peer}: {error}");
            }
        });
    }
}

async fn relay(
    mut client: TcpStream,
    backend: SocketAddr,
    limits: Limits,
) -> io::Result<(u64, u64)> {
    client.set_nodelay(true)?;
    let mut upstream = timeout(limits.connect_timeout, TcpStream::connect(backend)).await??;
    upstream.set_nodelay(true)?;
    // Fixed buffers bound memory and preserve backpressure and TCP half-closes.
    copy_bidirectional_with_sizes(
        &mut client,
        &mut upstream,
        limits.buffer_size,
        limits.buffer_size,
    )
    .await
}

#[cfg(test)]
mod tests;
