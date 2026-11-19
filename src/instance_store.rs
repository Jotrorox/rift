//! Durable instance registrations and filesystem intents, kept in the
//! instance database (SQLite, PostgreSQL or MariaDB).
//!
//! Every write is an [`InstanceOp`] run through [`Database::run`] and
//! performed by [`execute`]. Group states and node schedules reach memory
//! only through the apply hooks [`Store::watch_states`] registers.
use crate::{
    config::{Config, GroupMode, GroupState, MovedServer, NodeSchedule, ServiceInstance},
    database::{
        self, Conn, Database, Dialect, HookId, Location, OptionalExtension, Row,
        operation::{Operation, OrMessage, Outcome},
        params,
    },
    provisioning::{self, Plan},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions as FileOptions},
    io,
    net::{SocketAddr, TcpListener},
    path::Path,
    sync::{Arc, Mutex},
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

const INSTANCES_POSTGRES: &str = r#"CREATE TABLE instances (
    name TEXT COLLATE "C" PRIMARY KEY NOT NULL,
    node TEXT COLLATE "C" NOT NULL DEFAULT '',
    port BIGINT NOT NULL CHECK(port BETWEEN 1 AND 65535),
    phase TEXT COLLATE "C" NOT NULL CHECK(phase IN ('creating','ready','removing','deleting')),
    explicitly_stopped BIGINT NOT NULL DEFAULT 0 CHECK(explicitly_stopped IN (0,1)),
    definition TEXT COLLATE "C" NOT NULL,
    UNIQUE(node, port)
);
CREATE TABLE sequences (
    group_name TEXT COLLATE "C" PRIMARY KEY NOT NULL,
    value TEXT COLLATE "C" NOT NULL
);"#;

/// MariaDB keys text by `VARCHAR` (names are at most 107 bytes) and needs
/// `LONGTEXT` for definitions past 64 KiB.
const INSTANCES_MARIADB: &str = "CREATE TABLE instances (
    name VARCHAR(255) PRIMARY KEY NOT NULL,
    node VARCHAR(255) NOT NULL DEFAULT '',
    port BIGINT NOT NULL CHECK(port BETWEEN 1 AND 65535),
    phase VARCHAR(16) NOT NULL CHECK(phase IN ('creating','ready','removing','deleting')),
    explicitly_stopped BIGINT NOT NULL DEFAULT 0 CHECK(explicitly_stopped IN (0,1)),
    definition LONGTEXT NOT NULL,
    UNIQUE(node, port)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_nopad_bin;
CREATE TABLE sequences (
    group_name VARCHAR(255) PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_nopad_bin;";

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
const LOGINS_POSTGRES: &str = r#"CREATE TABLE login_generations (
    name TEXT COLLATE "C" PRIMARY KEY NOT NULL,
    generation BIGINT NOT NULL CHECK(generation > 0)
);"#;
const LOGINS_MARIADB: &str = "CREATE TABLE login_generations (
    name VARCHAR(255) PRIMARY KEY NOT NULL,
    generation BIGINT NOT NULL CHECK(generation > 0)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_nopad_bin;";

/// Node schedules (cordon and drain) live in their own component too.
const NODES_COMPONENT: &str = "node_schedules";
const NODES_SCHEMA_VERSION: u32 = 1;
const NODES_V1: &str = "CREATE TABLE node_schedules (
    name TEXT PRIMARY KEY NOT NULL,
    schedule TEXT NOT NULL CHECK(schedule IN ('cordoned','draining'))
) STRICT;";
const NODES_POSTGRES: &str = r#"CREATE TABLE node_schedules (
    name TEXT COLLATE "C" PRIMARY KEY NOT NULL,
    schedule TEXT COLLATE "C" NOT NULL CHECK(schedule IN ('cordoned','draining'))
);"#;
const NODES_MARIADB: &str = "CREATE TABLE node_schedules (
    name VARCHAR(255) PRIMARY KEY NOT NULL,
    schedule VARCHAR(16) NOT NULL CHECK(schedule IN ('cordoned','draining'))
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_nopad_bin;";

/// Where `rift admin move` put servers of `managed_servers`, in a component
/// of its own; older releases ignore the table and run the servers where the
/// configuration says.
const MOVED_COMPONENT: &str = "moved_servers";
const MOVED_SCHEMA_VERSION: u32 = 1;
const MOVED_V1: &str = "CREATE TABLE moved_servers (
    name TEXT PRIMARY KEY NOT NULL,
    configured_node TEXT NOT NULL,
    configured_backend TEXT NOT NULL,
    node TEXT NOT NULL,
    port INTEGER NOT NULL CHECK(port BETWEEN 1 AND 65535)
) STRICT;";
const MOVED_POSTGRES: &str = r#"CREATE TABLE moved_servers (
    name TEXT COLLATE "C" PRIMARY KEY NOT NULL,
    configured_node TEXT COLLATE "C" NOT NULL,
    configured_backend TEXT COLLATE "C" NOT NULL,
    node TEXT COLLATE "C" NOT NULL,
    port BIGINT NOT NULL CHECK(port BETWEEN 1 AND 65535)
);"#;
const MOVED_MARIADB: &str = "CREATE TABLE moved_servers (
    name VARCHAR(255) PRIMARY KEY NOT NULL,
    configured_node VARCHAR(255) NOT NULL,
    configured_backend TEXT NOT NULL,
    node VARCHAR(255) NOT NULL,
    port BIGINT NOT NULL CHECK(port BETWEEN 1 AND 65535)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_nopad_bin;";

/// The runtime state of each managed server as a cluster leader last
/// recorded it (docs/clustering.md, "Inputs from other peers"), in a
/// component of its own. Only a cluster leader writes it: every peer routes
/// from the same view, and a new leader starts from the old one's. A single
/// Rift keeps the (empty) table and never reads it.
const SERVERS_COMPONENT: &str = "server_states";
const SERVERS_SCHEMA_VERSION: u32 = 1;
const SERVERS_V1: &str = "CREATE TABLE server_states (
    name TEXT PRIMARY KEY NOT NULL,
    phase TEXT NOT NULL,
    last_error TEXT,
    automatic INTEGER NOT NULL DEFAULT 1 CHECK(automatic IN (0,1)),
    restart_attempts INTEGER NOT NULL DEFAULT 0
) STRICT;";
const SERVERS_POSTGRES: &str = r#"CREATE TABLE server_states (
    name TEXT COLLATE "C" PRIMARY KEY NOT NULL,
    phase TEXT COLLATE "C" NOT NULL,
    last_error TEXT,
    automatic BIGINT NOT NULL DEFAULT 1 CHECK(automatic IN (0,1)),
    restart_attempts BIGINT NOT NULL DEFAULT 0
);"#;
const SERVERS_MARIADB: &str = "CREATE TABLE server_states (
    name VARCHAR(255) PRIMARY KEY NOT NULL,
    phase VARCHAR(16) NOT NULL,
    last_error TEXT,
    automatic BIGINT NOT NULL DEFAULT 1 CHECK(automatic IN (0,1)),
    restart_attempts BIGINT NOT NULL DEFAULT 0
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_nopad_bin;";

const GROUPS_V1: &str = "CREATE TABLE group_states (
    name TEXT PRIMARY KEY NOT NULL,
    suspended INTEGER NOT NULL DEFAULT 0 CHECK(suspended IN (0,1)),
    mode TEXT NOT NULL DEFAULT 'active' CHECK(mode IN ('active','dev'))
) STRICT;";
const GROUPS_POSTGRES: &str = r#"CREATE TABLE group_states (
    name TEXT COLLATE "C" PRIMARY KEY NOT NULL,
    suspended BIGINT NOT NULL DEFAULT 0 CHECK(suspended IN (0,1)),
    mode TEXT COLLATE "C" NOT NULL DEFAULT 'active' CHECK(mode IN ('active','dev'))
);"#;
const GROUPS_MARIADB: &str = "CREATE TABLE group_states (
    name VARCHAR(255) PRIMARY KEY NOT NULL,
    suspended BIGINT NOT NULL DEFAULT 0 CHECK(suspended IN (0,1)),
    mode VARCHAR(16) NOT NULL DEFAULT 'active' CHECK(mode IN ('active','dev'))
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_nopad_bin;";

/// The DDL of a component for `dialect`: `[sqlite, postgres, mariadb]`.
fn ddl(dialect: Dialect, [sqlite, postgres, mariadb]: [&str; 3]) -> String {
    match dialect {
        Dialect::Sqlite => sqlite,
        Dialect::Postgres => postgres,
        Dialect::Mariadb => mariadb,
    }
    .to_owned()
}

/// The SQL that brings the `instances` component from version `from` to
/// [`SCHEMA_VERSION`]. Version 2 stores each instance's node ('' for this
/// host); ports are unique per node, since every node has its own port
/// range. `user_version` keeps the version too, for Rift releases that
/// predate `rift_schema`, so a downgrade still opens the file. Server
/// databases always start at the latest version.
fn instances_sql(dialect: Dialect, from: u32) -> String {
    match (dialect, from) {
        (Dialect::Postgres, _) => INSTANCES_POSTGRES.to_owned(),
        (Dialect::Mariadb, _) => INSTANCES_MARIADB.to_owned(),
        (Dialect::Sqlite, 0) => format!(
            "{INSTANCES_V2}
            CREATE TABLE sequences (group_name TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL) STRICT;
            PRAGMA user_version={SCHEMA_VERSION};"
        ),
        (Dialect::Sqlite, _) => format!(
            "ALTER TABLE instances RENAME TO instances_v1;
            {INSTANCES_V2}
            INSERT INTO instances (name,node,port,phase,explicitly_stopped,definition)
                SELECT name,'',port,phase,explicitly_stopped,definition FROM instances_v1;
            DROP TABLE instances_v1;
            PRAGMA user_version={SCHEMA_VERSION};"
        ),
    }
}

fn groups_sql(dialect: Dialect, _from: u32) -> String {
    ddl(dialect, [GROUPS_V1, GROUPS_POSTGRES, GROUPS_MARIADB])
}

fn logins_sql(dialect: Dialect, _from: u32) -> String {
    ddl(dialect, [LOGINS_V1, LOGINS_POSTGRES, LOGINS_MARIADB])
}

fn nodes_sql(dialect: Dialect, _from: u32) -> String {
    ddl(dialect, [NODES_V1, NODES_POSTGRES, NODES_MARIADB])
}

fn moved_sql(dialect: Dialect, _from: u32) -> String {
    ddl(dialect, [MOVED_V1, MOVED_POSTGRES, MOVED_MARIADB])
}

fn servers_sql(dialect: Dialect, _from: u32) -> String {
    ddl(dialect, [SERVERS_V1, SERVERS_POSTGRES, SERVERS_MARIADB])
}

type Upgrade = fn(Dialect, u32) -> String;

/// The schema components of this module, their latest versions and the SQL
/// that creates or upgrades them, in the order startup migrates them.
const COMPONENTS: [(&str, u32, Upgrade); 6] = [
    (COMPONENT, SCHEMA_VERSION, instances_sql),
    (GROUPS_COMPONENT, GROUPS_SCHEMA_VERSION, groups_sql),
    (LOGINS_COMPONENT, LOGINS_SCHEMA_VERSION, logins_sql),
    (NODES_COMPONENT, NODES_SCHEMA_VERSION, nodes_sql),
    (MOVED_COMPONENT, MOVED_SCHEMA_VERSION, moved_sql),
    (SERVERS_COMPONENT, SERVERS_SCHEMA_VERSION, servers_sql),
];

/// Every component of this module and the version this Rift writes.
pub fn schema_versions() -> Vec<(&'static str, u32)> {
    COMPONENTS
        .iter()
        .map(|(component, version, _)| (*component, *version))
        .collect()
}

const INSTANCE_TABLES: &[&str] = &["instances", "sequences"];
const SEQUENCES: &[&str] = &["sequences"];
const INSTANCES: &[&str] = &["instances"];
const CREATION: &[&str] = &["instances", "login_generations"];
const GROUP_STATES: &[&str] = &["group_states"];
const LOGIN_GENERATIONS: &[&str] = &["login_generations"];
const NODE_SCHEDULES: &[&str] = &["node_schedules"];
const MOVED_SERVERS: &[&str] = &["moved_servers"];
const SERVER_STATES: &[&str] = &["server_states"];

/// The tables of `component`, if it belongs to this module.
pub fn component_tables(component: &str) -> Option<&'static [&'static str]> {
    match component {
        COMPONENT => Some(INSTANCE_TABLES),
        GROUPS_COMPONENT => Some(GROUP_STATES),
        LOGINS_COMPONENT => Some(LOGIN_GENERATIONS),
        NODES_COMPONENT => Some(NODE_SCHEDULES),
        MOVED_COMPONENT => Some(MOVED_SERVERS),
        SERVERS_COMPONENT => Some(SERVER_STATES),
        _ => None,
    }
}

/// An instance `rift cluster init` moved from this host to its peer's node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertedInstance {
    pub name: String,
    /// The absolute directory the instance used on this host.
    pub from: std::path::PathBuf,
    /// The directory the node uses, relative to its `--root`.
    pub to: std::path::PathBuf,
}

/// A managed server's runtime state as a cluster leader records it in
/// `server_states`: what its worker shows (`phase` is a
/// [`crate::managed::Phase`] name), whether automatic starts are allowed,
/// and its recovery budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerState {
    pub phase: String,
    pub last_error: Option<String>,
    pub automatic: bool,
    pub restart_attempts: u32,
}

/// Migrate `component` to `to_version` in the caller's transaction (see
/// [`database::migrate_component`]); `None` when the component is not one of
/// this module's.
pub fn migrate(
    connection: &Conn,
    component: &str,
    to_version: u32,
) -> Option<Result<bool, String>> {
    let (name, latest, sql) = COMPONENTS.iter().find(|(name, _, _)| *name == component)?;
    Some(database::migrate_component(connection, name, to_version, *latest, sql).or_message())
}

/// A write to the instance registry, group states, node schedules or the
/// moves of named servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum InstanceOp {
    /// Advance the group's durable name sequence past `floor` (the highest
    /// number among registered instances) and every name in `taken` (or, ASCII
    /// case ignored, in `taken_nocase`) that the configuration already uses.
    /// The outcome is the new name.
    AllocateName {
        group: String,
        floor: u64,
        taken: Vec<String>,
        taken_nocase: Vec<String>,
    },
    /// Register an instance in phase `creating` and give its name a new
    /// login generation. `definition` is the stored JSON.
    BeginCreation {
        name: String,
        node: String,
        port: u16,
        definition: String,
    },
    /// Move a registration from phase `from` to `to`.
    Transition {
        name: String,
        from: String,
        to: String,
    },
    /// Delete a registration (its login generation stays).
    Forget {
        name: String,
    },
    /// Re-register a ready instance on another node, if it is still on `from`.
    Move {
        name: String,
        from: String,
        node: String,
        port: u16,
        definition: String,
    },
    SetExplicitStop {
        name: String,
        stopped: bool,
    },
    /// Record a group's state; the default state removes its row.
    SetGroupState {
        group: String,
        suspended: bool,
        mode: String,
    },
    /// Record a node's schedule; `active` removes its row.
    SetNodeSchedule {
        node: String,
        schedule: String,
    },
    /// Delete the states of groups not in `keep`; the outcome names them.
    PruneGroupStates {
        keep: Vec<String>,
    },
    /// Delete the schedules of nodes not in `keep`; the outcome names them.
    PruneNodeSchedules {
        keep: Vec<String>,
    },
    /// Record that `rift admin move` moved named server `name` to `port` on
    /// `node`, away from the node and backend the configuration names.
    SetMovedServer {
        name: String,
        configured_node: String,
        configured_backend: String,
        node: String,
        port: u16,
    },
    /// Forget the move of named server `name`.
    ForgetMovedServer {
        name: String,
    },
    /// Forget the moves of servers not in `keep`; the outcome names them.
    PruneMovedServers {
        keep: Vec<String>,
    },
    /// A cluster leader records the runtime state of managed server `name`
    /// for every peer.
    SetServerState {
        name: String,
        state: ServerState,
    },
    /// Forget the runtime states of servers not in `keep`; the outcome
    /// names them.
    PruneServerStates {
        keep: Vec<String>,
    },
}

/// The result of an [`InstanceOp`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
pub enum InstanceOutcome {
    Done,
    /// The name [`InstanceOp::AllocateName`] allocated.
    Name(String),
    /// The groups, nodes or moved servers a prune removed.
    Pruned(Vec<String>),
    /// The message of a failure; nothing was written.
    Failed(String),
}

impl InstanceOutcome {
    pub fn failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

impl InstanceOp {
    pub fn tables(&self) -> &'static [&'static str] {
        match self {
            Self::AllocateName { .. } => SEQUENCES,
            Self::BeginCreation { .. } => CREATION,
            Self::Transition { .. }
            | Self::Forget { .. }
            | Self::Move { .. }
            | Self::SetExplicitStop { .. } => INSTANCES,
            Self::SetGroupState { .. } | Self::PruneGroupStates { .. } => GROUP_STATES,
            Self::SetNodeSchedule { .. } | Self::PruneNodeSchedules { .. } => NODE_SCHEDULES,
            Self::SetMovedServer { .. }
            | Self::ForgetMovedServer { .. }
            | Self::PruneMovedServers { .. } => MOVED_SERVERS,
            Self::SetServerState { .. } | Self::PruneServerStates { .. } => SERVER_STATES,
        }
    }
}

/// Perform `op` in the caller's transaction.
pub fn execute(connection: &Conn, op: &InstanceOp) -> InstanceOutcome {
    perform(connection, op).unwrap_or_else(InstanceOutcome::Failed)
}

fn perform(connection: &Conn, op: &InstanceOp) -> Result<InstanceOutcome, String> {
    match op {
        InstanceOp::AllocateName {
            group,
            floor,
            taken,
            taken_nocase,
        } => {
            let current: Option<String> = connection
                .query_row(
                    "SELECT value FROM sequences WHERE group_name=?1",
                    params![group],
                    |r| r.get(0),
                )
                .optional()
                .or_message()?;
            let mut sequence = current
                .map(|s| s.parse::<u64>())
                .transpose()
                .or_message()?
                .unwrap_or(0)
                .max(*floor);
            let name = loop {
                sequence = sequence
                    .checked_add(1)
                    .ok_or_else(|| "instance name sequence exhausted".to_owned())?;
                let name = format!("{group}-{sequence}");
                if !taken.contains(&name)
                    && !taken_nocase.iter().any(|n| n.eq_ignore_ascii_case(&name))
                {
                    break name;
                }
            };
            connection.execute("INSERT INTO sequences VALUES (?1,?2) ON CONFLICT(group_name) DO UPDATE SET value=excluded.value",
                params![group, sequence.to_string()]).or_message()?;
            Ok(InstanceOutcome::Name(name))
        }
        InstanceOp::BeginCreation {
            name,
            node,
            port,
            definition,
        } => {
            connection
                .execute(
                    "INSERT INTO instances (name,node,port,phase,definition) VALUES (?1,?2,?3,'creating',?4)",
                    params![name, node, port, definition],
                )
                .or_message()?;
            // Every creation of a name gets a new login generation, so a
            // recreated instance never inherits its predecessor's bus token.
            connection
                .execute(
                    "INSERT INTO login_generations (name,generation) VALUES (?1,1)
                     ON CONFLICT(name) DO UPDATE SET generation=login_generations.generation+1",
                    params![name],
                )
                .or_message()?;
            Ok(InstanceOutcome::Done)
        }
        InstanceOp::Transition { name, from, to } => {
            let changed = connection
                .execute(
                    "UPDATE instances SET phase=?1 WHERE name=?2 AND phase=?3",
                    params![to, name, from],
                )
                .or_message()?;
            if changed != 1 {
                return Err(format!("instances.{name}: expected phase {from}"));
            }
            Ok(InstanceOutcome::Done)
        }
        InstanceOp::Forget { name } => {
            connection
                .execute("DELETE FROM instances WHERE name=?1", params![name])
                .or_message()?;
            Ok(InstanceOutcome::Done)
        }
        InstanceOp::Move {
            name,
            from,
            node,
            port,
            definition,
        } => {
            let changed = connection
                .execute(
                    "UPDATE instances SET node=?1, port=?2, definition=?3
                     WHERE name=?4 AND phase='ready' AND node=?5",
                    params![node, port, definition, name, from],
                )
                .or_message()?;
            if changed != 1 {
                return Err(format!(
                    "instances.{name}: registration changed during the move"
                ));
            }
            Ok(InstanceOutcome::Done)
        }
        InstanceOp::SetExplicitStop { name, stopped } => {
            // Static managed servers are outside the instance registry.
            connection
                .execute(
                    "UPDATE instances SET explicitly_stopped=?1 WHERE name=?2 AND phase='ready'",
                    params![stopped, name],
                )
                .or_message()?;
            Ok(InstanceOutcome::Done)
        }
        InstanceOp::SetGroupState {
            group,
            suspended,
            mode,
        } => {
            let mode =
                GroupMode::parse(mode).ok_or_else(|| format!("invalid mode for group {group}"))?;
            let state = GroupState {
                suspended: *suspended,
                mode,
            };
            if state.is_default() {
                connection
                    .execute("DELETE FROM group_states WHERE name=?1", params![group])
                    .or_message()?;
            } else {
                connection
                    .execute(
                        "INSERT INTO group_states (name,suspended,mode) VALUES (?1,?2,?3)
                         ON CONFLICT(name) DO UPDATE SET suspended=excluded.suspended, mode=excluded.mode",
                        params![group, state.suspended, state.mode.as_str()],
                    )
                    .or_message()?;
            }
            Ok(InstanceOutcome::Done)
        }
        InstanceOp::SetNodeSchedule { node, schedule } => {
            let schedule = NodeSchedule::parse(schedule)
                .ok_or_else(|| format!("invalid schedule for node {node}"))?;
            if schedule == NodeSchedule::Active {
                connection
                    .execute("DELETE FROM node_schedules WHERE name=?1", params![node])
                    .or_message()?;
            } else {
                connection
                    .execute(
                        "INSERT INTO node_schedules (name,schedule) VALUES (?1,?2)
                         ON CONFLICT(name) DO UPDATE SET schedule=excluded.schedule",
                        params![node, schedule.as_str()],
                    )
                    .or_message()?;
            }
            Ok(InstanceOutcome::Done)
        }
        InstanceOp::PruneGroupStates { keep } => {
            prune(connection, "group_states", keep).map(InstanceOutcome::Pruned)
        }
        InstanceOp::PruneNodeSchedules { keep } => {
            prune(connection, "node_schedules", keep).map(InstanceOutcome::Pruned)
        }
        InstanceOp::SetMovedServer {
            name,
            configured_node,
            configured_backend,
            node,
            port,
        } => {
            connection
                .execute(
                    "INSERT INTO moved_servers (name,configured_node,configured_backend,node,port)
                     VALUES (?1,?2,?3,?4,?5)
                     ON CONFLICT(name) DO UPDATE SET configured_node=excluded.configured_node,
                        configured_backend=excluded.configured_backend, node=excluded.node,
                        port=excluded.port",
                    params![name, configured_node, configured_backend, node, port],
                )
                .or_message()?;
            Ok(InstanceOutcome::Done)
        }
        InstanceOp::ForgetMovedServer { name } => {
            connection
                .execute("DELETE FROM moved_servers WHERE name=?1", params![name])
                .or_message()?;
            Ok(InstanceOutcome::Done)
        }
        InstanceOp::PruneMovedServers { keep } => {
            prune(connection, "moved_servers", keep).map(InstanceOutcome::Pruned)
        }
        InstanceOp::SetServerState { name, state } => {
            if crate::managed::Phase::parse(&state.phase).is_none() {
                return Err(format!(
                    "server_states.{name}: unknown phase {:?}",
                    state.phase
                ));
            }
            connection
                .execute(
                    "INSERT INTO server_states (name,phase,last_error,automatic,restart_attempts)
                     VALUES (?1,?2,?3,?4,?5)
                     ON CONFLICT(name) DO UPDATE SET phase=excluded.phase,
                        last_error=excluded.last_error, automatic=excluded.automatic,
                        restart_attempts=excluded.restart_attempts",
                    params![
                        name,
                        state.phase,
                        state.last_error,
                        state.automatic,
                        state.restart_attempts
                    ],
                )
                .or_message()?;
            Ok(InstanceOutcome::Done)
        }
        InstanceOp::PruneServerStates { keep } => {
            prune(connection, "server_states", keep).map(InstanceOutcome::Pruned)
        }
    }
}

/// Every runtime state a cluster leader recorded.
fn read_server_states(connection: &Conn) -> Result<BTreeMap<String, ServerState>, String> {
    let rows = connection
        .query_map(
            "SELECT name,phase,last_error,automatic,restart_attempts FROM server_states ORDER BY name",
            params![],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    ServerState {
                        phase: row.get::<_, String>(1)?,
                        last_error: row.get::<_, Option<String>>(2)?,
                        automatic: row.get::<_, bool>(3)?,
                        restart_attempts: row.get::<_, u32>(4)?,
                    },
                ))
            },
        )
        .or_message()?;
    Ok(rows.into_iter().collect())
}

/// Delete the rows of `table` whose name is not in `keep`; their names.
fn prune(connection: &Conn, table: &str, keep: &[String]) -> Result<Vec<String>, String> {
    let names: Vec<String> = connection
        .query_map(
            &format!("SELECT name FROM {table} ORDER BY name"),
            params![],
            |row| row.get(0),
        )
        .or_message()?;
    let removed: Vec<String> = names
        .into_iter()
        .filter(|name| !keep.contains(name))
        .collect();
    for name in &removed {
        connection
            .execute(&format!("DELETE FROM {table} WHERE name=?1"), params![name])
            .or_message()?;
    }
    Ok(removed)
}

/// Every group with a state other than the default.
fn read_group_states(connection: &Conn) -> Result<BTreeMap<String, GroupState>, String> {
    let rows = connection
        .query_map(
            "SELECT name,suspended,mode FROM group_states ORDER BY name",
            params![],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .or_message()?;
    rows.into_iter()
        .map(|(name, suspended, mode)| {
            let mode =
                GroupMode::parse(&mode).ok_or_else(|| format!("invalid mode for group {name}"))?;
            Ok((name, GroupState { suspended, mode }))
        })
        .collect()
}

/// Every node that is cordoned or draining.
fn read_node_schedules(connection: &Conn) -> Result<BTreeMap<String, NodeSchedule>, String> {
    let rows = connection
        .query_map(
            "SELECT name,schedule FROM node_schedules ORDER BY name",
            params![],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .or_message()?;
    rows.into_iter()
        .map(|(name, schedule)| {
            let schedule = NodeSchedule::parse(&schedule)
                .ok_or_else(|| format!("invalid schedule for node {name}"))?;
            Ok((name, schedule))
        })
        .collect()
}

/// Every named server `rift admin move` moved.
fn read_moved_servers(connection: &Conn) -> Result<BTreeMap<String, MovedServer>, String> {
    let rows = connection
        .query_map(
            "SELECT name,configured_node,configured_backend,node,port FROM moved_servers ORDER BY name",
            params![],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    MovedServer {
                        configured_node: row.get(1)?,
                        configured_backend: row.get(2)?,
                        node: row.get(3)?,
                        port: row.get(4)?,
                    },
                ))
            },
        )
        .or_message()?;
    Ok(rows.into_iter().collect())
}

const RECORD_COLUMNS: &str =
    "SELECT name,port,phase,explicitly_stopped,definition,node FROM instances ORDER BY name";

/// Every registration, validated.
fn read_records(connection: &Conn) -> Result<Vec<Record>, String> {
    let rows = connection
        .query_map(RECORD_COLUMNS, params![], |row: &Row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u16>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .or_message()?;
    rows.into_iter()
        .map(|(name, port, phase, stopped, json, node)| {
            let definition: Definition = serde_json::from_str(&json).or_message()?;
            if port != definition.instance.port
                || node != definition.instance.node.clone().unwrap_or_default()
                || !["creating", "ready", "removing", "deleting"].contains(&phase.as_str())
            {
                return Err(format!("invalid registration for {name}"));
            }
            if definition.instance.node.is_some() {
                if !crate::config::valid_node_directory(&definition.plan.directory)
                    || definition.plan.stage.is_some()
                {
                    return Err(format!("instances.{name}: invalid node directory"));
                }
            } else {
                provisioning::validate_plan(&definition.instance, &name, &definition.plan)
                    .map_err(|e| format!("instances.{name}: {e}"))?;
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

/// How long a `keep_running` server found during removal recovery may take
/// to stop before it is killed.
const REMOVAL_STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug)]
pub struct Store {
    database: Arc<Database>,
    /// Serializes read-modify-write changes of group states.
    group_writes: Mutex<()>,
    /// Hooks registered through this store, removed with it.
    hooks: Mutex<Vec<HookId>>,
}

impl Drop for Store {
    fn drop(&mut self) {
        let hooks = std::mem::take(self.hooks.get_mut().unwrap_or_else(|e| e.into_inner()));
        for hook in hooks {
            self.database.remove_hook(hook);
        }
    }
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
    pub fn check(location: &Location, config: &Config) -> io::Result<DatabaseCheck> {
        let opened = match location {
            Location::Sqlite(path) => {
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
                Database::read_only(&path)?
            }
            Location::Server(_) => {
                let database = Database::connect_read_only(location)?;
                if database.owner()?.is_some() {
                    return Ok(DatabaseCheck::InUse);
                }
                database
            }
        };
        let store = Self::with_database(Arc::new(opened));
        let (version, check) = {
            let connection = store.database.read();
            let version = database::version(&connection, COMPONENT).map_err(|e| store.error(e))?;
            // A server keeps its own consistency; only a file is checked.
            let check: Option<String> = match connection.dialect() {
                Dialect::Sqlite => Some(
                    connection
                        .query_row("PRAGMA quick_check", params![], |r| r.get(0))
                        .map_err(|e| store.error(e))?,
                ),
                Dialect::Postgres | Dialect::Mariadb => None,
            };
            (version, check)
        };
        match check {
            Some(check) if check != "ok" => {
                return Err(store.error(format!("integrity check failed: {check}")));
            }
            // A server database without instance tables yet; startup
            // creates them.
            None if version == 0 => return Ok(DatabaseCheck::Missing),
            _ => {}
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
        for (component, latest, sql) in COMPONENTS {
            database.migrate(component, latest, sql)?;
        }
        Ok(Self::with_database(database))
    }

    fn with_database(database: Arc<Database>) -> Self {
        Self {
            database,
            group_writes: Mutex::new(()),
            hooks: Mutex::new(Vec::new()),
        }
    }

    /// The database this store shares with the permission store.
    pub fn database(&self) -> &Arc<Database> {
        &self.database
    }

    fn error(&self, error: impl std::fmt::Display) -> io::Error {
        self.database.error(error)
    }

    /// Run one write; a failed outcome becomes this store's error.
    fn run(&self, op: InstanceOp) -> io::Result<InstanceOutcome> {
        match self.database.run(Operation::Instances(op))? {
            Outcome::Instances(InstanceOutcome::Failed(message)) => Err(self.error(message)),
            Outcome::Instances(outcome) => Ok(outcome),
            other => Err(self.error(format!("unexpected outcome {other:?}"))),
        }
    }

    fn records(&self) -> io::Result<Vec<Record>> {
        read_records(&self.database.read()).map_err(|e| self.error(e))
    }

    /// Validate every registration before recovering intents or launching workers.
    pub fn restore(&self, config: &mut Config) -> io::Result<()> {
        self.restore_with(config, true)
    }

    /// [`Store::restore`]; without `recover`, only validate and take the
    /// ready registrations and moves into `config`, leaving unfinished
    /// creations, removals and stale moves alone. A cluster peer restores
    /// this way: only its leader recovers ([`Store::recover`]), once it
    /// leads (docs/clustering.md, "Work that runs once: the leader").
    pub fn restore_with(&self, config: &mut Config, recover: bool) -> io::Result<()> {
        let records = self.records()?;
        let mut recovery_ports = Vec::new();
        for record in &records {
            if recover
                && ["removing", "deleting"].contains(&record.phase.as_str())
                && !record.remote()
            {
                let address = SocketAddr::from(([127, 0, 0, 1], record.definition.instance.port));
                // A `keep_running` server outlived the proxy that was removing it.
                crate::managed::stop_kept(&record.definition.plan.directory, REMOVAL_STOP_GRACE)
                    .map_err(|e| self.error(format!("instances.{}: {e}", record.name)))?;
                recovery_ports.push(TcpListener::bind(address).map_err(|e| self.error(format!(
                    "instances.{}: port {address} is still in use during removal recovery: {e}; stop the old server before retrying", record.name)))?);
            }
        }
        let mut candidate = config.clone();
        let moved = self.moved_servers()?;
        candidate
            .restore_moved_servers(&moved)
            .map_err(|e| self.error(format!("configuration conflict: {e}")))?;
        candidate
            .restore_instances(
                records
                    .iter()
                    .filter(|r| r.phase == "ready")
                    .map(|r| (r.name.clone(), r.definition.instance.clone()))
                    .collect(),
            )
            .map_err(|e| self.error(format!("configuration conflict: {e}")))?;
        for (name, entry) in &moved {
            match candidate.moved_servers.get(name) {
                Some(_) => crate::log_info!(
                    "managed_servers.{name} runs on node {} since rift admin move; the configuration still names node {}",
                    entry.node,
                    entry.configured_node
                ),
                None => crate::log_info!(
                    "managed_servers.{name}: the configuration changed since rift admin move, so it decides the node again"
                ),
            }
        }
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
                if definition.instance.storage.discards_data() {
                    storage_paths.push(provisioning::tombstone(&definition.plan)?);
                }
            }
            for directory in &storage_paths {
                // Stored paths must not overlap the database, templates, or another worker.
                if let Some(file) = self.database.path()
                    && (file.starts_with(directory) || directory.starts_with(file))
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
                provisioning::validate_registered(&definition.instance, &definition.plan)
                    .map_err(|e| self.error(format!("instances.{}: {e}", record.name)))?;
                check_configured_port(&candidate, &record.name, definition.instance.port)
                    .map_err(|e| self.error(e))?;
            }
        }
        if recover {
            self.recover_records(records)?;
            self.retain_moved_servers(&candidate.moved_servers)?;
        }
        *config = candidate;
        Ok(())
    }

    /// Finish what an earlier controller left half done: abort unfinished
    /// creations, finish removals, and forget the moves `config` no longer
    /// applies. A cluster leader runs this when it gains leadership, for
    /// whatever the previous leader left.
    pub fn recover(&self, config: &Config) -> io::Result<()> {
        self.recover_records(self.records()?)?;
        self.retain_moved_servers(&config.moved_servers)
    }

    fn recover_records(&self, records: Vec<Record>) -> io::Result<()> {
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
        Ok(())
    }

    /// Keep these sockets until startup validation finishes; fail before autostart.
    /// A port held by the instance's own `keep_running` server, which its
    /// worker adopts, is not a conflict.
    pub fn reserve_restored_ports(&self, config: &Config) -> io::Result<Vec<TcpListener>> {
        config.instances.iter().filter(|(_, instance)| instance.node.is_none()).filter_map(|(name, instance)| {
            if let Err(error) = check_configured_port(config, name, instance.port) {
                return Some(Err(error));
            }
            let address = SocketAddr::from(([127,0,0,1], instance.port));
            match TcpListener::bind(address) {
                Ok(listener) => Some(Ok(listener)),
                Err(_) if config.managed_servers.get(name).is_some_and(|server| server.keep_running && crate::managed::kept_running(&server.directory)) => None,
                Err(e) => Some(Err(self.error(format!("instances.{name}: port {address} is unavailable: {e}; stop the conflicting process or correct the configuration")))),
            }
        }).collect()
    }

    /// The storage plan of registered instance `name`.
    pub(crate) fn plan(&self, name: &str) -> io::Result<Option<Plan>> {
        Ok(self
            .records()?
            .into_iter()
            .find(|r| r.name == name && r.phase == "ready")
            .map(|r| r.definition.plan))
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
        let prefix = format!("{group}-");
        // Also handle a registry imported by an embedding caller.
        let floor = config
            .instances
            .keys()
            .filter_map(|name| name.strip_prefix(&prefix)?.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        // A candidate `{group}-{n}` can only equal a name with this prefix:
        // a destination, a managed server, or (for BungeeCord, ASCII case
        // ignored) a backend.
        let taken: BTreeSet<String> = config
            .backends
            .keys()
            .chain(config.service_groups.keys())
            .chain(config.managed_servers.keys())
            .filter(|name| name.starts_with(&prefix))
            .cloned()
            .collect();
        let taken_nocase: Vec<String> = if config.network.bungeecord {
            config
                .backends
                .keys()
                .filter(|name| {
                    name.get(..prefix.len())
                        .is_some_and(|head| head.eq_ignore_ascii_case(&prefix))
                })
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        match self.run(InstanceOp::AllocateName {
            group: group.to_owned(),
            floor,
            taken: taken.into_iter().collect(),
            taken_nocase,
        })? {
            InstanceOutcome::Name(name) => Ok(name),
            other => Err(self.error(format!("unexpected outcome {other:?}"))),
        }
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
        self.run(InstanceOp::BeginCreation {
            name: name.to_owned(),
            node: instance.node.clone().unwrap_or_default(),
            port: instance.port,
            definition,
        })?;
        Ok(())
    }

    /// `rift cluster init`: put every ready instance of this host (stored
    /// with an empty node and an absolute directory) on node `node`, the
    /// initializing peer, with the directory `config` gives an instance of
    /// its group there (relative to the node's `--root`). Its files stay
    /// where they are; the result says where each instance was and where
    /// the node looks for it. `config` must define the node.
    pub fn convert_local_instances(
        &self,
        config: &Config,
        node: &str,
    ) -> io::Result<Vec<ConvertedInstance>> {
        let mut converted = Vec::new();
        for record in self.records()? {
            if record.remote() || record.phase != "ready" {
                continue;
            }
            let instance = &record.definition.instance;
            let mut candidate = config.clone();
            candidate
                .add_instance_on(&instance.group, &record.name, instance.port, Some(node))
                .map_err(|e| {
                    self.error(format!(
                        "instances.{}: cannot put it on node {node}: {e}",
                        record.name
                    ))
                })?;
            let directory = candidate
                .managed_servers
                .get(&record.name)
                .map(|server| server.directory.clone())
                .ok_or_else(|| self.error(format!("instances.{}: no directory", record.name)))?;
            let definition = serde_json::to_string(&Definition {
                instance: ServiceInstance {
                    node: Some(node.to_owned()),
                    ..instance.clone()
                },
                plan: Plan {
                    directory: directory.clone(),
                    marker: None,
                    stage: None,
                    created: false,
                },
            })
            .map_err(|e| self.error(e))?;
            self.run(InstanceOp::Move {
                name: record.name.clone(),
                from: String::new(),
                node: node.to_owned(),
                port: instance.port,
                definition,
            })?;
            converted.push(ConvertedInstance {
                name: record.name.clone(),
                from: record.definition.plan.directory.clone(),
                to: directory,
            });
        }
        Ok(converted)
    }

    /// Re-register a ready remote instance on another node: the commit point
    /// of `rift admin move`. Only its node and port change; its phase,
    /// administrator stop and login generation stay. `from` must still be
    /// the stored node, so a concurrent change cannot be overwritten.
    pub fn move_instance(&self, config: &Config, name: &str, from: &str) -> io::Result<()> {
        let instance = config
            .instances
            .get(name)
            .ok_or_else(|| self.error("unknown instance"))?;
        let node = instance
            .node
            .clone()
            .ok_or_else(|| self.error("instances move only between nodes"))?;
        let record = self
            .records()?
            .into_iter()
            .find(|r| r.name == name && r.phase == "ready")
            .ok_or_else(|| self.error(format!("instances.{name}: not registered")))?;
        if record.definition.instance.node.as_deref() != Some(from)
            || record.definition.instance.group != instance.group
            || record.definition.instance.storage != instance.storage
        {
            return Err(self.error(format!(
                "instances.{name}: registration changed during the move"
            )));
        }
        let definition = serde_json::to_string(&Definition {
            instance: instance.clone(),
            plan: record.definition.plan,
        })
        .map_err(|e| self.error(e))?;
        self.run(InstanceOp::Move {
            name: name.to_owned(),
            from: from.to_owned(),
            node,
            port: instance.port,
            definition,
        })?;
        Ok(())
    }

    /// The bus login generation of instance `name`; 0 if it was never
    /// created here (a static managed server).
    pub fn login_generation(&self, name: &str) -> io::Result<u64> {
        let connection = self.database.read();
        let generation: Option<i64> = connection
            .query_row(
                "SELECT generation FROM login_generations WHERE name=?1",
                params![name],
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
        self.run(InstanceOp::Transition {
            name: name.to_owned(),
            from: from.to_owned(),
            to: to.to_owned(),
        })?;
        Ok(())
    }

    fn forget(&self, name: &str) -> io::Result<()> {
        self.run(InstanceOp::Forget {
            name: name.to_owned(),
        })?;
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
        if record.definition.instance.storage.discards_data() && !record.remote() {
            if record.phase == "removing" {
                provisioning::retire_storage(&record.definition.plan)?;
                self.transition(name, "removing", "deleting")?;
            }
            provisioning::delete_retired(&record.definition.plan)?;
        }
        self.forget(name)
    }

    pub fn set_explicit_stop(&self, name: &str, stopped: bool) -> io::Result<()> {
        self.run(InstanceOp::SetExplicitStop {
            name: name.to_owned(),
            stopped,
        })?;
        Ok(())
    }

    /// Every group with a state other than the default.
    pub fn group_states(&self) -> io::Result<BTreeMap<String, GroupState>> {
        read_group_states(&self.database.read()).map_err(|e| self.error(e))
    }

    /// Record a group's state; the default state removes its row.
    pub fn set_group_state(&self, group: &str, state: GroupState) -> io::Result<()> {
        self.run(InstanceOp::SetGroupState {
            group: group.to_owned(),
            suspended: state.suspended,
            mode: state.mode.as_str().to_owned(),
        })?;
        Ok(())
    }

    /// Change a group's stored state with `change` and return the new state.
    /// Changes through this store are serialized, so concurrent changes to
    /// different fields are both kept.
    pub fn update_group_state(
        &self,
        group: &str,
        change: impl FnOnce(&mut GroupState),
    ) -> io::Result<GroupState> {
        let _serialized = self.group_writes.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.group_states()?.get(group).copied().unwrap_or_default();
        change(&mut state);
        self.set_group_state(group, state)?;
        Ok(state)
    }

    /// Delete the states of groups `keep` does not name; their names.
    pub fn prune_group_states<'a>(
        &self,
        keep: impl IntoIterator<Item = &'a String>,
    ) -> io::Result<Vec<String>> {
        match self.run(InstanceOp::PruneGroupStates {
            keep: keep.into_iter().cloned().collect(),
        })? {
            InstanceOutcome::Pruned(names) => Ok(names),
            other => Err(self.error(format!("unexpected outcome {other:?}"))),
        }
    }

    /// Every node that is cordoned or draining.
    pub fn node_schedules(&self) -> io::Result<BTreeMap<String, NodeSchedule>> {
        read_node_schedules(&self.database.read()).map_err(|e| self.error(e))
    }

    /// Record a node's schedule; `active` removes its row.
    pub fn set_node_schedule(&self, node: &str, schedule: NodeSchedule) -> io::Result<()> {
        self.run(InstanceOp::SetNodeSchedule {
            node: node.to_owned(),
            schedule: schedule.as_str().to_owned(),
        })?;
        Ok(())
    }

    /// The servers of `managed_servers` that `rift admin move` moved.
    pub fn moved_servers(&self) -> io::Result<BTreeMap<String, MovedServer>> {
        read_moved_servers(&self.database.read()).map_err(|e| self.error(e))
    }

    /// Record where named server `name` runs since its move; `None` forgets
    /// the move, once the server is back where the configuration has it.
    pub fn set_moved_server(&self, name: &str, moved: Option<&MovedServer>) -> io::Result<()> {
        let op = match moved {
            None => InstanceOp::ForgetMovedServer {
                name: name.to_owned(),
            },
            Some(moved) => InstanceOp::SetMovedServer {
                name: name.to_owned(),
                configured_node: moved.configured_node.clone(),
                configured_backend: moved.configured_backend.clone(),
                node: moved.node.clone(),
                port: moved.port,
            },
        };
        self.run(op)?;
        Ok(())
    }

    /// Forget the moves that no longer apply, because the configuration
    /// changed the node or backend of their servers, or dropped them.
    pub fn retain_moved_servers(&self, applied: &BTreeMap<String, MovedServer>) -> io::Result<()> {
        if self
            .moved_servers()?
            .keys()
            .all(|name| applied.contains_key(name))
        {
            return Ok(());
        }
        self.run(InstanceOp::PruneMovedServers {
            keep: applied.keys().cloned().collect(),
        })?;
        Ok(())
    }

    /// Delete the schedules of nodes `keep` does not name; their names.
    pub fn prune_node_schedules<'a>(
        &self,
        keep: impl IntoIterator<Item = &'a String>,
    ) -> io::Result<Vec<String>> {
        match self.run(InstanceOp::PruneNodeSchedules {
            keep: keep.into_iter().cloned().collect(),
        })? {
            InstanceOutcome::Pruned(names) => Ok(names),
            other => Err(self.error(format!("unexpected outcome {other:?}"))),
        }
    }

    fn remember(&self, hook: HookId) {
        self.hooks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(hook);
    }

    /// Mirror group states and node schedules into memory: `groups` and
    /// `nodes` get the whole table now and again after every change to it,
    /// whether the write was local or applied from elsewhere. They run on the
    /// writing thread while it holds the database connection, so they must
    /// not use the database.
    pub fn watch_states(
        &self,
        groups: impl Fn(BTreeMap<String, GroupState>) + Send + Sync + 'static,
        nodes: impl Fn(BTreeMap<String, NodeSchedule>) + Send + Sync + 'static,
    ) -> io::Result<()> {
        let label = self.database.label().to_owned();
        let connection = self.database.lock();
        groups(read_group_states(&connection).map_err(|e| self.error(e))?);
        nodes(read_node_schedules(&connection).map_err(|e| self.error(e))?);
        let group_label = label.clone();
        self.remember(
            self.database.on_change(
                &["group_states"],
                move |connection, _| match read_group_states(connection) {
                    Ok(states) => groups(states),
                    Err(error) => {
                        log_error!(
                            "instance database {group_label}: group states not reloaded: {error}"
                        )
                    }
                },
            ),
        );
        self.remember(
            self.database
                .on_change(
                    &["node_schedules"],
                    move |connection, _| match read_node_schedules(connection) {
                        Ok(schedules) => nodes(schedules),
                        Err(error) => {
                            log_error!(
                                "instance database {label}: node schedules not reloaded: {error}"
                            )
                        }
                    },
                ),
        );
        drop(connection);
        Ok(())
    }

    /// Mirror the ready registrations (what `Config.instances` holds) into
    /// memory: `instances` gets them after every change to the registry,
    /// under the same rules as [`Store::watch_states`].
    pub fn watch_instances(
        &self,
        instances: impl Fn(BTreeMap<String, ServiceInstance>) + Send + Sync + 'static,
    ) {
        let label = self.database.label().to_owned();
        self.remember(
            self.database
                .on_change(&["instances"], move |connection, _| {
                    match read_records(connection) {
                        Ok(records) => instances(
                            records
                                .into_iter()
                                .filter(|record| record.phase == "ready")
                                .map(|record| (record.name, record.definition.instance))
                                .collect(),
                        ),
                        Err(error) => {
                            log_error!(
                                "instance database {label}: registrations not reloaded: {error}"
                            )
                        }
                    }
                }),
        );
    }

    /// The runtime states a cluster leader recorded, by server.
    pub fn server_states(&self) -> io::Result<BTreeMap<String, ServerState>> {
        read_server_states(&self.database.read()).map_err(|e| self.error(e))
    }

    /// Record `name`'s runtime state (a cluster leader's write).
    pub fn set_server_state(&self, name: &str, state: ServerState) -> io::Result<()> {
        self.run(InstanceOp::SetServerState {
            name: name.to_owned(),
            state,
        })?;
        Ok(())
    }

    /// Forget the runtime states of servers `keep` does not name.
    pub fn prune_server_states<'a>(
        &self,
        keep: impl IntoIterator<Item = &'a String>,
    ) -> io::Result<Vec<String>> {
        match self.run(InstanceOp::PruneServerStates {
            keep: keep.into_iter().cloned().collect(),
        })? {
            InstanceOutcome::Pruned(names) => Ok(names),
            other => Err(self.error(format!("unexpected outcome {other:?}"))),
        }
    }

    /// Mirror the recorded runtime states into memory: `states` gets the
    /// whole table now and after every change, under the rules of
    /// [`Store::watch_states`].
    pub fn watch_server_states(
        &self,
        states: impl Fn(BTreeMap<String, ServerState>) + Send + Sync + 'static,
    ) -> io::Result<()> {
        let label = self.database.label().to_owned();
        let connection = self.database.lock();
        states(read_server_states(&connection).map_err(|e| self.error(e))?);
        self.remember(
            self.database
                .on_change(
                    &["server_states"],
                    move |connection, _| match read_server_states(connection) {
                        Ok(current) => states(current),
                        Err(error) => {
                            log_error!(
                                "instance database {label}: server states not reloaded: {error}"
                            )
                        }
                    },
                ),
        );
        drop(connection);
        Ok(())
    }

    /// Mirror the moves of named servers into memory: `moved` gets the
    /// whole table after every change, under the rules of
    /// [`Store::watch_states`].
    pub fn watch_moved_servers(
        &self,
        moved: impl Fn(BTreeMap<String, MovedServer>) + Send + Sync + 'static,
    ) {
        let label = self.database.label().to_owned();
        self.remember(
            self.database
                .on_change(
                    &["moved_servers"],
                    move |connection, _| match read_moved_servers(connection) {
                        Ok(current) => moved(current),
                        Err(error) => {
                            log_error!(
                                "instance database {label}: moved servers not reloaded: {error}"
                            )
                        }
                    },
                ),
        );
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
