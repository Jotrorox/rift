mod handshake;
mod routing;

use routing::{Backend, Routes};
use std::{env, io, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncWriteExt, copy_bidirectional_with_sizes},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    time::{sleep, timeout},
};

const USAGE: &str = "Usage: rift [<listen-ip:port> <backend-ip:port>]\n\
    Backend may also be a DNS hostname with a port.\n\
    Routing: rift <listen-ip:port> [--route <hostname=backend:port>]... [--default <backend:port>]\n\
    Routes: exact hostname, '*.example.com', or '*' (default).\n\
    Priority: exact, longest wildcard suffix, default. Unmatched clients close.\n\
    Defaults: 0.0.0.0:25565 127.0.0.1:25566\n\
    IPv6: rift '[::]:25565' '[::1]:25566'";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const BUFFER_SIZE: usize = 32 * 1024;
// Bound sockets, tasks and relay buffers, including while the backend is down.
static CONNECTIONS: Semaphore = Semaphore::const_new(4096);

enum Mode {
    Direct(Backend),
    Routed(Routes),
}

fn configuration(args: &[String]) -> io::Result<(SocketAddr, Mode)> {
    match args {
        [] => Ok((
            parse_address("0.0.0.0:25565")?,
            Mode::Direct(Backend::parse("127.0.0.1:25566")?),
        )),
        [listen, backend] => Ok((
            parse_address(listen)?,
            Mode::Direct(Backend::parse(backend)?),
        )),
        [listen, options @ ..] if !options.is_empty() && options.len().is_multiple_of(2) => {
            let listen = parse_address(listen)?;
            let mut routes = Routes::default();
            for option in options.as_chunks::<2>().0 {
                match option[0].as_str() {
                    "--route" => routes.add(&option[1])?,
                    "--default" => routes.set_default(Backend::parse(&option[1])?)?,
                    _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE)),
                }
            }
            Ok((listen, Mode::Routed(routes)))
        }
        _ => Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE)),
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if matches!(args.as_slice(), [help] if help == "--help" || help == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let (listen, mode) = configuration(&args)?;
    match &mode {
        Mode::Direct(backend) => backend.check_loop(listen)?,
        Mode::Routed(routes) => routes.check_loops(listen)?,
    }
    let mode = Arc::new(mode);
    let listener = TcpListener::bind(listen).await?;
    let listen = listener.local_addr()?;
    eprintln!("rift: listening on {listen}");
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
        let Ok(permit) = CONNECTIONS.try_acquire() else {
            // Reject overload immediately instead of queuing unbounded work.
            continue;
        };
        let mode = Arc::clone(&mode);
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) = handle(client, &mode, listen).await {
                eprintln!("rift: {peer}: {error}");
            }
        });
    }
}

fn parse_address(value: &str) -> io::Result<SocketAddr> {
    value
        .parse()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, format!("{value}: {error}")))
}

async fn handle(mut client: TcpStream, mode: &Mode, listen: SocketAddr) -> io::Result<(u64, u64)> {
    client.set_nodelay(true)?;
    let (backend, packet) = match mode {
        Mode::Direct(backend) => (backend, Vec::new()),
        Mode::Routed(routes) => {
            let (host, packet) = timeout(HANDSHAKE_TIMEOUT, handshake::read(&mut client)).await??;
            (routes.select(&host)?, packet)
        }
    };
    let mut upstream = timeout(CONNECT_TIMEOUT, async {
        let mut upstream = backend.connect(listen).await?;
        upstream.set_nodelay(true)?;
        upstream.write_all(&packet).await?;
        Ok::<_, io::Error>(upstream)
    })
    .await??;
    let (sent, received) = relay(&mut client, &mut upstream).await?;
    Ok((sent + packet.len() as u64, received))
}

async fn relay(client: &mut TcpStream, upstream: &mut TcpStream) -> io::Result<(u64, u64)> {
    client.set_nodelay(true)?;
    upstream.set_nodelay(true)?;
    // Fixed buffers bound memory and preserve backpressure and TCP half-closes.
    copy_bidirectional_with_sizes(client, upstream, BUFFER_SIZE, BUFFER_SIZE).await
}

#[cfg(test)]
mod tests;
