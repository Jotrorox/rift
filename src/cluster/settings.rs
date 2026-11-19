//! `cluster.toml`: what a peer needs before it can reach the cluster.
//!
//! The shared settings (`rift.config.cluster`) are replicated with the rest
//! of `rift.lua`; these cannot be, because they say how to reach the cluster
//! in the first place. The file sits beside `rift.lua`:
//!
//! ```toml
//! peer = "eu-1"
//! listen = "0.0.0.0:7444"
//! advertise = "10.0.0.1:7444"        # what other peers dial; default: listen
//! certificate = "cluster/eu-1.pem"   # signed by the cluster CA, names advertise
//! private_key = "cluster/eu-1.key"
//! ca = "cluster/ca.pem"
//! token_env = "RIFT_CLUSTER_TOKEN"   # default
//! seeds = ["10.0.0.2:7444", "10.0.0.3:7444"]
//! public_address = "eu-1.example.com:25565"
//! witness = false
//!
//! [labels]
//! region = "eu"
//! ```
//!
//! Relative paths are relative to the directory of `rift.lua`, which also
//! holds the Raft file and the state machine.
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs, io,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::OnceLock,
};

pub const FILE: &str = "cluster.toml";
pub const DEFAULT_TOKEN_ENV: &str = "RIFT_CLUSTER_TOKEN";
/// The Raft log, vote and identity of this peer.
pub const RAFT_FILE: &str = "rift.raft.sqlite3";
/// The instance database, which holds the replicated state machine, when
/// no configuration names another (`instance_database`).
pub const DEFAULT_DATABASE: &str = "rift.sqlite3";
/// The last snapshot this peer built or installed.
pub const SNAPSHOT_DIRECTORY: &str = "rift.raft.snapshots";
/// Shorter tokens are refused: the token is the only secret that keeps a
/// holder of a CA-signed certificate from joining another cluster's links.
pub const MIN_TOKEN_BYTES: usize = 16;
const MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// This peer's name; also its node name in a cluster.
    pub peer: String,
    pub listen: SocketAddr,
    /// `host:port` other peers dial. The certificate must name its host.
    pub advertise: String,
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub ca: PathBuf,
    pub token_env: String,
    /// Other peers' addresses to contact first.
    pub seeds: Vec<String>,
    pub labels: BTreeMap<String, String>,
    /// Where players reach this peer, for transfers during a drain.
    pub public_address: Option<String>,
    /// A voter that takes no players and hands leadership away.
    pub witness: bool,
    /// The directory of `rift.lua`: Raft file, state machine, snapshots.
    pub directory: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    peer: String,
    listen: String,
    advertise: Option<String>,
    certificate: String,
    private_key: String,
    ca: String,
    token_env: Option<String>,
    #[serde(default)]
    seeds: Vec<String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    public_address: Option<String>,
    #[serde(default)]
    witness: bool,
}

impl Settings {
    /// `cluster.toml` beside the configuration file `config`.
    pub fn path_beside(config: &Path) -> PathBuf {
        directory_of(config).join(FILE)
    }

    /// The settings beside `config`, or `None` when there is no
    /// `cluster.toml`, which means this Rift is not a cluster peer.
    pub fn load_beside(config: &Path) -> io::Result<Option<Self>> {
        let path = Self::path_beside(config);
        let text = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("{}: {error}", path.display()),
                ));
            }
        };
        if text.len() > MAX_BYTES {
            return Err(super::invalid(format!(
                "{}: exceeds {} KiB",
                path.display(),
                MAX_BYTES / 1024
            )));
        }
        let text = String::from_utf8(text)
            .map_err(|_| super::invalid(format!("{}: not UTF-8", path.display())))?;
        Self::parse(&text, &directory_of(config))
            .map(Some)
            .map_err(|error| super::invalid(format!("{}: {error}", path.display())))
    }

    /// Parse `cluster.toml` text; relative paths resolve against `directory`.
    pub fn parse(text: &str, directory: &Path) -> Result<Self, String> {
        let raw: Raw = toml::from_str(text).map_err(|error| {
            let message = error.message().trim().to_owned();
            match error.span() {
                Some(span) => {
                    let line = text[..span.start.min(text.len())].lines().count().max(1);
                    format!("line {line}: {message}")
                }
                None => message,
            }
        })?;
        if !valid_peer_name(&raw.peer) {
            return Err(format!(
                "peer: {:?} must be 1..=64 lowercase letters, digits, '-' or '_', and not \"local\"",
                raw.peer
            ));
        }
        let listen: SocketAddr = raw
            .listen
            .parse()
            .map_err(|_| format!("listen: {:?} is not an ip:port", raw.listen))?;
        let advertise = match raw.advertise {
            Some(address) => {
                check_address(&address, "advertise")?;
                address
            }
            None if listen.ip().is_unspecified() => {
                return Err(
                    "advertise: required when listen is a wildcard address; set the host:port other peers dial"
                        .into(),
                );
            }
            None => listen.to_string(),
        };
        let token_env = raw
            .token_env
            .unwrap_or_else(|| DEFAULT_TOKEN_ENV.to_owned());
        let mut bytes = token_env.bytes();
        if !bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
            || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(format!(
                "token_env: {token_env:?} is not an environment variable name"
            ));
        }
        for seed in &raw.seeds {
            check_address(seed, "seeds")?;
        }
        if raw.seeds.len() > 64 {
            return Err("seeds: at most 64 addresses".into());
        }
        for (key, value) in &raw.labels {
            if key.is_empty() || key.len() > 64 || value.len() > 256 {
                return Err(format!("labels.{key}: keys 1..=64 bytes, values up to 256"));
            }
        }
        if let Some(address) = &raw.public_address {
            check_address(address, "public_address")?;
        }
        let file = |value: String, key: &str| -> Result<PathBuf, String> {
            if value.trim().is_empty() || value.contains('\0') {
                return Err(format!("{key}: expected a file path"));
            }
            Ok(directory.join(value))
        };
        Ok(Self {
            peer: raw.peer,
            listen,
            advertise,
            certificate: file(raw.certificate, "certificate")?,
            private_key: file(raw.private_key, "private_key")?,
            ca: file(raw.ca, "ca")?,
            token_env,
            seeds: raw.seeds,
            labels: raw.labels,
            public_address: raw.public_address,
            witness: raw.witness,
            directory: directory.to_path_buf(),
        })
    }

    /// The cluster token from `token_env`.
    pub fn token(&self) -> io::Result<String> {
        let token = std::env::var(&self.token_env).map_err(|_| {
            super::invalid(format!(
                "cluster: set {} to the cluster token (the same on every peer)",
                self.token_env
            ))
        })?;
        if token.len() < MIN_TOKEN_BYTES {
            return Err(super::invalid(format!(
                "cluster: {} must hold at least {MIN_TOKEN_BYTES} bytes",
                self.token_env
            )));
        }
        Ok(token)
    }

    pub fn raft_file(&self) -> PathBuf {
        self.directory.join(RAFT_FILE)
    }

    /// [`DEFAULT_DATABASE`] in this peer's directory.
    pub fn default_database(&self) -> PathBuf {
        self.directory.join(DEFAULT_DATABASE)
    }

    pub fn snapshot_directory(&self) -> PathBuf {
        self.directory.join(SNAPSHOT_DIRECTORY)
    }
}

/// The directory a configuration file is in; `.` for a bare file name.
pub fn directory_of(config: &Path) -> PathBuf {
    match config.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Peer names: lowercase so they compare the same everywhere, short enough
/// for metric labels, and never `local`, which means "this host" for nodes.
pub fn valid_peer_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "local"
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn check_address(address: &str, key: &str) -> Result<(), String> {
    let valid = address.len() <= 256
        && address.rsplit_once(':').is_some_and(|(host, port)| {
            !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p > 0)
        });
    if valid {
        Ok(())
    } else {
        Err(format!("{key}: {address:?} is not a host:port"))
    }
}

/// The host part of `host:port`, without IPv6 brackets: the TLS name a peer
/// certificate must carry.
pub fn host_of(address: &str) -> &str {
    let host = address.rsplit_once(':').map_or(address, |(host, _)| host);
    host.trim_start_matches('[').trim_end_matches(']')
}

static LOCAL_PEER: OnceLock<String> = OnceLock::new();

/// Record this process's peer name for `rift.peer`. Set once by `main` before
/// the configuration is evaluated; later calls are ignored.
pub fn set_local_peer(name: &str) {
    let _ = LOCAL_PEER.set(name.to_owned());
}

/// `rift.peer`: this process's peer name, `None` outside a cluster. Inside
/// [`with_peer`] on the same thread, the name given there.
pub fn local_peer() -> Option<&'static str> {
    PEER_OVERRIDE
        .with(std::cell::Cell::get)
        .or_else(|| LOCAL_PEER.get().map(String::as_str))
}

thread_local! {
    static PEER_OVERRIDE: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

/// Peer names [`with_peer`] has used, kept for the life of the process: at
/// most one per member name a cluster ever had.
static PEER_NAMES: std::sync::Mutex<std::collections::BTreeSet<&'static str>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Run `work` on this thread with `rift.peer` set to `name`: how a peer
/// evaluates a configuration the way another peer would (docs/clustering.md,
/// "One configuration").
pub fn with_peer<T>(name: &str, work: impl FnOnce() -> T) -> T {
    let name: &'static str = {
        let mut names = PEER_NAMES.lock().unwrap_or_else(|e| e.into_inner());
        match names.get(name) {
            Some(known) => *known,
            None => {
                let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
                names.insert(leaked);
                leaked
            }
        }
    };
    struct Restore(Option<&'static str>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0;
            PEER_OVERRIDE.with(|cell| cell.set(previous));
        }
    }
    let _restore = Restore(PEER_OVERRIDE.with(|cell| cell.replace(Some(name))));
    work()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        peer = "eu-1"
        listen = "0.0.0.0:7444"
        advertise = "10.0.0.1:7444"
        certificate = "cluster/eu-1.pem"
        private_key = "cluster/eu-1.key"
        ca = "cluster/ca.pem"
        seeds = ["10.0.0.2:7444", "eu-3.internal:7444"]
        [labels]
        region = "eu"
    "#;

    #[test]
    fn parses_a_peer_file_relative_to_the_configuration() {
        let settings = Settings::parse(MINIMAL, Path::new("/srv/rift")).unwrap();
        assert_eq!(settings.peer, "eu-1");
        assert_eq!(settings.advertise, "10.0.0.1:7444");
        assert_eq!(
            settings.certificate,
            Path::new("/srv/rift/cluster/eu-1.pem")
        );
        assert_eq!(settings.token_env, DEFAULT_TOKEN_ENV);
        assert_eq!(settings.seeds.len(), 2);
        assert_eq!(settings.labels["region"], "eu");
        assert!(!settings.witness);
        assert_eq!(
            settings.raft_file(),
            Path::new("/srv/rift/rift.raft.sqlite3")
        );
    }

    #[test]
    fn refuses_unknown_fields_bad_names_and_missing_advertise() {
        let unknown = format!("{MINIMAL}\nextra = 1");
        assert!(Settings::parse(&unknown, Path::new(".")).is_err());
        let local = MINIMAL.replace("\"eu-1\"", "\"local\"");
        assert!(Settings::parse(&local, Path::new(".")).is_err());
        let upper = MINIMAL.replace("\"eu-1\"", "\"EU-1\"");
        assert!(Settings::parse(&upper, Path::new(".")).is_err());
        let wildcard = MINIMAL.replace("advertise = \"10.0.0.1:7444\"", "");
        let error = Settings::parse(&wildcard, Path::new(".")).unwrap_err();
        assert!(error.contains("advertise"), "{error}");
        let seed = MINIMAL.replace("eu-3.internal:7444", "eu-3.internal");
        assert!(Settings::parse(&seed, Path::new(".")).is_err());
    }

    #[test]
    fn host_of_strips_ports_and_brackets() {
        assert_eq!(host_of("10.0.0.1:7444"), "10.0.0.1");
        assert_eq!(host_of("[::1]:7444"), "::1");
        assert_eq!(host_of("eu-1.internal:7444"), "eu-1.internal");
    }
}
