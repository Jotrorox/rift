use std::{collections::HashMap, io, net::SocketAddr};
use tokio::{
    net::{TcpStream, lookup_host},
    time::{Instant, timeout_at},
};

/// The phase in which backend establishment failed, including timeouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectStage {
    Dns,
    Connect,
}

#[derive(Debug)]
pub struct ConnectError {
    pub stage: ConnectStage,
    pub error: io::Error,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for ConnectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

pub enum Mode {
    Direct(Backend),
    Routed(Routes),
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn dns_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backend(String);

impl std::str::FromStr for Backend {
    type Err = io::Error;

    fn from_str(value: &str) -> io::Result<Self> {
        Self::parse(value)
    }
}

impl Backend {
    pub fn parse(value: &str) -> io::Result<Self> {
        if value.parse::<SocketAddr>().is_err() {
            let (host, port) = value
                .rsplit_once(':')
                .ok_or_else(|| invalid("backend requires host:port"))?;
            if !dns_name(host.strip_suffix('.').unwrap_or(host)) || port.parse::<u16>().is_err() {
                return Err(invalid(format!("invalid backend: {value}")));
            }
        }
        Ok(Self(value.to_owned()))
    }

    pub fn check_loop(&self, listen: SocketAddr) -> io::Result<()> {
        if let Ok(address) = self.0.parse() {
            check_loop(listen, address)?;
        }
        Ok(())
    }

    // Resolution and every connection attempt share the caller's deadline.
    // Resolve per connection so DNS changes do not require a proxy restart.
    pub async fn connect(&self, listeners: &[SocketAddr]) -> io::Result<TcpStream> {
        connect_addresses(self.resolve().await?, listeners).await
    }

    /// Resolution and TCP attempts share a deadline, with phase retained even
    /// when a pending future times out.
    pub async fn connect_until(
        &self,
        listeners: &[SocketAddr],
        deadline: Instant,
    ) -> Result<TcpStream, ConnectError> {
        let addresses = timeout_at(deadline, self.resolve())
            .await
            .map_err(io::Error::from)
            .and_then(|result| result)
            .map_err(|error| ConnectError {
                stage: ConnectStage::Dns,
                error,
            })?;
        timeout_at(deadline, connect_addresses(addresses, listeners))
            .await
            .map_err(io::Error::from)
            .and_then(|result| result)
            .map_err(|error| ConnectError {
                stage: ConnectStage::Connect,
                error,
            })
    }

    async fn resolve(&self) -> io::Result<Vec<SocketAddr>> {
        let addresses: Vec<_> = lookup_host(self.0.as_str()).await?.collect();
        if addresses.is_empty() {
            return Err(invalid("backend DNS returned no addresses"));
        }
        Ok(addresses)
    }

    pub fn address(&self) -> &str {
        &self.0
    }
}

async fn connect_addresses(
    addresses: Vec<SocketAddr>,
    listeners: &[SocketAddr],
) -> io::Result<TcpStream> {
    let mut last_error = invalid("backend DNS returned no addresses");
    for address in addresses {
        if let Err(error) = listeners
            .iter()
            .try_for_each(|listen| check_loop(*listen, address))
        {
            last_error = error;
            continue;
        }
        match TcpStream::connect(address).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

fn check_loop(listen: SocketAddr, backend: SocketAddr) -> io::Result<()> {
    if listen.port() != 0
        && listen.port() == backend.port()
        && (listen.ip().to_canonical() == backend.ip().to_canonical()
            || (listen.ip().is_unspecified() && backend.ip().to_canonical().is_loopback()))
    {
        return Err(invalid(
            "listen and backend must not point to the same socket",
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub struct Routes<T = Backend> {
    exact: HashMap<String, T>,
    wildcard: Vec<(String, T)>,
    default: Option<T>,
}

impl Routes {
    pub fn add(&mut self, value: &str) -> io::Result<()> {
        let (pattern, target) = value
            .split_once('=')
            .ok_or_else(|| invalid("route requires hostname=backend:port"))?;
        let backend = Backend::parse(target)?;
        self.add_pattern(pattern, backend)
    }
}

impl<T> Default for Routes<T> {
    fn default() -> Self {
        Self {
            exact: HashMap::new(),
            wildcard: Vec::new(),
            default: None,
        }
    }
}

impl<T> Routes<T> {
    pub fn add_pattern(&mut self, pattern: &str, backend: T) -> io::Result<()> {
        if pattern == "*" {
            return self.set_default(backend);
        }
        let pattern = pattern
            .strip_suffix('.')
            .unwrap_or(pattern)
            .to_ascii_lowercase();
        let name = pattern.strip_prefix("*.").unwrap_or(&pattern);
        if !dns_name(name) {
            return Err(invalid(format!("invalid route hostname: {pattern}")));
        }
        if pattern.starts_with("*.") {
            let suffix = format!(".{name}");
            if self.wildcard.iter().any(|(key, _)| key == &suffix) {
                return Err(invalid(format!("duplicate route: {pattern}")));
            }
            self.wildcard.push((suffix, backend));
            self.wildcard
                .sort_by_key(|(key, _)| std::cmp::Reverse(key.len()));
        } else if self.exact.insert(pattern.clone(), backend).is_some() {
            return Err(invalid(format!("duplicate route: {pattern}")));
        }
        Ok(())
    }

    pub fn set_default(&mut self, backend: T) -> io::Result<()> {
        if self.default.is_some() {
            return Err(invalid("duplicate default route"));
        }
        self.default = Some(backend);
        Ok(())
    }

    pub fn select(&self, host: &str) -> io::Result<&T> {
        self.exact
            .get(host)
            .or_else(|| {
                self.wildcard.iter().find_map(|(suffix, backend)| {
                    (host.len() > suffix.len() && host.ends_with(suffix)).then_some(backend)
                })
            })
            .or(self.default.as_ref())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "no route for handshake hostname")
            })
    }
}

impl Routes {
    pub fn check_loops(&self, listen: SocketAddr) -> io::Result<()> {
        for backend in self
            .exact
            .values()
            .chain(self.wildcard.iter().map(|(_, backend)| backend))
            .chain(self.default.iter())
        {
            backend.check_loop(listen)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapped_loopback_cannot_route_back_into_a_local_service() {
        for (listen, backend) in [
            ("127.0.0.1:8080", "[::ffff:127.0.0.1]:8080"),
            ("[::ffff:127.0.0.1]:8080", "127.0.0.1:8080"),
            ("[::]:8080", "[::ffff:127.0.0.1]:8080"),
        ] {
            assert!(check_loop(listen.parse().unwrap(), backend.parse().unwrap()).is_err());
        }
    }

    #[test]
    fn dns_timeout_retains_the_resolution_stage() {
        use std::{sync::mpsc, time::Duration};
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let (ready, started) = mpsc::channel();
        let (release, held) = mpsc::channel();
        let blocker = runtime.spawn_blocking(move || {
            ready.send(()).unwrap();
            held.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        runtime.block_on(async {
            // Hostname resolution needs the occupied blocking worker. No
            // external DNS server or slow/unroutable network is required.
            let error = Backend::parse("localhost:1")
                .unwrap()
                .connect_until(&[], Instant::now() + Duration::from_millis(20))
                .await
                .unwrap_err();
            assert_eq!(error.stage, ConnectStage::Dns);
            assert_eq!(error.error.kind(), io::ErrorKind::TimedOut);
            release.send(()).unwrap();
            blocker.await.unwrap();
        });
    }

    #[test]
    fn precedence_and_label_boundaries_are_independent_of_order() {
        for reverse in [false, true] {
            let mut entries = vec![
                "*.example.com=wild:1",
                "*.sub.example.com=specific:2",
                "PLAY.SUB.EXAMPLE.COM.=exact:3",
                "*=default:4",
            ];
            if reverse {
                entries.reverse();
            }
            let mut routes = Routes::default();
            for entry in entries {
                routes.add(entry).unwrap();
            }
            for (host, expected) in [
                ("play.sub.example.com", "exact:3"),
                ("other.sub.example.com", "specific:2"),
                ("a.b.example.com", "wild:1"),
                ("sub.example.com", "wild:1"),
                ("example.com", "default:4"),
                ("badexample.com", "default:4"),
                ("elsewhere.test", "default:4"),
            ] {
                assert_eq!(routes.select(host).unwrap().0, expected);
            }
        }
        assert!(
            Routes::<Backend>::default()
                .select("unmatched.test")
                .is_err()
        );
    }

    #[test]
    fn rejects_invalid_and_duplicate_routes() {
        for entry in [
            "example.com",
            "=host:1",
            "foo.*.com=host:1",
            "*example.com=host:1",
            "*.=host:1",
            "a..b=host:1",
            "a=host",
            "a=host:65536",
            "a=::1:25565",
            "a=host:1=extra",
        ] {
            assert!(Routes::default().add(entry).is_err(), "{entry}");
        }
        for entries in [
            ["EXAMPLE.COM=host:1", "example.com.=other:2"],
            ["*.EXAMPLE.COM=host:1", "*.example.com.=other:2"],
            ["*=host:1", "*=other:2"],
        ] {
            let mut routes = Routes::default();
            routes.add(entries[0]).unwrap();
            assert!(routes.add(entries[1]).is_err());
        }
    }

    #[tokio::test]
    async fn dns_loop_is_rejected_before_connecting() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen = listener.local_addr().unwrap();
        let backend = Backend::parse(&format!("localhost:{}", listen.port())).unwrap();
        assert!(backend.connect(&[listen]).await.is_err());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }
}
