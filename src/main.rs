use std::{env, io, net::SocketAddr, time::Duration};
use tokio::{
    io::copy_bidirectional_with_sizes,
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    time::{sleep, timeout},
};

const USAGE: &str = "Usage: rift [<listen-ip:port> <backend-ip:port>]\n\
    Defaults: 0.0.0.0:25565 127.0.0.1:25566\n\
    IPv6: rift '[::]:25565' '[::1]:25566'";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const BUFFER_SIZE: usize = 32 * 1024;
// Bound sockets, tasks and relay buffers, including while the backend is down.
static CONNECTIONS: Semaphore = Semaphore::const_new(4096);

#[tokio::main]
async fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let (listen, backend) = match args.as_slice() {
        [] => ("0.0.0.0:25565", "127.0.0.1:25566"),
        [help] if help == "--help" || help == "-h" => {
            println!("{USAGE}");
            return Ok(());
        }
        [listen, backend] => (listen.as_str(), backend.as_str()),
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE)),
    };
    let listen = parse_address(listen)?;
    let backend = parse_address(backend)?;
    if listen.port() == backend.port()
        && (listen.ip() == backend.ip()
            || (listen.ip().is_unspecified() && backend.ip().is_loopback()))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "listen and backend must not point to the same socket",
        ));
    }

    let listener = TcpListener::bind(listen).await?;
    eprintln!("rift: {} -> {backend}", listener.local_addr()?);
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
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) = relay(client, backend).await {
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

async fn relay(mut client: TcpStream, backend: SocketAddr) -> io::Result<(u64, u64)> {
    client.set_nodelay(true)?;
    let mut upstream = timeout(CONNECT_TIMEOUT, TcpStream::connect(backend)).await??;
    upstream.set_nodelay(true)?;
    // Fixed buffers bound memory and preserve backpressure and TCP half-closes.
    copy_bidirectional_with_sizes(&mut client, &mut upstream, BUFFER_SIZE, BUFFER_SIZE).await
}

#[cfg(test)]
mod tests;
