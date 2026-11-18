//! Copy every Rift table from one instance database into another, in either
//! direction between SQLite and a server (PostgreSQL or MariaDB)
//! (`rift database copy`).
//!
//! Both databases must already have Rift's tables at the same schema
//! versions; the caller opens every component on both sides first, which
//! creates and migrates them. Tables are discovered from the databases, not
//! listed here, so components added later are copied too. Rows keep their
//! ids; PostgreSQL identity columns and MariaDB `AUTO_INCREMENT` columns
//! continue after the copied ones.
use super::{Conn, Database, Dialect, Kind, Result, Value, params};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};

/// Rows copied per table.
pub type Report = BTreeMap<String, u64>;

/// Tables a freshly opened database already has rows in: the schema
/// versions, the permission revision and the `default` group.
const SEEDED: &[&str] = &["rift_schema", "perm_meta", "perm_subject"];

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn tables(connection: &Conn) -> Result<BTreeSet<String>> {
    let sql = match connection.dialect() {
        Dialect::Sqlite => {
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'"
        }
        Dialect::Postgres => {
            "SELECT table_name::text FROM information_schema.tables
             WHERE table_schema = current_schema() AND table_type = 'BASE TABLE'"
        }
        Dialect::Mariadb => {
            "SELECT TABLE_NAME FROM information_schema.TABLES
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_TYPE = 'BASE TABLE'"
        }
    };
    Ok(connection
        .query_map(sql, params![], |row| row.get(0))?
        .into_iter()
        .collect())
}

/// The storage class a declared column type converts to.
fn kind(declared: &str) -> Kind {
    let declared = declared.to_ascii_uppercase();
    if declared.contains("INT") {
        Kind::Integer
    } else if declared.contains("BLOB") || declared.contains("BYTEA") {
        Kind::Blob
    } else if declared.contains("REAL") || declared.contains("FLOA") || declared.contains("DOUB") {
        Kind::Real
    } else {
        Kind::Text
    }
}

/// The columns of `table` in order, with their storage classes.
fn columns(connection: &Conn, table: &str) -> Result<Vec<(String, Kind)>> {
    let rows = match connection.dialect() {
        Dialect::Sqlite => connection.query_map(
            &format!(
                "SELECT name, type FROM pragma_table_info({})",
                sql_text(table)
            ),
            params![],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?,
        Dialect::Postgres => connection.query_map(
            "SELECT column_name::text, data_type::text FROM information_schema.columns
             WHERE table_schema = current_schema() AND table_name = ?1 ORDER BY ordinal_position",
            params![table],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?,
        Dialect::Mariadb => connection.query_map(
            "SELECT COLUMN_NAME, DATA_TYPE FROM information_schema.COLUMNS
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?1 ORDER BY ORDINAL_POSITION",
            params![table],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?,
    };
    Ok(rows
        .into_iter()
        .map(|(name, declared)| {
            let kind = kind(&declared);
            (name, kind)
        })
        .collect())
}

fn sql_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The tables `table` references with foreign keys.
fn references(connection: &Conn, table: &str) -> Result<BTreeSet<String>> {
    let referenced = match connection.dialect() {
        Dialect::Sqlite => connection.query_map(
            &format!(
                "SELECT DISTINCT \"table\" FROM pragma_foreign_key_list({})",
                sql_text(table)
            ),
            params![],
            |row| row.get(0),
        )?,
        Dialect::Postgres => connection.query_map(
            "SELECT DISTINCT target.relname::text FROM pg_constraint c
             JOIN pg_class source ON source.oid = c.conrelid
             JOIN pg_class target ON target.oid = c.confrelid
             WHERE c.contype = 'f' AND source.relname = ?1
               AND source.relnamespace =
                   (SELECT oid FROM pg_namespace WHERE nspname = current_schema())",
            params![table],
            |row| row.get(0),
        )?,
        Dialect::Mariadb => connection.query_map(
            "SELECT DISTINCT REFERENCED_TABLE_NAME FROM information_schema.KEY_COLUMN_USAGE
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?1
               AND REFERENCED_TABLE_NAME IS NOT NULL",
            params![table],
            |row| row.get(0),
        )?,
    };
    Ok(referenced.into_iter().collect())
}

/// `tables` with every table after the tables it references.
fn ordered(connection: &Conn, tables: &BTreeSet<String>) -> Result<Vec<String>> {
    let mut pending: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for table in tables {
        let mut references = references(connection, table)?;
        references.retain(|other| other != table && tables.contains(other));
        pending.insert(table.clone(), references);
    }
    let mut order = Vec::new();
    while !pending.is_empty() {
        let ready: Vec<String> = pending
            .iter()
            .filter(|(_, references)| references.is_empty())
            .map(|(table, _)| table.clone())
            .collect();
        if ready.is_empty() {
            // A cycle; insert the rest in name order and let the database judge.
            order.extend(pending.into_keys());
            break;
        }
        for table in ready {
            pending.remove(&table);
            for references in pending.values_mut() {
                references.remove(&table);
            }
            order.push(table);
        }
    }
    Ok(order)
}

fn versions(connection: &Conn) -> Result<BTreeMap<String, i64>> {
    Ok(connection
        .query_map(
            "SELECT component, version FROM rift_schema",
            params![],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?
        .into_iter()
        .collect())
}

fn count(connection: &Conn, table: &str) -> Result<i64> {
    connection.query_row(
        &format!("SELECT count(*) FROM {}", quote(table)),
        params![],
        |row| row.get(0),
    )
}

/// Copy every table of `source` into `target`. A target holding more than a
/// freshly opened database does is refused unless `replace`, which deletes
/// its rows first. Everything happens in one target transaction.
pub fn copy(source: &Database, target: &Database, replace: bool) -> io::Result<Report> {
    let error = |error: super::Error| io::Error::other(error.to_string());
    let from = source.lock();
    let mut to = target.lock();
    let source_versions = versions(&from).map_err(error)?;
    let target_versions = versions(&to).map_err(error)?;
    for (component, version) in &source_versions {
        match target_versions.get(component) {
            Some(target) if target == version => {}
            Some(target) => {
                return Err(io::Error::other(format!(
                    "schema version {version} of {component} in the source differs from {target} in the target; open both with the same Rift release first"
                )));
            }
            None => {
                return Err(io::Error::other(format!(
                    "the target has no {component} tables yet; start Rift once with the target's configuration, or enable the same features, then copy again"
                )));
            }
        }
    }
    let source_tables = tables(&from).map_err(error)?;
    let target_tables = tables(&to).map_err(error)?;
    let copied: BTreeSet<String> = source_tables
        .iter()
        .filter(|table| table.as_str() != "rift_schema")
        .cloned()
        .collect();
    if let Some(missing) = copied.iter().find(|table| !target_tables.contains(*table)) {
        return Err(io::Error::other(format!(
            "the target has no table {missing}; start Rift once with the target's configuration, then copy again"
        )));
    }
    if !replace {
        for table in &target_tables {
            let rows = count(&to, table).map_err(error)?;
            let seeded = SEEDED.contains(&table.as_str());
            let fresh = match table.as_str() {
                "perm_subject" => rows <= 1,
                _ => seeded || rows == 0,
            };
            if !fresh {
                return Err(io::Error::other(format!(
                    "the target already holds data ({rows} rows in {table}); pass --replace to overwrite it"
                )));
            }
        }
    }
    let order = ordered(&to, &copied).map_err(error)?;
    let dialect = to.dialect();
    let transaction = to.transaction_immediate().map_err(error)?;
    for table in order.iter().rev() {
        transaction
            .execute(&format!("DELETE FROM {}", quote(table)), params![])
            .map_err(error)?;
    }
    let mut report = Report::new();
    for table in &order {
        let target_columns: BTreeMap<String, Kind> = columns(&transaction, table)
            .map_err(error)?
            .into_iter()
            .collect();
        let source_columns = columns(&from, table).map_err(error)?;
        let mut names = Vec::new();
        let mut kinds = Vec::new();
        for (name, _) in &source_columns {
            let Some(kind) = target_columns.get(name) else {
                return Err(io::Error::other(format!(
                    "the target's table {table} has no column {name}"
                )));
            };
            names.push(quote(name));
            kinds.push(*kind);
        }
        let list = names.join(", ");
        let select = format!("SELECT {list} FROM {}", quote(table));
        let placeholders: Vec<String> = (1..=names.len()).map(|n| format!("?{n}")).collect();
        let insert = format!(
            "INSERT INTO {} ({list}) VALUES ({})",
            quote(table),
            placeholders.join(", ")
        );
        let mut rows = 0u64;
        from.each(&select, params![], |row| -> Result<bool> {
            let values: Vec<Value> = row
                .values()
                .iter()
                .zip(&kinds)
                .map(|(value, kind)| match value {
                    Value::Null(_) => Value::Null(*kind),
                    value => value.clone(),
                })
                .collect();
            transaction.execute(&insert, &values)?;
            rows += 1;
            Ok(true)
        })
        .map_err(error)?;
        report.insert(table.clone(), rows);
    }
    // MariaDB moves AUTO_INCREMENT past every id inserted explicitly.
    if dialect == Dialect::Postgres {
        // Identity columns continue after the copied ids.
        let identities = transaction
            .query_map(
                "SELECT table_name::text, column_name::text FROM information_schema.columns
                 WHERE table_schema = current_schema() AND is_identity = 'YES'",
                params![],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(error)?;
        for (table, column) in identities {
            if !copied.contains(&table) {
                continue;
            }
            transaction
                .query_row(
                    &format!(
                        "SELECT setval(pg_get_serial_sequence({}, {}), COALESCE(MAX({}), 0) + 1, false) FROM {}",
                        sql_text(&quote(&table)),
                        sql_text(&column),
                        quote(&column),
                        quote(&table)
                    ),
                    params![],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .map_err(error)?;
        }
    }
    transaction.commit().map_err(error)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(database: &Database) {
        database
            .migrate("copy_test", 1, |dialect, _| match dialect {
                Dialect::Sqlite => "CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                    CREATE TABLE child (id INTEGER PRIMARY KEY,
                        parent INTEGER NOT NULL REFERENCES parent(id), note TEXT, data BLOB);"
                    .into(),
                Dialect::Postgres => "CREATE TABLE parent (id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, name TEXT COLLATE \"C\" NOT NULL);
                    CREATE TABLE child (id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
                        parent BIGINT NOT NULL REFERENCES parent(id), note TEXT, data BYTEA);"
                    .into(),
                Dialect::Mariadb => "CREATE TABLE parent (id BIGINT AUTO_INCREMENT PRIMARY KEY, name TEXT NOT NULL)
                        ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_nopad_bin;
                    CREATE TABLE child (id BIGINT AUTO_INCREMENT PRIMARY KEY,
                        parent BIGINT NOT NULL, note TEXT, data LONGBLOB,
                        FOREIGN KEY (parent) REFERENCES parent(id))
                        ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_nopad_bin;"
                    .into(),
            })
            .unwrap();
    }

    fn roundtrip(source: &Database, target: &Database) {
        seed(source);
        seed(target);
        {
            let connection = source.lock();
            for (id, name) in [(3i64, "c"), (7, "g")] {
                connection
                    .execute(
                        "INSERT INTO parent (id, name) VALUES (?1, ?2)",
                        params![id, name],
                    )
                    .unwrap();
            }
            connection
                .execute(
                    "INSERT INTO child (id, parent, note, data) VALUES (?1, ?2, ?3, ?4)",
                    params![1i64, 7i64, None::<String>, Some(vec![0u8, 255])],
                )
                .unwrap();
        }
        let report = copy(source, target, false).unwrap();
        assert_eq!(report["parent"], 2);
        assert_eq!(report["child"], 1);
        let connection = target.lock();
        let child = connection
            .query_row(
                "SELECT parent, note, data FROM child WHERE id = 1",
                params![],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(child, (7, None, vec![0, 255]));
        // New rows continue after the copied ids.
        let id: i64 = connection
            .query_row(
                "INSERT INTO parent (name) VALUES ('new') RETURNING id",
                params![],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(id, 8);
        drop(connection);
        let error = copy(source, target, false).unwrap_err().to_string();
        assert!(error.contains("--replace"), "{error}");
        copy(source, target, true).unwrap();
        assert_eq!(count(&target.lock(), "parent").unwrap(), 2);
    }

    #[test]
    fn sqlite_copies_into_sqlite() {
        roundtrip(&Database::memory(), &Database::memory());
    }

    #[test]
    fn sqlite_copies_into_a_server_and_back() {
        let Some(location) =
            super::super::test_server_or_skip("sqlite_copies_into_a_server_and_back")
        else {
            return;
        };
        let server = Database::connect(&location).unwrap();
        roundtrip(&Database::memory(), &server);
        let back = Database::memory();
        seed(&back);
        let report = copy(&server, &back, false).unwrap();
        assert_eq!(report["parent"], 2);
    }

    #[test]
    fn components_missing_in_the_target_are_named() {
        let source = Database::memory();
        seed(&source);
        let error = copy(&source, &Database::memory(), false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("copy_test"), "{error}");
    }
}
