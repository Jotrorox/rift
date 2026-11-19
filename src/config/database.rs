//! `rift.config.database`: which backend holds the instance database.
//!
//! ```lua
//! rift.config.database = {
//!     backend = "postgres",
//!     url_env = "RIFT_DATABASE_URL", -- the default
//!     ca = "certs/postgres-ca.pem",  -- optional extra trust anchors
//!     schema = "rift",               -- optional
//!     standby = false,
//! }
//! ```
//!
//! `backend = "mariadb"` takes the same fields, with a `mariadb://` URL;
//! `schema` then names a database on the server.
//!
//! Without the table, or with `backend = "sqlite"`, the database is the
//! SQLite file `instance_database`.
use crate::database::{DEFAULT_URL_ENV, Engine, ServerConfig, Url};
use mlua::{Table, Value};
use std::path::Path;

const FIELDS: &[&str] = &["backend", "url", "url_env", "ca", "standby", "schema"];

/// Whether `name` can name a PostgreSQL schema or MariaDB database here:
/// 1..=63 bytes of lowercase letters, digits and `_`, not starting with a
/// digit or `pg_`.
fn valid_schema(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && !name.starts_with("pg_")
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn text(table: &Table, key: &str) -> Result<Option<String>, String> {
    match table.raw_get::<Value>(key).map_err(|e| e.to_string())? {
        Value::Nil => Ok(None),
        value => super::string(value, &format!("database.{key}")).map(Some),
    }
}

const BACKENDS: &str = "database.backend: expected \"sqlite\", \"postgres\" or \"mariadb\"";

/// The server settings, or `None` for SQLite.
pub(super) fn parse(root: &Table, directory: &Path) -> Result<Option<ServerConfig>, String> {
    let Some(table) = super::options(root, "database", FIELDS)? else {
        return Ok(None);
    };
    let backend = text(&table, "backend")?.ok_or(BACKENDS)?;
    let engine = match backend.as_str() {
        "sqlite" => {
            for key in FIELDS.iter().filter(|key| **key != "backend") {
                if !table
                    .raw_get::<Value>(*key)
                    .map_err(|e| e.to_string())?
                    .is_nil()
                {
                    return Err(format!(
                        "database.{key}: only for a server backend; the SQLite file is instance_database"
                    ));
                }
            }
            return Ok(None);
        }
        "postgres" | "postgresql" => Engine::Postgres,
        "mariadb" => Engine::Mariadb,
        "mysql" => {
            return Err(
                "database.backend: MySQL is not supported (it lacks INSERT ... RETURNING); use \"mariadb\" with MariaDB 10.5 or newer".into(),
            );
        }
        _ => return Err(BACKENDS.into()),
    };
    let url = match (text(&table, "url")?, text(&table, "url_env")?) {
        (Some(_), Some(_)) => {
            return Err("database: set url or url_env, not both".into());
        }
        (Some(url), None) => {
            match engine {
                Engine::Postgres => url
                    .parse::<tokio_postgres::Config>()
                    .map(|_| ())
                    .map_err(|error| error.to_string()),
                Engine::Mariadb => crate::database::mariadb::options(&url).map(|_| ()),
            }
            .map_err(|error| format!("database.url: {error}"))?;
            Url::Literal(url)
        }
        (None, url_env) => {
            let name = url_env.unwrap_or_else(|| DEFAULT_URL_ENV.to_owned());
            if !super::environment_name(&name) {
                return Err(format!(
                    "database.url_env: {name:?} is not an environment variable name"
                ));
            }
            Url::Env(name)
        }
    };
    let ca = match text(&table, "ca")? {
        None => None,
        Some(path) if path.trim().is_empty() || path.contains('\0') => {
            return Err("database.ca: expected a nonblank file path without NUL".into());
        }
        Some(path) => Some(directory.join(path)),
    };
    let schema = match text(&table, "schema")? {
        None => None,
        Some(schema) if valid_schema(&schema) => Some(schema),
        Some(schema) => {
            return Err(format!(
                "database.schema: {schema:?} must be 1..=63 lowercase letters, digits or _, not starting with a digit or pg_"
            ));
        }
    };
    Ok(Some(ServerConfig {
        engine,
        url,
        ca,
        standby: super::boolean(&table, "database", "standby", false)?,
        schema,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_lua(source: &str) -> Result<Option<ServerConfig>, String> {
        let lua = mlua::Lua::new();
        let root: Table = lua.load(source).eval().unwrap();
        parse(&root, Path::new("/etc/rift"))
    }

    #[test]
    fn postgres_settings_parse_with_defaults() {
        assert_eq!(parse_lua("return {}").unwrap(), None);
        assert_eq!(
            parse_lua("return { database = { backend = 'sqlite' } }").unwrap(),
            None
        );
        let config = parse_lua("return { database = { backend = 'postgres' } }")
            .unwrap()
            .unwrap();
        assert_eq!(config.engine, Engine::Postgres);
        assert_eq!(config.url, Url::Env(DEFAULT_URL_ENV.into()));
        assert!(!config.standby);
        assert_eq!(config.url_variable(), Some(DEFAULT_URL_ENV));
        let config = parse_lua(
            "return { database = { backend = 'postgres', url = 'postgres://rift@db/rift', ca = 'ca.pem', standby = true, schema = 'network_1' } }",
        )
        .unwrap()
        .unwrap();
        assert_eq!(config.url, Url::Literal("postgres://rift@db/rift".into()));
        assert_eq!(config.ca.as_deref(), Some(Path::new("/etc/rift/ca.pem")));
        assert!(config.standby);
        assert_eq!(config.schema.as_deref(), Some("network_1"));
        assert_eq!(
            config.describe(),
            "postgres://rift@db:5432/rift (schema network_1)"
        );
    }

    #[test]
    fn mariadb_settings_parse_with_defaults() {
        let config = parse_lua("return { database = { backend = 'mariadb' } }")
            .unwrap()
            .unwrap();
        assert_eq!(config.engine, Engine::Mariadb);
        assert_eq!(config.url, Url::Env(DEFAULT_URL_ENV.into()));
        let config = parse_lua(
            "return { database = { backend = 'mariadb', url = 'mariadb://rift:secret@db/rift?require_ssl=true', schema = 'network_1' } }",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            config.describe(),
            "mariadb://rift@db:3306/rift (database network_1)"
        );
    }

    #[test]
    fn invalid_database_settings_are_refused() {
        for (source, message) in [
            ("{ database = {} }", "database.backend"),
            (
                "{ database = { backend = 'mysql' } }",
                "MySQL is not supported",
            ),
            ("{ database = { backend = 'oracle' } }", "database.backend"),
            (
                "{ database = { backend = 'mariadb', url = 'postgres://db/rift' } }",
                "database.url",
            ),
            (
                "{ database = { backend = 'mariadb', url = 'mariadb://db/rift?sslmode=require' } }",
                "database.url",
            ),
            (
                "{ database = { backend = 'sqlite', url = 'postgres://db' } }",
                "database.url",
            ),
            (
                "{ database = { backend = 'postgres', url = 'postgres://db', url_env = 'X' } }",
                "not both",
            ),
            (
                "{ database = { backend = 'postgres', url_env = '1X' } }",
                "database.url_env",
            ),
            (
                "{ database = { backend = 'postgres', url = 'postgres://db?sslmode=sometimes' } }",
                "database.url",
            ),
            (
                "{ database = { backend = 'postgres', schema = 'Rift' } }",
                "database.schema",
            ),
            (
                "{ database = { backend = 'postgres', standby = 'yes' } }",
                "database.standby",
            ),
            (
                "{ database = { backend = 'postgres', password = 'x' } }",
                "password",
            ),
        ] {
            let error = parse_lua(&format!("return {source}")).unwrap_err();
            assert!(error.contains(message), "{source}: {error}");
        }
    }
}
