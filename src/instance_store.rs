//! Durable instance registrations and filesystem intents, using bundled SQLite.
use crate::{
    config::{Config, InstanceStorage, ServiceInstance},
    provisioning::{self, Plan},
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions as FileOptions},
    io,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Definition {
    instance: ServiceInstance,
    plan: Plan,
}

#[derive(Debug)]
struct Record {
    name: String,
    phase: String,
    stopped: bool,
    definition: Definition,
}

#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    connection: Mutex<Connection>,
    // The OS releases this lock even after a crash. Keep the lock file in place.
    _lock: File,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn private_file(path: &Path) -> io::Result<File> {
    provisioning::checked_path(path)?;
    if fs::symlink_metadata(path).is_ok_and(|m| !m.is_file()) {
        return Err(invalid("instance database and lock must be regular files"));
    }
    let mut options = FileOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

impl Store {
    pub fn open(path: &Path) -> io::Result<Self> {
        let path = provisioning::checked_path(path)?;
        fs::create_dir_all(path.parent().unwrap())?;
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lock = private_file(Path::new(&lock_path))?;
        lock.try_lock().map_err(|error| {
            io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!(
                    "instance database {} is already in use or cannot be locked: {error}",
                    path.display()
                ),
            )
        })?;
        private_file(&path)?;
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| invalid(format!("instance database {}: {e}", path.display())))?;
        let store = Self {
            path,
            connection: Mutex::new(connection),
            _lock: lock,
        };
        {
            let connection = store.connection.lock().unwrap();
            connection
                .busy_timeout(Duration::from_secs(2))
                .map_err(|e| store.error(e))?;
            connection
                .execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")
                .map_err(|e| store.error(e))?;
            let version: u32 = connection
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .map_err(|e| store.error(e))?;
            match version {
                0 => connection.execute_batch("BEGIN IMMEDIATE;
                    CREATE TABLE instances (
                        name TEXT PRIMARY KEY NOT NULL,
                        port INTEGER UNIQUE NOT NULL CHECK(port BETWEEN 1 AND 65535),
                        phase TEXT NOT NULL CHECK(phase IN ('creating','ready','removing','deleting')),
                        explicitly_stopped INTEGER NOT NULL DEFAULT 0 CHECK(explicitly_stopped IN (0,1)),
                        definition TEXT NOT NULL
                    ) STRICT;
                    CREATE TABLE sequences (group_name TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL) STRICT;
                    PRAGMA user_version=1; COMMIT;")
                    .map_err(|e| store.error(e))?,
                1 => {},
                _ => return Err(store.error(format!("unsupported schema version {version}"))),
            }
            let check: String = connection
                .query_row("PRAGMA quick_check", [], |r| r.get(0))
                .map_err(|e| store.error(e))?;
            if check != "ok" {
                return Err(store.error(format!("integrity check failed: {check}")));
            }
        }
        Ok(store)
    }

    fn error(&self, error: impl std::fmt::Display) -> io::Error {
        io::Error::other(format!(
            "instance database {}: {error}",
            self.path.display()
        ))
    }

    fn records(&self) -> io::Result<Vec<Record>> {
        let connection = self.connection.lock().unwrap();
        let mut statement = connection
            .prepare(
                "SELECT name,port,phase,explicitly_stopped,definition FROM instances ORDER BY name",
            )
            .map_err(|e| self.error(e))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u16>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .map_err(|e| self.error(e))?;
        rows.map(|row| {
            let (name, port, phase, stopped, json) = row.map_err(|e| self.error(e))?;
            let definition: Definition = serde_json::from_str(&json).map_err(|e| self.error(e))?;
            if port != definition.instance.port
                || !["creating", "ready", "removing", "deleting"].contains(&phase.as_str())
            {
                return Err(self.error(format!("invalid registration for {name}")));
            }
            provisioning::validate_plan(&definition.instance, &name, &definition.plan)
                .map_err(|e| self.error(format!("instances.{name}: {e}")))?;
            Ok(Record {
                name,
                phase,
                stopped,
                definition,
            })
        })
        .collect()
    }

    /// Validate every registration before recovering intents or launching workers.
    pub fn restore(&self, config: &mut Config) -> io::Result<()> {
        let records = self.records()?;
        let mut recovery_ports = Vec::new();
        for record in &records {
            if ["removing", "deleting"].contains(&record.phase.as_str()) {
                let address = SocketAddr::from(([127, 0, 0, 1], record.definition.instance.port));
                recovery_ports.push(TcpListener::bind(address).map_err(|e| self.error(format!(
                    "instances.{}: port {address} is still in use during removal recovery: {e}; stop the old server before retrying", record.name)))?);
            }
        }
        let mut candidate = config.clone();
        candidate
            .restore_instances(
                records
                    .iter()
                    .filter(|r| r.phase == "ready")
                    .map(|r| (r.name.clone(), r.definition.instance.clone()))
                    .collect(),
            )
            .map_err(|e| self.error(format!("configuration conflict: {e}")))?;
        for record in &records {
            let definition = &record.definition;
            let directory = &definition.plan.directory;
            let mut storage_paths = vec![directory.clone()];
            if record.phase != "ready" {
                storage_paths.extend(definition.plan.stage.iter().cloned());
                if definition.instance.storage == InstanceStorage::Disposable {
                    storage_paths.push(provisioning::tombstone(&definition.plan)?);
                }
            }
            for directory in &storage_paths {
                // Stored paths must not overlap the database, templates, or another worker.
                if self.path.starts_with(directory) || directory.starts_with(&self.path) {
                    return Err(self.error(format!(
                        "instances.{}: storage overlaps database",
                        record.name
                    )));
                }
                for (name, server) in &candidate.managed_servers {
                    if record.phase == "ready"
                        && name == &record.name
                        && directory == &definition.plan.directory
                    {
                        continue;
                    }
                    let other = provisioning::checked_path(&server.directory)?;
                    if directory.starts_with(&other) || other.starts_with(directory) {
                        return Err(self.error(format!(
                            "instances.{}: storage conflicts with managed_servers.{name}",
                            record.name
                        )));
                    }
                }
                for other in &records {
                    if other.name == record.name {
                        continue;
                    }
                    let other_directory = &other.definition.plan.directory;
                    if directory.starts_with(other_directory)
                        || other_directory.starts_with(directory)
                    {
                        return Err(self.error(format!(
                            "instances.{}: storage conflicts with instances.{}",
                            record.name, other.name
                        )));
                    }
                }
                for template in candidate.templates.values() {
                    for source in std::iter::once(&template.server_jar)
                        .chain(template.plugins.iter())
                        .chain(template.configs.iter())
                        .chain(template.map.iter())
                    {
                        let source = provisioning::checked_path(source)?;
                        if directory.starts_with(&source) || source.starts_with(directory) {
                            return Err(self.error(format!(
                                "instances.{}: storage overlaps template assets",
                                record.name
                            )));
                        }
                    }
                }
            }
            if record.phase == "ready" {
                let group = &candidate.service_groups[&definition.instance.group];
                if group.storage != definition.instance.storage {
                    return Err(self.error(format!(
                        "instances.{}: service_groups.{}.storage changed",
                        record.name, definition.instance.group
                    )));
                }
                if provisioning::checked_path(&candidate.managed_servers[&record.name].directory)?
                    != *directory
                {
                    return Err(self.error(format!(
                        "instances.{}: configured directory changed (stored {})",
                        record.name,
                        directory.display()
                    )));
                }
                provisioning::validate_restored(&definition.plan)
                    .map_err(|e| self.error(format!("instances.{}: {e}", record.name)))?;
                check_configured_port(&candidate, &record.name, definition.instance.port)
                    .map_err(|e| self.error(e))?;
            }
        }
        for record in records {
            let result = match record.phase.as_str() {
                "creating" => self.abort_creation(&record.name),
                "removing" | "deleting" => self.finish_removal(&record.name),
                _ => Ok(()),
            };
            result.map_err(|e| {
                self.error(format!(
                    "recovery of instances.{} ({}): {e}",
                    record.name, record.phase
                ))
            })?;
        }
        *config = candidate;
        Ok(())
    }

    /// Keep these sockets until startup validation finishes; fail before autostart.
    pub fn reserve_restored_ports(&self, config: &Config) -> io::Result<Vec<TcpListener>> {
        config.instances.iter().map(|(name, instance)| {
            check_configured_port(config, name, instance.port)?;
            let address = SocketAddr::from(([127,0,0,1], instance.port));
            TcpListener::bind(address).map_err(|e| self.error(format!("instances.{name}: port {address} is unavailable: {e}; stop the conflicting process or correct the configuration")))
        }).collect()
    }

    pub fn stopped_names(&self) -> io::Result<BTreeSet<String>> {
        Ok(self
            .records()?
            .into_iter()
            .filter(|r| r.phase == "ready" && r.stopped)
            .map(|r| r.name)
            .collect())
    }

    pub fn allocate_name(&self, config: &Config, group: &str) -> io::Result<String> {
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction().map_err(|e| self.error(e))?;
        let current: Option<String> = transaction
            .query_row(
                "SELECT value FROM sequences WHERE group_name=?1",
                [group],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| self.error(e))?;
        let mut sequence = current
            .map(|s| s.parse::<u64>())
            .transpose()
            .map_err(|e| self.error(e))?
            .unwrap_or(0);
        // Also handle a registry imported by an embedding caller.
        for name in config.instances.keys() {
            if let Some(n) = name
                .strip_prefix(&format!("{group}-"))
                .and_then(|s| s.parse::<u64>().ok())
            {
                sequence = sequence.max(n);
            }
        }
        let name = loop {
            sequence = sequence
                .checked_add(1)
                .ok_or_else(|| self.error("instance name sequence exhausted"))?;
            let name = format!("{group}-{sequence}");
            if !config.is_destination(&name)
                && !config.managed_servers.contains_key(&name)
                && !(config.network.bungeecord
                    && config
                        .backends
                        .keys()
                        .any(|n| n.eq_ignore_ascii_case(&name)))
            {
                break name;
            }
        };
        transaction.execute("INSERT INTO sequences VALUES (?1,?2) ON CONFLICT(group_name) DO UPDATE SET value=excluded.value",
            params![group, sequence.to_string()]).map_err(|e| self.error(e))?;
        transaction.commit().map_err(|e| self.error(e))?;
        Ok(name)
    }

    pub fn begin_creation(&self, config: &Config, name: &str, plan: &Plan) -> io::Result<()> {
        let instance = config
            .instances
            .get(name)
            .ok_or_else(|| self.error("unknown instance"))?;
        provisioning::validate_plan(instance, name, plan)?;
        let definition = serde_json::to_string(&Definition {
            instance: instance.clone(),
            plan: plan.clone(),
        })
        .map_err(|e| self.error(e))?;
        self.connection
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO instances (name,port,phase,definition) VALUES (?1,?2,'creating',?3)",
                params![name, instance.port, definition],
            )
            .map_err(|e| self.error(e))?;
        Ok(())
    }

    pub fn commit_creation(&self, name: &str) -> io::Result<()> {
        self.transition(name, "creating", "ready")
    }
    pub fn undo_registration(&self, name: &str) -> io::Result<()> {
        self.transition(name, "ready", "creating")?;
        self.abort_creation(name)
    }
    pub fn begin_removal(&self, name: &str) -> io::Result<()> {
        self.transition(name, "ready", "removing")
    }
    fn transition(&self, name: &str, from: &str, to: &str) -> io::Result<()> {
        let changed = self
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE instances SET phase=?1 WHERE name=?2 AND phase=?3",
                params![to, name, from],
            )
            .map_err(|e| self.error(e))?;
        if changed != 1 {
            return Err(self.error(format!("instances.{name}: expected phase {from}")));
        }
        Ok(())
    }

    fn forget(&self, name: &str) -> io::Result<()> {
        self.connection
            .lock()
            .unwrap()
            .execute("DELETE FROM instances WHERE name=?1", [name])
            .map_err(|e| self.error(e))?;
        Ok(())
    }

    pub fn abort_creation(&self, name: &str) -> io::Result<()> {
        let record = self
            .records()?
            .into_iter()
            .find(|r| r.name == name && r.phase == "creating")
            .ok_or_else(|| self.error("no pending creation"))?;
        provisioning::rollback_plan(&record.definition.plan)?;
        self.forget(name)
    }

    pub fn finish_removal(&self, name: &str) -> io::Result<()> {
        let record = self
            .records()?
            .into_iter()
            .find(|r| r.name == name && ["removing", "deleting"].contains(&r.phase.as_str()))
            .ok_or_else(|| self.error("no pending removal"))?;
        if record.definition.instance.storage == InstanceStorage::Disposable {
            if record.phase == "removing" {
                provisioning::retire_storage(&record.definition.plan)?;
                self.transition(name, "removing", "deleting")?;
            }
            provisioning::delete_retired(&record.definition.plan)?;
        }
        self.forget(name)
    }

    pub fn set_explicit_stop(&self, name: &str, stopped: bool) -> io::Result<()> {
        // Static managed servers are outside the instance registry.
        self.connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE instances SET explicitly_stopped=?1 WHERE name=?2 AND phase='ready'",
                params![stopped, name],
            )
            .map_err(|e| self.error(e))?;
        Ok(())
    }

    pub fn reserved_ports(&self) -> io::Result<BTreeSet<u16>> {
        Ok(self
            .records()?
            .into_iter()
            .map(|r| r.definition.instance.port)
            .collect())
    }
}

#[cfg(test)]
mod tests;

pub fn check_configured_port(config: &Config, name: &str, port: u16) -> io::Result<()> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    for (other, backend) in &config.backends {
        if other != name
            && (backend.check_loop(address).is_err()
                || backend.address().rsplit_once(':').is_some_and(|(host, p)| {
                    host.trim_end_matches('.').eq_ignore_ascii_case("localhost")
                        && p.parse::<u16>().ok() == Some(port)
                }))
        {
            return Err(invalid(format!(
                "instances.{name}: port {port} conflicts with backends.{other}"
            )));
        }
    }
    for bound in config
        .listeners
        .values()
        .copied()
        .chain(config.metrics)
        .chain(config.admin.as_ref().map(|s| s.listen))
        .chain(config.web.as_ref().map(|s| s.listen))
        .chain(config.status.as_ref().map(|s| s.listen))
    {
        if bound.port() == port
            && (bound.ip().is_unspecified() || bound.ip().to_canonical() == address.ip())
        {
            return Err(invalid(format!(
                "instances.{name}: port {port} conflicts with a configured Rift listener ({bound})"
            )));
        }
    }
    Ok(())
}
