//! The proxy's permission authority: the SQLite store in the shared database
//! ([`store`]), compile cache, revisions and change fan-out ([`authority`]),
//! the change log ([`audit`]), LuckPerms import and export ([`import`]), the
//! Lua `rift.permissions` module ([`lua`]), the `/perms` grammar shared by
//! every command surface ([`command`]), the bus handlers and change fan-out
//! ([`control`]), the `/api/perms` HTTP routes ([`http`]) and the replication
//! contract a follower proxy will implement ([`follower`]).
//!
//! The model, the resolver and the wire types live in the `rift-permissions`
//! crate so backends resolve exactly as the proxy does; nothing here walks
//! inheritance or matches patterns. `docs/permissions.md` describes the
//! behaviour, `docs/permissions-protocol.md` the JSON shapes.
mod audit;
mod authority;
pub mod command;
pub mod control;
pub mod follower;
pub mod http;
mod import;
pub(crate) mod lua;
mod store;

pub use audit::LogQuery;
pub use authority::{Authority, Bulk, Holder, Member, Notification, Registered, UserSummary};
pub use http::BackendDirectory;
pub use import::{ImportReport, read_export, write_export};

use rift_permissions::wire::ErrorCode;
use std::{fmt, io};

/// The publish and subscribe grants of a backend principal
/// (`messaging.principals[].preset = "backend"`): the permission protocol,
/// and the [network API](crate::network_service), whose `connect` a backend
/// may use for its own players only.
pub fn backend_grants() -> (Vec<String>, Vec<String>) {
    let strings = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect();
    let mut publish: Vec<String> = strings(&control::BACKEND_PUBLISH);
    publish.push(format!("{}.*", crate::network_service::PREFIX));
    (publish, strings(&control::BACKEND_SUBSCRIBE))
}

/// Extension commands whose permission is neither registered by a backend
/// nor granted (`true`, directly or by a covering wildcard) by any node in
/// the store at `database` or in `extensions.groups`. Reads the database
/// read-only; a missing file or a store without permission tables counts as
/// empty. For `rift check --strict`.
pub fn ungranted_command_permissions(
    database: Option<&std::path::Path>,
    groups: &std::collections::BTreeMap<String, rift_permissions::Group>,
    commands: &[(String, String)],
) -> io::Result<Vec<(String, String)>> {
    let opened = database
        .filter(|path| path.exists())
        .map(crate::database::Database::read_only)
        .transpose()?;
    let connection = opened.as_ref().map(|database| database.lock());
    let in_store = |sql: &str, key: &str| -> bool {
        connection
            .as_ref()
            .is_some_and(|connection| connection.query_row(sql, [key], |_| Ok(())).is_ok())
    };
    let configured = |pattern: &str| {
        groups.values().flat_map(|group| &group.nodes).any(|node| {
            matches!(&node.payload, rift_permissions::Payload::Permission { key, value: true } if key == pattern)
        })
    };
    let mut missing = Vec::new();
    for (command, permission) in commands {
        let registered = in_store("SELECT 1 FROM perm_registered WHERE node = ?1", permission);
        let granted = command::covering_patterns(permission).iter().any(|pattern| {
            configured(pattern)
                || in_store(
                    "SELECT 1 FROM perm_node WHERE kind = 0 AND value = 'true' AND key = ?1 LIMIT 1",
                    pattern,
                )
        });
        if !registered && !granted {
            missing.push((command.clone(), permission.clone()));
        }
    }
    Ok(missing)
}

/// Every failure of a permission read or write.
#[derive(Debug)]
#[non_exhaustive]
pub enum PermError {
    /// A value fails validation, or a track move is impossible.
    Invalid(rift_permissions::Error),
    /// A named group, track or user does not exist.
    NotFound { what: &'static str, name: String },
    /// The change conflicts with current state, such as creating a group that
    /// exists.
    Conflict(String),
    /// A store limit from `permissions.limits` would be exceeded.
    Limit { what: &'static str, limit: usize },
    /// The caller may not do this, such as reporting a context key outside
    /// `permissions.contexts.external`.
    Forbidden(String),
    /// A Lua callback's deadline passed before the write committed.
    Deadline,
    /// The database failed or holds a row Rift cannot read.
    Storage(String),
    /// An import file cannot be read or parsed.
    Import(String),
}

impl PermError {
    /// The code a bus or HTTP reply carries.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Invalid(rift_permissions::Error::Track { .. }) | Self::Conflict(_) => {
                ErrorCode::Conflict
            }
            Self::Invalid(_) | Self::Import(_) => ErrorCode::InvalidRequest,
            Self::NotFound { .. } => ErrorCode::NotFound,
            Self::Limit { .. } => ErrorCode::Limit,
            Self::Forbidden(_) => ErrorCode::Forbidden,
            Self::Deadline => ErrorCode::Unavailable,
            Self::Storage(_) => ErrorCode::Internal,
        }
    }

    fn not_found(what: &'static str, name: impl fmt::Display) -> Self {
        Self::NotFound {
            what,
            name: name.to_string(),
        }
    }
}

impl fmt::Display for PermError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(error) => error.fmt(f),
            Self::NotFound { what, name } => write!(f, "unknown {what} {name:?}"),
            Self::Conflict(message) | Self::Forbidden(message) | Self::Import(message) => {
                f.write_str(message)
            }
            Self::Limit { what, limit } => {
                write!(f, "permission limit exceeded: at most {limit} {what}")
            }
            Self::Deadline => f.write_str("script deadline exceeded"),
            Self::Storage(message) => write!(f, "permission store: {message}"),
        }
    }
}

impl std::error::Error for PermError {}

impl From<rift_permissions::Error> for PermError {
    fn from(error: rift_permissions::Error) -> Self {
        Self::Invalid(error)
    }
}

impl From<rusqlite::Error> for PermError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

impl From<PermError> for io::Error {
    fn from(error: PermError) -> Self {
        let kind = match error {
            PermError::Invalid(_)
            | PermError::NotFound { .. }
            | PermError::Conflict(_)
            | PermError::Limit { .. }
            | PermError::Import(_) => io::ErrorKind::InvalidInput,
            PermError::Forbidden(_) => io::ErrorKind::PermissionDenied,
            PermError::Deadline => io::ErrorKind::TimedOut,
            PermError::Storage(_) => io::ErrorKind::Other,
        };
        io::Error::new(kind, error.to_string())
    }
}

#[cfg(test)]
mod tests;
