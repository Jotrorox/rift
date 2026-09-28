mod handshake;

use rift::{
    config::{Config, Limits},
    hooks::{ConnectionInfo, RouteDecision, Router},
    routing::{Backend, Mode, Routes},
};
use std::{env, io, net::SocketAddr, path::Path, process::ExitCode, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncWriteExt, copy_bidirectional_with_sizes},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    task::JoinSet,
    time::{sleep, timeout},
};

const USAGE: &str = "Usage: rift [<listen-ip:port> <backend-ip:port>]\n\
    Backend may also be a DNS hostname with a port.\n\
    rift --config <path>\n\
    Routing: rift <listen-ip:port> [--route <hostname=backend:port>]... [--default <backend:port>]\n\
    Routes: exact hostname, '*.example.com', or '*' (default).\n\
    Priority: exact, longest wildcard suffix, default. Unmatched clients close.\n\
    No arguments: load ./rift.lua if present, otherwise use defaults.\n\
    Defaults: 0.0.0.0:25565 127.0.0.1:25566\n\
    Explicit addresses and routing options override ./rift.lua.\n\
    IPv6: rift '[::]:25565' '[::1]:25566'";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

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
        [listen, options @ ..] if !options.is_empty() && options.len().is_multiple_of(2) => {
            let listen = listen
                .parse::<SocketAddr>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let mut routes = Routes::default();
            for option in options.as_chunks::<2>().0 {
                match option[0].as_str() {
                    "--route" => routes.add(&option[1])?,
                    "--default" => routes.set_default(Backend::parse(&option[1])?)?,
                    _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE)),
                }
            }
            routes.check_loops(listen)?;
            return serve(
                vec![("default".into(), listen, Mode::Routed(routes))],
                Limits::default(),
                None,
            )
            .await;
        }
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE)),
    };
    let listeners = config
        .listeners
        .iter()
        .map(|(name, address)| Ok((name.clone(), *address, config.mode(name)?)))
        .collect::<io::Result<Vec<_>>>()?;
    let router = config.on_route.as_ref().map(|_| Router::new(&config));
    serve(listeners, config.limits, router).await
}

async fn serve(
    configured: Vec<(String, SocketAddr, Mode)>,
    limits: Limits,
    router: Option<Router>,
) -> io::Result<()> {
    // Bind everything before accepting clients, so partial startup fails cleanly.
    let mut listeners = Vec::new();
    for (name, address, mode) in configured {
        let listener = TcpListener::bind(address).await.map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("listeners.{name} ({address}): {error}"),
            )
        })?;
        listeners.push((listener, name, mode));
    }
    let addresses = Arc::new(
        listeners
            .iter()
            .map(|(listener, _, _)| listener.local_addr())
            .collect::<io::Result<Vec<_>>>()?,
    );
    // One shared limit bounds sockets, tasks and buffers across all listeners.
    let connections = Arc::new(Semaphore::new(limits.max_connections));
    let mut tasks = JoinSet::new();
    for (listener, name, mode) in listeners {
        eprintln!("rift: listening on {}", listener.local_addr()?);
        tasks.spawn(accept(
            listener,
            Arc::new(mode),
            limits,
            connections.clone(),
            addresses.clone(),
            router.as_ref().map(|router| (name, router.clone())),
        ));
    }
    if let Some(result) = tasks.join_next().await {
        result.map_err(io::Error::other)?;
    }
    Err(io::Error::other("listener stopped unexpectedly"))
}

async fn accept(
    listener: TcpListener,
    mode: Arc<Mode>,
    limits: Limits,
    connections: Arc<Semaphore>,
    addresses: Arc<Vec<SocketAddr>>,
    routing: Option<(String, Router)>,
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
        let mode = Arc::clone(&mode);
        let addresses = Arc::clone(&addresses);
        let routing = routing.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mode = if let Some((listener, router)) = routing {
                let local_addr = match client.local_addr() {
                    Ok(address) => address,
                    Err(error) => {
                        eprintln!("rift: {peer}: local address: {error}");
                        return;
                    }
                };
                let connection = ConnectionInfo {
                    default_backend: router.default_backend(&listener).map(str::to_owned),
                    listener,
                    peer_addr: peer,
                    local_addr,
                };
                match router.route(connection).await {
                    Ok(RouteDecision::Default) => mode,
                    Ok(RouteDecision::Backend(name)) => Arc::new(Mode::Direct(
                        router.backend(&name).expect("validated backend").clone(),
                    )),
                    Ok(RouteDecision::Reject { .. }) => return,
                    Err(error) => {
                        eprintln!("rift: {peer}: on_route: {error}");
                        return;
                    }
                }
            } else {
                mode
            };
            if let Err(error) = handle(client, &mode, &addresses, limits).await {
                eprintln!("rift: {peer}: {error}");
            }
        });
    }
}

async fn handle(
    mut client: TcpStream,
    mode: &Mode,
    listeners: &[SocketAddr],
    limits: Limits,
) -> io::Result<(u64, u64)> {
    client.set_nodelay(true)?;
    let (backend, packet) = match mode {
        Mode::Direct(backend) => (backend, Vec::new()),
        Mode::Routed(routes) => {
            let (host, packet) = timeout(HANDSHAKE_TIMEOUT, handshake::read(&mut client)).await??;
            (routes.select(&host)?, packet)
        }
    };
    let mut upstream = timeout(limits.connect_timeout, async {
        let mut upstream = backend.connect(listeners).await?;
        upstream.set_nodelay(true)?;
        upstream.write_all(&packet).await?;
        Ok::<_, io::Error>(upstream)
    })
    .await??;
    // Fixed buffers bound memory and preserve backpressure and TCP half-closes.
    let (sent, received) = copy_bidirectional_with_sizes(
        &mut client,
        &mut upstream,
        limits.buffer_size,
        limits.buffer_size,
    )
    .await?;
    Ok((sent + packet.len() as u64, received))
}

#[cfg(test)]
mod tests;
