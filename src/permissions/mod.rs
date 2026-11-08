//! The proxy's permission authority: the SQLite store in the shared database
//! ([`store`]), compile cache, revisions and change fan-out ([`authority`]),
//! the change log ([`audit`]), LuckPerms import and export ([`import`]), the
//! Lua `rift.permissions` module ([`lua`]) and the replication contract a
//! follower proxy will implement ([`follower`]).
//!
//! The model, the resolver and the wire types live in the `rift-permissions`
//! crate so backends resolve exactly as the proxy does; nothing here walks
//! inheritance or matches patterns. `docs/permissions.md` describes the
//! behaviour, `docs/permissions-protocol.md` the JSON shapes.
mod audit;
mod authority;
pub mod follower;
mod import;
pub(crate) mod lua;
mod store;

pub use audit::LogQuery;
pub use authority::{Authority, Bulk, Holder, Member, Notification, Registered};
pub use import::{ImportReport, read_export, write_export};
pub use rift_permissions as model;

use rift_permissions::wire::ErrorCode;
use std::{fmt, io};

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
