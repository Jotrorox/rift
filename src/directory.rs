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
//! Joins and quits are written off the session's task; lookups read the
//! table directly.
use crate::database::{self, Conn, Database, Dialect, OptionalExtension, Row, params};
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
const MARIADB_SCHEMA: &str = "CREATE TABLE players (
    uuid VARCHAR(255) PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    name_key VARCHAR(255) NOT NULL,
    first_seen BIGINT NOT NULL,
    last_seen BIGINT NOT NULL,
    last_ip TEXT NOT NULL DEFAULT '',
    last_server TEXT NOT NULL DEFAULT '',
    language TEXT
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_nopad_bin;
CREATE INDEX players_name ON players (name_key, last_seen);";

/// Most records one `search` returns.
pub const MAX_SEARCH: u32 = 100;

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
        database.migrate(COMPONENT, SCHEMA_VERSION, |dialect, _| {
            match dialect {
                Dialect::Sqlite => SCHEMA,
                Dialect::Postgres => POSTGRES_SCHEMA,
                Dialect::Mariadb => MARIADB_SCHEMA,
            }
            .to_owned()
        })?;
        Ok(Self { database })
    }

    /// A directory that lasts as long as the process.
    pub fn memory() -> Self {
        Self::open(Arc::new(Database::memory())).expect("in-memory SQLite accepts the schema")
    }

    fn read<T>(&self, work: impl FnOnce(&Conn) -> database::Result<T>) -> Result<T, String> {
        let connection = self.database.lock();
        work(&connection).map_err(|error| self.database.error(error).to_string())
    }

    /// A player joined: remember the name, address and server. `ip` is
    /// `None` when addresses are not recorded, which also forgets an
    /// address recorded earlier.
    pub fn joined(&self, uuid: &str, name: &str, ip: Option<&str>, server: &str) {
        let now = unix_now() as i64;
        let result = self.read(|connection| {
            let greatest = connection.dialect().greatest();
            connection.execute(
                &format!(
                    "INSERT INTO players (uuid, name, name_key, first_seen, last_seen, last_ip, last_server)
                     VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6)
                     ON CONFLICT(uuid) DO UPDATE SET name = excluded.name, name_key = excluded.name_key,
                         last_seen = {greatest}(players.last_seen, excluded.last_seen),
                         last_ip = excluded.last_ip, last_server = excluded.last_server"
                ),
                params![uuid, name, name.to_ascii_lowercase(), now, ip.unwrap_or_default(), server],
            )
        });
        if let Err(error) = result {
            eprintln!("rift: cannot record the join of {name}: {error}");
        }
    }

    /// A player left from `server`.
    pub fn left(&self, uuid: &str, server: &str) {
        let now = unix_now() as i64;
        let result = self.read(|connection| {
            let greatest = connection.dialect().greatest();
            connection.execute(
                &format!(
                    "UPDATE players SET last_seen = {greatest}(last_seen, ?2), last_server = ?3 WHERE uuid = ?1"
                ),
                params![uuid, now, server],
            )
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
            let like = connection.dialect().like("name_key", "?1");
            connection.query_map(
                &format!(
                    "SELECT {COLUMNS} FROM players WHERE {like} ESCAPE '\\'
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
        self.read(|connection| {
            connection.execute(
                "UPDATE players SET language = ?2 WHERE uuid = ?1",
                params![uuid, language],
            )
        })
        .map(|changed| changed > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALEX: &str = "00000000000000000000000000000001";
    const STEVE: &str = "00000000000000000000000000000002";

    /// A directory in a fresh server schema, when the tests have one.
    fn server(test: &str) -> Option<Directory> {
        let location = crate::database::test_server_or_skip(test)?;
        Some(Directory::open(Arc::new(Database::connect(&location).unwrap())).unwrap())
    }

    #[test]
    fn joins_and_quits_are_remembered_by_uuid_and_name() {
        joins_and_quits(&Directory::memory());
    }

    #[test]
    fn joins_and_quits_are_remembered_on_a_server() {
        if let Some(directory) = server("joins_and_quits_are_remembered_on_a_server") {
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
    fn search_matches_prefixes_literally_on_a_server() {
        if let Some(directory) = server("search_matches_prefixes_literally_on_a_server") {
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
    fn languages_belong_to_known_players_on_a_server() {
        if let Some(directory) = server("languages_belong_to_known_players_on_a_server") {
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
