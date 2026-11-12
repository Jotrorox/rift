//! A bus login for every managed instance, so one template `config.yml`
//! works for all of them.
//!
//! Each instance of a group (or managed server) with a `messaging` block
//! authenticates as its own principal, `server.<name>`, with a token Rift
//! derives instead of stores:
//!
//! ```text
//! ri1.<name>.<generation>.<hex HMAC-SHA256(key, "rift-instance-login/1" ‖ name ‖ generation)>
//! ```
//!
//! The key is `local/instance-key` beside the configuration (32 random
//! bytes, mode 0600, created on first use). The generation increases each
//! time an instance of that name is created and is kept in the instance
//! database, so a recreated `lobby-3` gets a different token than the one
//! before it. Nothing stores tokens: the bus verifies one directly and then
//! checks that the instance is still registered at that generation. Tokens
//! survive proxy restarts, which matters because servers on nodes keep
//! running while the controller restarts.
//!
//! One shared login per group was rejected: it would let any lobby speak for
//! any other, including reporting contexts for players who are not on it.
//!
//! The key never leaves the controller. A node receives a token for each
//! server it runs in that server's start command, scoped to that one
//! instance.
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::{
    collections::HashMap,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{PoisonError, RwLock},
};
use tokio::sync::broadcast;

use crate::{config::InstanceMessaging, messaging::quic};

/// Every token starts with this, so the bus tells them from configured ones.
pub const TOKEN_PREFIX: &str = "ri1.";
/// The key file, relative to the configuration directory.
pub const KEY_FILE: &str = "local/instance-key";
const KEY_BYTES: usize = 32;
/// The certificate file Rift writes into each instance directory.
pub const CERTIFICATE_FILE: &str = "rift-bus.pem";

/// Where an instance reaches the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bus {
    /// `RIFT_BUS_ADDRESS`, `host:port`.
    pub address: String,
    /// `RIFT_BUS_SERVER_NAME`, the name the certificate is verified for.
    pub server_name: String,
    /// The messaging certificate (PEM), written to [`CERTIFICATE_FILE`].
    pub certificate: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Registered {
    generation: u64,
    group: Option<String>,
    grants: InstanceMessaging,
}

/// The logins of the registered instances. Shared by the bus (which asks
/// [`quic::DynamicIdentities::authenticate`]) and the process supervisor
/// (which hands each process its [`Logins::environment`]).
pub struct Logins {
    key: [u8; KEY_BYTES],
    bus: RwLock<Option<Bus>>,
    registered: RwLock<HashMap<String, Registered>>,
    revoked: broadcast::Sender<String>,
}

impl std::fmt::Debug for Logins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logins").finish_non_exhaustive()
    }
}

impl Logins {
    /// Logins under the key in `config_directory/local/instance-key`,
    /// created on first use.
    pub fn open(config_directory: &Path) -> io::Result<Self> {
        Ok(Self::with_key(load_key(&config_directory.join(KEY_FILE))?))
    }

    /// Logins under `key`, for tests and configurations without a file.
    pub fn with_key(key: [u8; KEY_BYTES]) -> Self {
        Self {
            key,
            bus: RwLock::new(None),
            registered: RwLock::default(),
            revoked: broadcast::channel(64).0,
        }
    }

    /// Set where instances reach the bus, once it is bound.
    pub fn set_bus(&self, bus: Bus) {
        *self.bus.write().unwrap_or_else(PoisonError::into_inner) = Some(bus);
    }

    /// The bus instances are told about, if it is set.
    pub fn bus(&self) -> Option<Bus> {
        self.bus
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The token of `name` at `generation`.
    pub fn token(&self, name: &str, generation: u64) -> String {
        let mac = self.mac(name, generation).finalize().into_bytes();
        let hex: String = mac.iter().map(|byte| format!("{byte:02x}")).collect();
        format!("{TOKEN_PREFIX}{name}.{generation}.{hex}")
    }

    fn mac(&self, name: &str, generation: u64) -> Hmac<Sha256> {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(b"rift-instance-login/1\0");
        mac.update(&(name.len() as u64).to_be_bytes());
        mac.update(name.as_bytes());
        mac.update(&generation.to_be_bytes());
        mac
    }

    /// Issue `name` its login at `generation`. A previous registration of the
    /// name at another generation is revoked.
    pub fn register(
        &self,
        name: &str,
        generation: u64,
        group: Option<&str>,
        grants: InstanceMessaging,
    ) {
        let previous = self
            .registered
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                name.to_owned(),
                Registered {
                    generation,
                    group: group.map(str::to_owned),
                    grants,
                },
            );
        if previous.is_some_and(|previous| previous.generation != generation) {
            let _ = self.revoked.send(principal(name));
        }
    }

    /// Withdraw `name`'s login and close its open connections.
    pub fn unregister(&self, name: &str) {
        let removed = self
            .registered
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name);
        if removed.is_some() {
            let _ = self.revoked.send(principal(name));
        }
    }

    /// Whether `name` currently has a login.
    pub fn is_registered(&self, name: &str) -> bool {
        self.registered
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(name)
    }

    /// The variables Rift sets for `name`'s process: `RIFT_SERVER_NAME` and
    /// `RIFT_SERVER_GROUP` always, the bus variables while it has a login.
    /// `directory` is where its process runs, for `RIFT_BUS_CERT`; a node
    /// agent sets that one itself, for its own path.
    pub fn environment(&self, name: &str, directory: Option<&Path>) -> Vec<(String, String)> {
        let registered = self
            .registered
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned();
        let mut variables = vec![("RIFT_SERVER_NAME".to_owned(), name.to_owned())];
        let Some(registered) = registered else {
            return variables;
        };
        if let Some(group) = &registered.group {
            variables.push(("RIFT_SERVER_GROUP".into(), group.clone()));
        }
        if let Some(bus) = self.bus() {
            variables.extend([
                ("RIFT_BUS_ADDRESS".to_owned(), bus.address),
                ("RIFT_BUS_SERVER_NAME".to_owned(), bus.server_name),
                (
                    "RIFT_BUS_TOKEN".to_owned(),
                    self.token(name, registered.generation),
                ),
            ]);
            if let Some(directory) = directory {
                let path = std::path::absolute(directory.join(CERTIFICATE_FILE))
                    .unwrap_or_else(|_| directory.join(CERTIFICATE_FILE));
                variables.push((
                    "RIFT_BUS_CERT".to_owned(),
                    path.to_string_lossy().into_owned(),
                ));
            }
        }
        variables
    }

    /// Write the bus certificate into an instance directory, so the process
    /// finds it at `RIFT_BUS_CERT`. Does nothing without a bus.
    pub fn write_certificate(&self, directory: &Path) -> io::Result<()> {
        let Some(bus) = self.bus() else {
            return Ok(());
        };
        let path = directory.join(CERTIFICATE_FILE);
        if std::fs::read_to_string(&path).is_ok_and(|current| current == bus.certificate) {
            return Ok(());
        }
        let temporary = directory.join(format!(".{CERTIFICATE_FILE}.tmp"));
        std::fs::write(&temporary, &bus.certificate)?;
        std::fs::rename(temporary, path)
    }

    /// Split a token into name and generation, verifying its MAC.
    fn verify(&self, token: &str) -> Option<(String, u64)> {
        let rest = token.strip_prefix(TOKEN_PREFIX)?;
        let (rest, mac) = rest.rsplit_once('.')?;
        let (name, generation) = rest.rsplit_once('.')?;
        let generation: u64 = generation.parse().ok()?;
        if name.is_empty() || mac.len() != 64 {
            return None;
        }
        let tag: Vec<u8> = (0..mac.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(mac.get(at..at + 2)?, 16).ok())
            .collect::<Option<_>>()?;
        // Constant-time comparison of the full tag.
        self.mac(name, generation).verify_slice(&tag).ok()?;
        Some((name.to_owned(), generation))
    }
}

/// The bus principal of an instance, the same as a configured backend's.
pub fn principal(name: &str) -> String {
    format!(
        "{}{name}",
        crate::permissions::control::BACKEND_PRINCIPAL_PREFIX
    )
}

impl quic::DynamicIdentities for Logins {
    fn prefix(&self) -> &str {
        TOKEN_PREFIX
    }

    fn authenticate(&self, token: &str) -> Option<quic::Identity> {
        let (name, generation) = self.verify(token)?;
        let registered = self
            .registered
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&name)
            .cloned()?;
        // A valid MAC for a removed or recreated instance is still refused.
        (registered.generation == generation).then(|| {
            let mut subscribe = registered.grants.subscribe;
            if let Some(inbox) = private_inbox(&name)
                && !subscribe.contains(&inbox)
            {
                subscribe.push(inbox);
            }
            quic::Identity {
                name: principal(&name),
                token: String::new(),
                publish: registered.grants.publish,
                subscribe,
                control: false,
                players: false,
            }
        })
    }

    fn revocations(&self) -> broadcast::Receiver<String> {
        self.revoked.subscribe()
    }
}

/// The reply namespace only `name`'s login may subscribe to:
/// `_INBOX.server.<name>.>`. Instances never get `_INBOX.>`, which would
/// expose every other identity's replies. `None` when the name is not a
/// single subject token, so one instance's namespace cannot cover another's.
pub fn private_inbox(name: &str) -> Option<String> {
    let token = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
    token.then(|| format!("_INBOX.{}.>", principal(name)))
}

/// The key at `path`, created with its directory when missing.
fn load_key(path: &Path) -> io::Result<[u8; KEY_BYTES]> {
    match read_key(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        result => return result,
    }
    if let Some(parent) = path.parent() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent)?;
    }
    let mut key = [0; KEY_BYTES];
    aws_lc_rs::rand::fill(&mut key).map_err(|_| io::Error::other("random source failed"))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(&key)?;
            file.sync_all()?;
            Ok(key)
        }
        // Another Rift created it meanwhile.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => read_key(path),
        Err(error) => Err(error),
    }
}

fn read_key(path: &Path) -> io::Result<[u8; KEY_BYTES]> {
    let bytes = std::fs::read(path)?;
    bytes.try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} must hold exactly {KEY_BYTES} bytes; delete it to issue new instance tokens",
                path.display()
            ),
        )
    })
}

/// The key path for a configuration file at `config`.
pub fn key_path(config: &Path) -> PathBuf {
    config.parent().unwrap_or(Path::new(".")).join(KEY_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use quic::DynamicIdentities;

    fn grants() -> InstanceMessaging {
        InstanceMessaging {
            publish: vec!["rift.perms.hello".into()],
            subscribe: vec![],
        }
    }

    #[test]
    fn tokens_verify_only_for_the_registered_generation() {
        let logins = Logins::with_key([7; 32]);
        let token = logins.token("lobby-3", 1);
        assert!(token.starts_with("ri1.lobby-3.1."));
        assert_eq!(token.len(), "ri1.lobby-3.1.".len() + 64);
        assert!(logins.authenticate(&token).is_none(), "not registered yet");
        logins.register("lobby-3", 1, Some("lobby"), grants());
        let identity = logins.authenticate(&token).unwrap();
        assert_eq!(identity.name, "server.lobby-3");
        assert!(!identity.control && !identity.players);
        // Replies arrive in a namespace no other instance can subscribe to.
        assert!(
            identity
                .subscribe
                .contains(&"_INBOX.server.lobby-3.>".to_owned())
        );
        assert_eq!(private_inbox("lobby.3"), None);
        // Tampering with any part breaks the MAC.
        for forged in [
            token.replace("lobby-3", "lobby-4"),
            token.replace(".1.", ".2."),
            format!(
                "{}{}",
                &token[..token.len() - 1],
                if token.ends_with('0') { '1' } else { '0' }
            ),
            Logins::with_key([8; 32]).token("lobby-3", 1),
        ] {
            assert!(logins.authenticate(&forged).is_none(), "{forged}");
        }
        // A recreated instance has a new generation and a new token.
        let mut revoked = logins.revocations();
        logins.register("lobby-3", 2, Some("lobby"), grants());
        assert_eq!(revoked.try_recv().unwrap(), "server.lobby-3");
        assert!(logins.authenticate(&token).is_none());
        let second = logins.token("lobby-3", 2);
        assert_ne!(second, token);
        assert!(logins.authenticate(&second).is_some());
        logins.unregister("lobby-3");
        assert_eq!(revoked.try_recv().unwrap(), "server.lobby-3");
        assert!(logins.authenticate(&second).is_none());
    }

    #[test]
    fn the_environment_carries_name_group_and_bus() {
        let logins = Logins::with_key([1; 32]);
        let directory = Path::new("/srv/lobby-3");
        assert_eq!(
            logins.environment("survival", Some(directory)),
            [("RIFT_SERVER_NAME".to_owned(), "survival".to_owned())]
        );
        logins.register("lobby-3", 4, Some("lobby"), grants());
        logins.set_bus(Bus {
            address: "10.0.0.2:7443".into(),
            server_name: "rift.internal".into(),
            certificate: "PEM".into(),
        });
        let environment: HashMap<_, _> = logins
            .environment("lobby-3", Some(directory))
            .into_iter()
            .collect();
        assert!(
            !logins
                .environment("lobby-3", None)
                .iter()
                .any(|(name, _)| name == "RIFT_BUS_CERT")
        );
        assert_eq!(environment["RIFT_SERVER_GROUP"], "lobby");
        assert_eq!(environment["RIFT_BUS_ADDRESS"], "10.0.0.2:7443");
        assert_eq!(environment["RIFT_BUS_SERVER_NAME"], "rift.internal");
        // Absolute in the host's own terms (Windows adds a drive).
        assert_eq!(
            Path::new(&environment["RIFT_BUS_CERT"]),
            std::path::absolute(directory.join(CERTIFICATE_FILE)).unwrap()
        );
        assert_eq!(environment["RIFT_BUS_TOKEN"], logins.token("lobby-3", 4));
    }

    #[test]
    fn the_key_is_created_once_with_mode_0600() {
        let directory =
            std::env::temp_dir().join(format!("rift-instance-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let first = Logins::open(&directory).unwrap().token("a", 1);
        let second = Logins::open(&directory).unwrap().token("a", 1);
        assert_eq!(first, second, "the key survives restarts");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(directory.join(KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(directory.join(KEY_FILE), b"short").unwrap();
        assert!(Logins::open(&directory).is_err());
        let _ = std::fs::remove_dir_all(directory);
    }
}
