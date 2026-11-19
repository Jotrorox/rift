//! What Rift remembers about every player who joined: the name they last
//! used, when and from where they were last seen, the server they were on
//! and the language a plugin chose for them.
//!
//! The records live in the instance database (`rift.sqlite3`) beside the
//! permission store, so a ban plugin can turn a name into a UUID and a
//! language plugin can keep its choice without tables of their own. Without
//! a database file (a configuration that never persists anything) they last
//! as long as the process.
//!
//! Joins and quits are written off the session's task as [`DirectoryOp`]s
//! (their own deterministic log entry in a cluster); lookups read the table
//! directly.
use crate::database::{
    self, Conn, Database, Dialect, OptionalExtension, Row,
    operation::{Operation, OrMessage, Outcome},
    params,
};
use serde::{Deserialize, Serialize};
use std::{io, sync::Arc};

const COMPONENT: &str = "players";
const SCHEMA_VERSION: u32 = 1;
const SCHEMA: &str = "CREATE TABLE players (
    uuid TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    name_key TEXT NOT NULL,
    first_seen INTEGER NOT NULL,
    last_seen INTEGER NOT NULL,
    last_ip TEXT NOT NULL DEFAULT '',
    last_server TEXT NOT NULL DEFAULT '',
    language TEXT
) STRICT;
CREATE INDEX players_name ON players (name_key, last_seen);";
const POSTGRES_SCHEMA: &str = r#"CREATE TABLE players (
    uuid TEXT COLLATE "C" PRIMARY KEY NOT NULL,
    name TEXT COLLATE "C" NOT NULL,
    name_key TEXT COLLATE "C" NOT NULL,
    first_seen BIGINT NOT NULL,
    last_seen BIGINT NOT NULL,
    last_ip TEXT COLLATE "C" NOT NULL DEFAULT '',
    last_server TEXT COLLATE "C" NOT NULL DEFAULT '',
    language TEXT COLLATE "C"
);
CREATE INDEX players_name ON players (name_key, last_seen);"#;

/// Most records one `search` returns.
pub const MAX_SEARCH: u32 = 100;

/// The tables directory writes change.
pub const TABLES: &[&str] = &["players"];

fn schema_sql(dialect: Dialect, _from: u32) -> String {
    match dialect {
        Dialect::Sqlite => SCHEMA,
        Dialect::Postgres => POSTGRES_SCHEMA,
    }
    .to_owned()
}

/// The directory's schema component and the version this Rift writes.
pub fn schema_versions() -> Vec<(&'static str, u32)> {
    vec![(COMPONENT, SCHEMA_VERSION)]
}

/// The tables of `component`, if it is the directory's.
pub fn component_tables(component: &str) -> Option<&'static [&'static str]> {
    (component == COMPONENT).then_some(TABLES)
}

/// Migrate the directory to `to_version` in the caller's transaction;
/// `None` for another component.
pub fn migrate(
    connection: &Conn,
    component: &str,
    to_version: u32,
) -> Option<Result<bool, String>> {
    (component == COMPONENT).then(|| {
        database::migrate_component(
            connection,
            COMPONENT,
            to_version,
            SCHEMA_VERSION,
            schema_sql,
        )
        .or_message()
    })
}

/// A write to the player directory. Times are seconds since the epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DirectoryOp {
    /// A player joined: remember the name, address (`None` forgets one) and
    /// server.
    Joined {
        uuid: String,
        name: String,
        ip: Option<String>,
        server: String,
        now: u64,
    },
    /// A player left from `server`.
    Left {
        uuid: String,
        server: String,
        now: u64,
    },
    /// Set or clear the language of a known player.
    SetLanguage {
        uuid: String,
        language: Option<String>,
    },
}

/// The result of a [`DirectoryOp`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum DirectoryOutcome {
    /// The number of rows the write changed.
    Changed {
        rows: usize,
    },
    Failed {
        message: String,
    },
}

impl DirectoryOutcome {
    pub fn failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

fn seconds(now: u64) -> i64 {
    i64::try_from(now).unwrap_or(i64::MAX)
}

/// Perform `op` in the caller's transaction.
pub fn apply(connection: &Conn, op: &DirectoryOp) -> DirectoryOutcome {
    let greatest = connection.dialect().greatest();
    let result = match op {
        DirectoryOp::Joined {
            uuid,
            name,
            ip,
            server,
            now,
        } => connection.execute(
            &format!(
                "INSERT INTO players (uuid, name, name_key, first_seen, last_seen, last_ip, last_server)
                 VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6)
                 ON CONFLICT(uuid) DO UPDATE SET name = excluded.name, name_key = excluded.name_key,
                     last_seen = {greatest}(players.last_seen, excluded.last_seen),
                     last_ip = excluded.last_ip, last_server = excluded.last_server"
            ),
            params![
                uuid,
                name,
                name.to_ascii_lowercase(),
                seconds(*now),
                ip.clone().unwrap_or_default(),
                server
            ],
        ),
        DirectoryOp::Left { uuid, server, now } => connection.execute(
            &format!(
                "UPDATE players SET last_seen = {greatest}(last_seen, ?2), last_server = ?3 WHERE uuid = ?1"
            ),
            params![uuid, seconds(*now), server],
        ),
        DirectoryOp::SetLanguage { uuid, language } => connection.execute(
            "UPDATE players SET language = ?2 WHERE uuid = ?1",
            params![uuid, language],
        ),
    };
    match result {
        Ok(rows) => DirectoryOutcome::Changed { rows },
        Err(error) => DirectoryOutcome::Failed {
            message: error.to_string(),
        },
    }
}

/// One remembered player. `online` is not stored: the caller knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// 32 lowercase hexadecimal digits.
    pub uuid: String,
    pub name: String,
    pub first_seen: u64,
    pub last_seen: u64,
    pub last_ip: String,
    pub last_server: String,
    pub language: Option<String>,
}

/// A language tag as plugins set it and catalogues name it: 2..=16 bytes of
/// `a-z0-9_`, lowercase (`de`, `de_de`, `pt_br`).
pub fn language_tag(tag: &str) -> Result<String, String> {
    let tag = tag.to_ascii_lowercase().replace('-', "_");
    if !(2..=16).contains(&tag.len())
        || !tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err("a language is 2..=16 bytes of a-z, 0-9 or '_', such as de or de_de".into());
    }
    Ok(tag)
}

/// `target` as a stored UUID key, if it is one (with or without dashes).
pub fn uuid_key(target: &str) -> Option<String> {
    let hex: String = target.chars().filter(|c| *c != '-').collect();
    (hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()) && target.len() <= 36)
        .then(|| hex.to_ascii_lowercase())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

const COLUMNS: &str = "uuid, name, first_seen, last_seen, last_ip, last_server, language";

fn record(row: &Row) -> database::Result<Record> {
    Ok(Record {
        uuid: row.get(0)?,
        name: row.get(1)?,
        first_seen: row.get::<_, i64>(2)?.max(0) as u64,
        last_seen: row.get::<_, i64>(3)?.max(0) as u64,
        last_ip: row.get(4)?,
        last_server: row.get(5)?,
        language: row.get(6)?,
    })
}

/// The remembered players of one instance database.
#[derive(Debug, Clone)]
pub struct Directory {
    database: Arc<Database>,
}

impl Directory {
    /// Create or migrate the table in `database`.
    pub fn open(database: Arc<Database>) -> io::Result<Self> {
        database.migrate(COMPONENT, SCHEMA_VERSION, schema_sql)?;
        Ok(Self { database })
    }

    /// A directory that lasts as long as the process.
    pub fn memory() -> Self {
        Self::open(Arc::new(Database::memory())).expect("in-memory SQLite accepts the schema")
    }

    fn read<T>(&self, work: impl FnOnce(&Conn) -> database::Result<T>) -> Result<T, String> {
        let connection = self.database.read();
        work(&connection).map_err(|error| self.database.error(error).to_string())
    }

    /// Run one write; the number of rows it changed.
    fn write(&self, op: DirectoryOp) -> Result<usize, String> {
        match self.database.run(Operation::Directory(op)) {
            Ok(Outcome::Directory(DirectoryOutcome::Changed { rows })) => Ok(rows),
            Ok(Outcome::Directory(DirectoryOutcome::Failed { message })) => {
                Err(self.database.error(message).to_string())
            }
            Ok(other) => Err(self
                .database
                .error(format!("unexpected outcome {other:?}"))
                .to_string()),
            Err(error) => Err(error.to_string()),
        }
    }

    /// A player joined: remember the name, address and server. `ip` is
    /// `None` when addresses are not recorded, which also forgets an
    /// address recorded earlier.
    pub fn joined(&self, uuid: &str, name: &str, ip: Option<&str>, server: &str) {
        let result = self.write(DirectoryOp::Joined {
            uuid: uuid.to_owned(),
            name: name.to_owned(),
            ip: ip.map(str::to_owned),
            server: server.to_owned(),
            now: unix_now(),
        });
        if let Err(error) = result {
            eprintln!("rift: cannot record the join of {name}: {error}");
        }
    }

    /// A player left from `server`.
    pub fn left(&self, uuid: &str, server: &str) {
        let result = self.write(DirectoryOp::Left {
            uuid: uuid.to_owned(),
            server: server.to_owned(),
            now: unix_now(),
        });
        if let Err(error) = result {
            eprintln!("rift: cannot record the quit of {uuid}: {error}");
        }
    }

    /// The record for a UUID, or for the player who most recently joined
    /// with a name (case-insensitive).
    pub fn lookup(&self, target: &str) -> Result<Option<Record>, String> {
        if target.is_empty() || target.len() > 64 {
            return Err("a player is a UUID or a name of at most 64 bytes".into());
        }
        self.read(|connection| match uuid_key(target) {
            Some(uuid) => connection
                .query_row(
                    &format!("SELECT {COLUMNS} FROM players WHERE uuid = ?1"),
                    params![uuid],
                    record,
                )
                .optional(),
            None => connection
                .query_row(
                    &format!(
                        "SELECT {COLUMNS} FROM players WHERE name_key = ?1
                         ORDER BY last_seen DESC LIMIT 1"
                    ),
                    params![target.to_ascii_lowercase()],
                    record,
                )
                .optional(),
        })
    }

    /// Records whose name starts with `prefix`, most recently seen first.
    pub fn search(&self, prefix: &str, limit: u32) -> Result<Vec<Record>, String> {
        if prefix.len() > 64 {
            return Err("a name prefix has at most 64 bytes".into());
        }
        if !(1..=MAX_SEARCH).contains(&limit) {
            return Err(format!("limit must be 1..={MAX_SEARCH}"));
        }
        // `_` is an ordinary character of names and a wildcard of LIKE.
        let pattern: String = prefix
            .to_ascii_lowercase()
            .chars()
            .flat_map(|c| match c {
                '%' | '_' | '\\' => vec!['\\', c],
                c => vec![c],
            })
            .chain(['%'])
            .collect();
        self.read(|connection| {
            let like = connection.dialect().like();
            connection.query_map(
                &format!(
                    "SELECT {COLUMNS} FROM players WHERE name_key {like} ?1 ESCAPE '\\'
                     ORDER BY last_seen DESC, name_key LIMIT ?2"
                ),
                params![pattern, limit],
                record,
            )
        })
    }

    /// The language a plugin set for `uuid`, if any.
    pub fn language(&self, uuid: &str) -> Option<String> {
        self.read(|connection| {
            connection
                .query_row(
                    "SELECT language FROM players WHERE uuid = ?1",
                    params![uuid],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
        })
        .ok()
        .flatten()
        .flatten()
    }

    /// Set or clear the language of a known player. False for a player
    /// Rift never saw.
    pub fn set_language(&self, uuid: &str, language: Option<&str>) -> Result<bool, String> {
        self.write(DirectoryOp::SetLanguage {
            uuid: uuid.to_owned(),
            language: language.map(str::to_owned),
        })
        .map(|changed| changed > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALEX: &str = "00000000000000000000000000000001";
    const STEVE: &str = "00000000000000000000000000000002";

    /// A directory in a fresh PostgreSQL schema, when the tests have one.
    fn postgres(test: &str) -> Option<Directory> {
        let location = crate::database::test_postgres_or_skip(test)?;
        Some(Directory::open(Arc::new(Database::connect(&location).unwrap())).unwrap())
    }

    #[test]
    fn joins_and_quits_are_remembered_by_uuid_and_name() {
        joins_and_quits(&Directory::memory());
    }

    #[test]
    fn joins_and_quits_are_remembered_on_postgres() {
        if let Some(directory) = postgres("joins_and_quits_are_remembered_on_postgres") {
            joins_and_quits(&directory);
        }
    }

    fn joins_and_quits(directory: &Directory) {
        assert_eq!(directory.lookup("Alex").unwrap(), None);
        directory.joined(ALEX, "Alex", Some("203.0.113.7"), "lobby");
        directory.joined(STEVE, "Steve_1", None, "lobby");
        let alex = directory.lookup("aLeX").unwrap().unwrap();
        assert_eq!(
            (
                alex.uuid.as_str(),
                alex.name.as_str(),
                alex.last_ip.as_str()
            ),
            (ALEX, "Alex", "203.0.113.7")
        );
        assert_eq!(alex.last_server, "lobby");
        assert!(alex.first_seen > 0 && alex.last_seen >= alex.first_seen);
        assert_eq!(alex.language, None);
        directory.left(ALEX, "survival");
        // A rename keeps the record and the first join.
        directory.joined(ALEX, "Alexa", None, "lobby");
        assert_eq!(directory.lookup("Alex").unwrap(), None);
        let renamed = directory
            .lookup("00000000-0000-0000-0000-000000000001")
            .unwrap()
            .unwrap();
        assert_eq!(renamed.name, "Alexa");
        assert_eq!(renamed.first_seen, alex.first_seen);
        assert_eq!(
            renamed.last_ip, "",
            "the address is not kept without recording"
        );
        assert_eq!(directory.lookup(STEVE).unwrap().unwrap().last_ip, "");
        assert!(directory.lookup("").is_err());
    }

    #[test]
    fn search_matches_prefixes_literally() {
        search_literally(&Directory::memory());
    }

    #[test]
    fn search_matches_prefixes_literally_on_postgres() {
        if let Some(directory) = postgres("search_matches_prefixes_literally_on_postgres") {
            search_literally(&directory);
        }
    }

    fn search_literally(directory: &Directory) {
        directory.joined(ALEX, "Al_ex", None, "lobby");
        directory.joined(STEVE, "Alxex", None, "lobby");
        let names = |prefix: &str| -> Vec<String> {
            directory
                .search(prefix, 10)
                .unwrap()
                .into_iter()
                .map(|record| record.name)
                .collect()
        };
        assert_eq!(names("al_"), ["Al_ex"]);
        assert_eq!(names("AL").len(), 2);
        assert_eq!(names("%"), Vec::<String>::new());
        assert_eq!(directory.search("a", 1).unwrap().len(), 1);
        assert!(directory.search("a", 0).is_err());
        assert!(directory.search("a", MAX_SEARCH + 1).is_err());
    }

    #[test]
    fn languages_belong_to_known_players() {
        languages(&Directory::memory());
    }

    #[test]
    fn languages_belong_to_known_players_on_postgres() {
        if let Some(directory) = postgres("languages_belong_to_known_players_on_postgres") {
            languages(&directory);
        }
    }

    fn languages(directory: &Directory) {
        assert!(!directory.set_language(ALEX, Some("de")).unwrap());
        directory.joined(ALEX, "Alex", None, "lobby");
        assert!(directory.set_language(ALEX, Some("de")).unwrap());
        assert_eq!(directory.language(ALEX).as_deref(), Some("de"));
        directory.joined(ALEX, "Alex", None, "lobby");
        assert_eq!(
            directory.lookup(ALEX).unwrap().unwrap().language.as_deref(),
            Some("de"),
            "a join keeps the language"
        );
        assert!(directory.set_language(ALEX, None).unwrap());
        assert_eq!(directory.language(ALEX), None);
        assert_eq!(language_tag("de-DE").unwrap(), "de_de");
        assert_eq!(language_tag("pt_br").unwrap(), "pt_br");
        assert!(language_tag("d").is_err());
        assert!(language_tag("de de").is_err());
    }
}
