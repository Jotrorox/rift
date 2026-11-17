//! Serialized configuration transactions shared by signals and HTTP clients.
use rift::config::Config;
use std::{
    fs::{self, OpenOptions},
    hash::{Hash, Hasher},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::sync::{mpsc, oneshot};

pub const MAX_SOURCE: usize = 256 * 1024;
pub type Reply = Result<String, Error>;
pub type Sender = mpsc::Sender<Command>;

#[derive(Debug)]
pub struct Error {
    pub status: u16,
    pub message: String,
}
impl Error {
    pub fn invalid(message: impl ToString) -> Self {
        Self {
            status: 400,
            message: message.to_string(),
        }
    }
    pub fn conflict(message: impl ToString) -> Self {
        Self {
            status: 409,
            message: message.to_string(),
        }
    }
}
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::invalid(error)
    }
}

pub enum Operation {
    Reload,
    Save { source: String, revision: String },
    Rollback { source: String, revision: String },
    CreateInstance { group: String },
    RemoveInstance { name: String },
}
pub struct Command {
    pub operation: Operation,
    pub reply: oneshot::Sender<Reply>,
    pub actor: String,
}

/// Existing instances keep their process definitions and storage ownership.
/// Templates and policies can change for future provisioning; static workers
/// and listener topology still require a restart. Returns the first reason a
/// candidate cannot replace the running configuration.
pub fn live_compatible(config: &Config, previous: &Config) -> Result<(), String> {
    if config.instance_database != previous.instance_database {
        return Err("instance_database: changing the database path requires a restart".into());
    }
    if config.listeners != previous.listeners {
        return Err("gameplay listener changes require a restart".into());
    }
    if config.admin != previous.admin {
        return Err("operational admin changes require a restart".into());
    }
    if config.messaging != previous.messaging {
        return Err("messaging changes require a restart".into());
    }
    nodes_compatible(config, previous)?;
    let names = config
        .managed_servers
        .keys()
        .chain(previous.managed_servers.keys());
    for name in names {
        if config.managed_servers.get(name) == previous.managed_servers.get(name) {
            continue;
        }
        return Err(match previous.instances.get(name) {
            Some(instance) => format!(
                "service_groups.{}: changes the process definition of running instance {name}; remove the instance first or restart",
                instance.group
            ),
            None => format!("managed_servers.{name}: managed server definitions require a restart"),
        });
    }
    if config.instances != previous.instances {
        return Err("runtime instance registrations changed during reload; retry".into());
    }
    for name in previous.managed_servers.keys() {
        if config.backends.get(name) != previous.backends.get(name) {
            return Err(format!(
                "backends.{name}: managed backend addresses require a restart"
            ));
        }
    }
    for (name, instance) in &previous.instances {
        let storage = |config: &Config| {
            config
                .service_groups
                .get(&instance.group)
                .map(|group| group.storage)
        };
        if storage(config) != storage(previous) {
            return Err(format!(
                "service_groups.{}.storage: remove instance {name} before changing storage",
                instance.group
            ));
        }
    }
    if config.extensions.as_ref().map(|e| &e.queues)
        != previous.extensions.as_ref().map(|e| &e.queues)
    {
        return Err("extension queue capacity changes require a restart".into());
    }
    Ok(())
}

/// Nodes may be added, removed and changed live, except that a node keeps
/// its address while servers run on it: their backend addresses are built
/// from it. Removing such a node already fails validation.
fn nodes_compatible(config: &Config, previous: &Config) -> Result<(), String> {
    for (name, node) in &previous.nodes {
        let Some(next) = config.nodes.get(name) else {
            continue;
        };
        if next.address == node.address {
            continue;
        }
        let servers: Vec<&str> = previous
            .managed_servers
            .iter()
            .filter(|(_, server)| server.node.as_deref() == Some(name))
            .map(|(server, _)| server.as_str())
            .collect();
        if !servers.is_empty() {
            return Err(format!(
                "nodes.{name}.address: {} run on this node at {}; remove them first, or restart to change the address",
                servers.join(", "),
                node.address
            ));
        }
    }
    Ok(())
}

pub fn read_source(path: &Path) -> io::Result<String> {
    if !fs::metadata(path)?.is_file() {
        return Err(io::Error::other("configuration must be a regular file"));
    }
    let mut source = String::new();
    fs::File::open(path)?
        .take(MAX_SOURCE as u64 + 1)
        .read_to_string(&mut source)?;
    if source.len() > MAX_SOURCE {
        return Err(io::Error::other(
            "configuration exceeds 256 KiB source limit",
        ));
    }
    Ok(source)
}

pub fn revision(source: &str) -> String {
    // An opaque content revision, not an authentication credential.
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hash);
    format!("{:016x}", hash.finish())
}

pub struct Candidate {
    pub source: String,
    pub config: Config,
    pub previous_disk: String,
    pub save: bool,
}

pub fn prepare_with_instances(
    path: &Path,
    active_source: &str,
    operation: Operation,
    previous: &Config,
) -> Result<Candidate, Error> {
    let disk = read_source(path)?;
    let (source, save) = match operation {
        Operation::Reload => (disk.clone(), false),
        Operation::Save {
            source,
            revision: expected,
        }
        | Operation::Rollback {
            source,
            revision: expected,
        } => {
            if expected != revision(active_source) {
                return Err(Error::conflict(
                    "configuration changed; fetch the latest revision before saving",
                ));
            }
            if disk != active_source {
                return Err(Error::conflict(
                    "configuration file changed on disk; reload it before saving",
                ));
            }
            (source, true)
        }
        Operation::CreateInstance { .. } | Operation::RemoveInstance { .. } => {
            return Err(Error::invalid(
                "instance operations do not edit configuration files",
            ));
        }
    };
    let config = Config::from_lua_at_with_instances(&source, path, previous)?;
    Ok(Candidate {
        source,
        config,
        previous_disk: disk,
        save,
    })
}

/// Create a sibling file and atomically replace the selected configuration. A
/// failed write leaves the original untouched; preserve its access permissions.
pub fn persist(path: &Path, source: &str, expected: &str) -> Result<(), Error> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().unwrap_or(Path::new("."));
    let filename = path
        .file_name()
        .ok_or_else(|| Error::invalid("configuration has no filename"))?;
    let temp: PathBuf = parent.join(format!(
        ".{}.rift-{}-{}.tmp",
        filename.to_string_lossy(),
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let permissions = fs::metadata(path)?.permissions();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    let result = (|| {
        file.write_all(source.as_bytes())?;
        file.set_permissions(permissions)?;
        file.sync_all()?;
        if read_source(path)? != expected {
            return Err(Error::conflict(
                "configuration file changed during save; reload it before saving",
            ));
        }
        fs::rename(&temp, path)?;
        Ok(())
    })();
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(group: &str, static_command: &str) -> String {
        format!(
            "return {{listeners={{public='127.0.0.1:25565'}},backends={{static='127.0.0.1:25566'}},routes={{public='lobby'}},managed_servers={{static={{directory='servers/static',command={{{static_command}}}}}}},service_groups={{lobby={{directory='servers/{{name}}',port_range={{25600,25602}},{group}}}}}}}"
        )
    }

    #[test]
    fn live_incompatibility_names_the_blocking_setting() {
        let mut previous =
            Config::from_lua(&source("command={'java'}", "'java'"), "live.lua").unwrap();
        previous.add_instance("lobby", "lobby-1", 25600).unwrap();
        let reload = |group: &str, static_command: &str| {
            let candidate = Config::from_lua_with_instances(
                &source(group, static_command),
                "live.lua",
                &previous,
            )
            .unwrap();
            live_compatible(&candidate, &previous)
        };
        assert_eq!(reload("command={'java'}", "'java'"), Ok(()));
        let mut moved_database = previous.clone();
        moved_database.instance_database = "other.sqlite3".into();
        assert!(
            live_compatible(&moved_database, &previous)
                .unwrap_err()
                .contains("instance_database")
        );
        assert_eq!(
            reload(
                "command={'java'},scaling={capacity_per_instance=10,max_instances=3}",
                "'java'"
            ),
            Ok(()),
            "scaling policies apply live"
        );
        let error = reload("command={'java','-Xmx2G'}", "'java'").unwrap_err();
        assert!(
            error.contains("service_groups.lobby") && error.contains("lobby-1"),
            "{error}"
        );
        let error = reload("command={'java'}", "'java','-Xmx2G'").unwrap_err();
        assert!(
            error.contains("managed_servers.static") && error.contains("restart"),
            "{error}"
        );
    }

    fn nodes_source(nodes: &str) -> String {
        format!(
            "rift.config.listeners.public='127.0.0.1:25565'
rift.config.backends.lobby='127.0.0.1:25566'
rift.config.routes.public='lobby'
rift.config.messaging={{listen='0.0.0.0:7443',certificate='c.pem',private_key='k.pem'}}
{nodes}
rift.config.service_groups.game={{command={{'java'}},port_range={{30000,30009}},nodes={{'a'}}}}"
        )
    }

    #[test]
    fn nodes_change_live_but_keep_their_servers() {
        let mut previous = Config::from_lua(
            &nodes_source(
                "rift.config.nodes.a={address='10.0.0.1'}
rift.config.nodes.b={address='10.0.0.2'}",
            ),
            "nodes.lua",
        )
        .unwrap();
        previous
            .add_instance_on("game", "game-1", 30000, Some("a"))
            .unwrap();
        let reload = |nodes: &str| {
            Config::from_lua_with_instances(&nodes_source(nodes), "nodes.lua", &previous)
                .map_err(|error| error.to_string())
                .and_then(|candidate| live_compatible(&candidate, &previous))
        };
        // Add, relabel, resize, retarget and remove nodes without servers.
        assert_eq!(
            reload(
                "rift.config.nodes.a={address='10.0.0.1',labels={'games'},memory_mb=4096,max_instances=4,token_env='OTHER_A'}
rift.config.nodes.c={address='10.0.0.3'}"
            ),
            Ok(())
        );
        assert_eq!(
            reload(
                "rift.config.nodes.a={address='10.0.0.1'}
rift.config.nodes.b={address='10.0.0.20'}"
            ),
            Ok(())
        );
        // The node of game-1 keeps its address and stays configured.
        let error = reload(
            "rift.config.nodes.a={address='10.0.0.10'}
rift.config.nodes.b={address='10.0.0.2'}",
        )
        .unwrap_err();
        assert!(
            error.contains("nodes.a.address") && error.contains("game-1"),
            "{error}"
        );
        let error = Config::from_lua_with_instances(
            &nodes_source("rift.config.nodes.b={address='10.0.0.2'}")
                .replace("nodes={'a'}", "nodes={'b'}"),
            "nodes.lua",
            &previous,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("instances.game-1") && error.contains("before removing its node"),
            "{error}"
        );
    }
}
