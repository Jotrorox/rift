//! Durable instance registrations and filesystem intents, using bundled SQLite.
use crate::{
    config::{Config, GroupMode, GroupState, InstanceStorage, ServiceInstance},
    database::{self, Database},
    provisioning::{self, Plan},
};
use rusqlite::{OptionalExtension, params};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions as FileOptions},
    io,
    net::{SocketAddr, TcpListener},
    path::Path,
    sync::Arc,
};

/// The `rift_schema` component and version of the instance tables.
const COMPONENT: &str = "instances";
const SCHEMA_VERSION: u32 = 2;

const INSTANCES_V2: &str = "CREATE TABLE instances (
    name TEXT PRIMARY KEY NOT NULL,
    node TEXT NOT NULL DEFAULT '',
    port INTEGER NOT NULL CHECK(port BETWEEN 1 AND 65535),
    phase TEXT NOT NULL CHECK(phase IN ('creating','ready','removing','deleting')),
    explicitly_stopped INTEGER NOT NULL DEFAULT 0 CHECK(explicitly_stopped IN (0,1)),
    definition TEXT NOT NULL,
    UNIQUE(node, port)
) STRICT;";

/// Operator group state lives in its own schema component, so the
/// `instances` schema (and a downgrade to a release without group state)
/// is unaffected; older releases ignore the extra table.
const GROUPS_COMPONENT: &str = "groups";
const GROUPS_SCHEMA_VERSION: u32 = 1;
/// Instance login generations live in their own schema component, like
/// group states, so the instances schema and downgrades are unaffected.
const LOGINS_COMPONENT: &str = "instance_logins";
const LOGINS_SCHEMA_VERSION: u32 = 1;
const LOGINS_V1: &str = "CREATE TABLE login_generations (
    name TEXT PRIMARY KEY NOT NULL,
    generation INTEGER NOT NULL CHECK(generation > 0)
) STRICT;";

const GROUPS_V1: &str = "CREATE TABLE group_states (
    name TEXT PRIMARY KEY NOT NULL,
    suspended INTEGER NOT NULL DEFAULT 0 CHECK(suspended IN (0,1)),
    mode TEXT NOT NULL DEFAULT 'active' CHECK(mode IN ('active','dev'))
) STRICT;";

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

impl Record {
    /// Remote instances keep their files on their node; this host never
    /// touches, validates or reserves anything for them.
    fn remote(&self) -> bool {
        self.definition.instance.node.is_some()
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[derive(Debug)]
pub struct Store {
    database: Arc<Database>,
}

/// What [`Store::check`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseCheck {
    /// No database yet; startup can create it.
    Missing,
    /// The database opens, passes an integrity check, and its registered
    /// instances fit the configuration.
    Ready { instances: usize },
    /// Another process holds the lock, normally a running Rift. Nothing else
    /// was checked.
    InUse,
}

impl Store {
    /// Check the database read-only, without creating, locking or migrating
    /// anything (for `rift check --strict`).
    pub fn check(path: &Path, config: &Config) -> io::Result<DatabaseCheck> {
        let path = provisioning::checked_path(path)?;
        let lock_path = database::lock_path(&path);
        if fs::symlink_metadata(&lock_path).is_ok() {
            let lock = FileOptions::new().read(true).write(true).open(&lock_path)?;
            match lock.try_lock() {
                Ok(()) => {}
                Err(fs::TryLockError::WouldBlock) => return Ok(DatabaseCheck::InUse),
                Err(fs::TryLockError::Error(error)) => return Err(error),
            }
        }
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.is_file() => {
                return Err(invalid("instance database and lock must be regular files"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                crate::preflight::check_creatable(&path)?;
                return Ok(DatabaseCheck::Missing);
            }
            Err(error) => return Err(error),
        }
        let store = Self {
            database: Arc::new(Database::read_only(&path)?),
        };
        let (version, check) = {
            let connection = store.database.lock();
            let version = database::version(&connection, COMPONENT).map_err(|e| store.error(e))?;
            let check: String = connection
                .query_row("PRAGMA quick_check", [], |r| r.get(0))
                .map_err(|e| store.error(e))?;
            (version, check)
        };
        if check != "ok" {
            return Err(store.error(format!("integrity check failed: {check}")));
        }
        match version {
            // Empty, or upgraded in place at startup.
            0 | 1 => return Ok(DatabaseCheck::Ready { instances: 0 }),
            SCHEMA_VERSION => {}
            _ => return Err(store.error(format!("unsupported schema version {version}"))),
        }
        let records = store.records()?;
        let mut candidate = config.clone();
        candidate
            .restore_instances(
                records
                    .iter()
                    .filter(|r| r.phase == "ready")
                    .map(|r| (r.name.clone(), r.definition.instance.clone()))
                    .collect(),
            )
            .map_err(|e| store.error(format!("configuration conflict: {e}")))?;
        Ok(DatabaseCheck::Ready {
            instances: records.len(),
        })
    }
}

impl Store {
    /// Open the database file and the instance tables in it.
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::new(Arc::new(Database::open(path)?))
    }

    /// The instance tables in a shared database, created or upgraded as needed.
    pub fn new(database: Arc<Database>) -> io::Result<Self> {
        // Version 2 stores each instance's node ('' for this host); ports
        // are unique per node, since every node has its own port range.
        // `user_version` keeps the version too, for Rift releases that
        // predate `rift_schema`, so a downgrade still opens the file.
        database.migrate(COMPONENT, SCHEMA_VERSION, |from| match from {
            0 => format!(
                "{INSTANCES_V2}
                CREATE TABLE sequences (group_name TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL) STRICT;
                PRAGMA user_version={SCHEMA_VERSION};"
            ),
            _ => format!(
                "ALTER TABLE instances RENAME TO instances_v1;
                {INSTANCES_V2}
                INSERT INTO instances (name,node,port,phase,explicitly_stopped,definition)
                    SELECT name,'',port,phase,explicitly_stopped,definition FROM instances_v1;
                DROP TABLE instances_v1;
                PRAGMA user_version={SCHEMA_VERSION};"
            ),
        })?;
        database.migrate(GROUPS_COMPONENT, GROUPS_SCHEMA_VERSION, |_| {
            GROUPS_V1.to_owned()
        })?;
        database.migrate(LOGINS_COMPONENT, LOGINS_SCHEMA_VERSION, |_| {
            LOGINS_V1.to_owned()
        })?;
        Ok(Self { database })
    }

    /// The database this store shares with the permission store.
    pub fn database(&self) -> &Arc<Database> {
        &self.database
    }

    fn error(&self, error: impl std::fmt::Display) -> io::Error {
        self.database.error(error)
    }

    fn records(&self) -> io::Result<Vec<Record>> {
        let connection = self.database.lock();
        let mut statement = connection
            .prepare(
                "SELECT name,port,phase,explicitly_stopped,definition,node FROM instances ORDER BY name",
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
                    row.get::<_, String>(5)?,
                ))
            })
            .map_err(|e| self.error(e))?;
        rows.map(|row| {
            let (name, port, phase, stopped, json, node) = row.map_err(|e| self.error(e))?;
            let definition: Definition = serde_json::from_str(&json).map_err(|e| self.error(e))?;
            if port != definition.instance.port
                || node != definition.instance.node.clone().unwrap_or_default()
                || !["creating", "ready", "removing", "deleting"].contains(&phase.as_str())
            {
                return Err(self.error(format!("invalid registration for {name}")));
            }
            if definition.instance.node.is_some() {
                if !crate::config::valid_node_directory(&definition.plan.directory)
                    || definition.plan.stage.is_some()
                {
                    return Err(self.error(format!("instances.{name}: invalid node directory")));
                }
            } else {
                provisioning::validate_plan(&definition.instance, &name, &definition.plan)
                    .map_err(|e| self.error(format!("instances.{name}: {e}")))?;
            }
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
            if ["removing", "deleting"].contains(&record.phase.as_str()) && !record.remote() {
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
            if record.remote() {
                if record.phase == "ready" {
                    let group = &candidate.service_groups[&definition.instance.group];
                    if group.storage != definition.instance.storage {
                        return Err(self.error(format!(
                            "instances.{}: service_groups.{}.storage changed",
                            record.name, definition.instance.group
                        )));
                    }
                    if candidate.managed_servers[&record.name].directory != *directory {
                        return Err(self.error(format!(
                            "instances.{}: node directory changed (stored {})",
                            record.name,
                            directory.display()
                        )));
                    }
                }
                continue;
            }
            let mut storage_paths = vec![directory.clone()];
            if record.phase != "ready" {
                storage_paths.extend(definition.plan.stage.iter().cloned());
                if definition.instance.storage == InstanceStorage::Disposable {
                    storage_paths.push(provisioning::tombstone(&definition.plan)?);
                }
            }
            for directory in &storage_paths {
                // Stored paths must not overlap the database, templates, or another worker.
                if self.database.path().starts_with(directory)
                    || directory.starts_with(self.database.path())
                {
                    return Err(self.error(format!(
                        "instances.{}: storage overlaps database",
                        record.name
                    )));
                }
                for (name, server) in &candidate.managed_servers {
                    if server.node.is_some()
                        || (record.phase == "ready"
                            && name == &record.name
                            && directory == &definition.plan.directory)
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
                    if other.name == record.name || other.remote() {
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
        config.instances.iter().filter(|(_, instance)| instance.node.is_none()).map(|(name, instance)| {
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
        let mut connection = self.database.lock();
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
        if instance.node.is_some() {
            if !crate::config::valid_node_directory(&plan.directory) || plan.stage.is_some() {
                return Err(self.error("invalid node directory"));
            }
        } else {
            provisioning::validate_plan(instance, name, plan)?;
        }
        let definition = serde_json::to_string(&Definition {
            instance: instance.clone(),
            plan: plan.clone(),
        })
        .map_err(|e| self.error(e))?;
        let mut connection = self.database.lock();
        let transaction = connection.transaction().map_err(|e| self.error(e))?;
        transaction
            .execute(
                "INSERT INTO instances (name,node,port,phase,definition) VALUES (?1,?2,?3,'creating',?4)",
                params![
                    name,
                    instance.node.clone().unwrap_or_default(),
                    instance.port,
                    definition
                ],
            )
            .map_err(|e| self.error(e))?;
        // Every creation of a name gets a new login generation, so a
        // recreated instance never inherits its predecessor's bus token.
        transaction
            .execute(
                "INSERT INTO login_generations (name,generation) VALUES (?1,1)
                 ON CONFLICT(name) DO UPDATE SET generation=generation+1",
                [name],
            )
            .map_err(|e| self.error(e))?;
        transaction.commit().map_err(|e| self.error(e))?;
        Ok(())
    }

    /// The bus login generation of instance `name`; 0 if it was never
    /// created here (a static managed server).
    pub fn login_generation(&self, name: &str) -> io::Result<u64> {
        let connection = self.database.lock();
        let generation: Option<i64> = connection
            .query_row(
                "SELECT generation FROM login_generations WHERE name=?1",
                [name],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| self.error(e))?;
        Ok(generation.map_or(0, |generation| generation as u64))
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
            .database
            .lock()
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
        self.database
            .lock()
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
        // A node agent deletes unregistered disposable directories itself
        // when it reconciles with the controller.
        if !record.remote() {
            provisioning::rollback_plan(&record.definition.plan)?;
        }
        self.forget(name)
    }

    pub fn finish_removal(&self, name: &str) -> io::Result<()> {
        let record = self
            .records()?
            .into_iter()
            .find(|r| r.name == name && ["removing", "deleting"].contains(&r.phase.as_str()))
            .ok_or_else(|| self.error("no pending removal"))?;
        if record.definition.instance.storage == InstanceStorage::Disposable && !record.remote() {
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
        self.database
            .lock()
            .execute(
                "UPDATE instances SET explicitly_stopped=?1 WHERE name=?2 AND phase='ready'",
                params![stopped, name],
            )
            .map_err(|e| self.error(e))?;
        Ok(())
    }

    /// Every group with a state other than the default.
    pub fn group_states(&self) -> io::Result<BTreeMap<String, GroupState>> {
        let connection = self.database.lock();
        let mut statement = connection
            .prepare("SELECT name,suspended,mode FROM group_states ORDER BY name")
            .map_err(|e| self.error(e))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| self.error(e))?;
        rows.map(|row| {
            let (name, suspended, mode) = row.map_err(|e| self.error(e))?;
            let mode = GroupMode::parse(&mode)
                .ok_or_else(|| self.error(format!("invalid mode for group {name}")))?;
            Ok((name, GroupState { suspended, mode }))
        })
        .collect()
    }

    /// Record a group's state; the default state removes its row.
    pub fn set_group_state(&self, group: &str, state: GroupState) -> io::Result<()> {
        let connection = self.database.lock();
        let result = if state.is_default() {
            connection.execute("DELETE FROM group_states WHERE name=?1", [group])
        } else {
            connection.execute(
                "INSERT INTO group_states (name,suspended,mode) VALUES (?1,?2,?3)
                 ON CONFLICT(name) DO UPDATE SET suspended=excluded.suspended, mode=excluded.mode",
                params![group, state.suspended, state.mode.as_str()],
            )
        };
        result.map_err(|e| self.error(e))?;
        Ok(())
    }

    /// Ports registered on this host, including unfinished creations.
    pub fn reserved_ports(&self) -> io::Result<BTreeSet<u16>> {
        self.reserved_ports_on(None)
    }

    /// Ports registered on `node` (`None` for this host).
    pub fn reserved_ports_on(&self, node: Option<&str>) -> io::Result<BTreeSet<u16>> {
        Ok(self
            .records()?
            .into_iter()
            .filter(|r| r.definition.instance.node.as_deref() == node)
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
