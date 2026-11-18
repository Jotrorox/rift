//! `rift database copy`: move the instance database between the SQLite file
//! and PostgreSQL.
use rift::{
    config::Config,
    database::{Database, Location},
};
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

pub const USAGE: &str = "Usage: rift database copy [--from <file> | --to <file>] [--replace] [config]\n\
    Copies every Rift table between a SQLite instance database and the PostgreSQL\n\
    database that rift.config.database configures (default config ./rift.lua).\n\
    --from <file>  copy from this SQLite file (default: instance_database) into PostgreSQL\n\
    --to <file>    copy from PostgreSQL into this SQLite file, to move back\n\
    --replace      overwrite a target that already holds data\n\
    Stop Rift first: both databases are opened exclusively.";

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

/// Create or migrate every component's tables, as startup does.
fn prepare(database: Database, config: &Config) -> io::Result<Arc<Database>> {
    let database = Arc::new(database);
    rift::permissions::Authority::open(database.clone(), &config.permissions)?;
    rift::instance_store::Store::new(database.clone())?;
    rift::directory::Directory::open(database.clone())?;
    rift::extensions::prepare_database_storage(&database, config)?;
    Ok(database)
}

pub fn run(args: &[String]) -> io::Result<()> {
    let Some((command, args)) = args.split_first() else {
        return Err(invalid(USAGE));
    };
    if command == "--help" || command == "-h" {
        println!("{USAGE}");
        return Ok(());
    }
    if command != "copy" {
        return Err(invalid(USAGE));
    }
    let mut from = None;
    let mut to = None;
    let mut replace = false;
    let mut config_path = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--from" => from = Some(PathBuf::from(args.next().ok_or_else(|| invalid(USAGE))?)),
            "--to" => to = Some(PathBuf::from(args.next().ok_or_else(|| invalid(USAGE))?)),
            "--replace" => replace = true,
            option if option.starts_with('-') => return Err(invalid(USAGE)),
            path if config_path.is_none() => config_path = Some(PathBuf::from(path)),
            _ => return Err(invalid(USAGE)),
        }
    }
    if from.is_some() && to.is_some() {
        return Err(invalid("rift database copy: use --from or --to, not both"));
    }
    let config_path = config_path.unwrap_or_else(|| PathBuf::from(crate::DEFAULT_CONFIG_PATH));
    let config = Config::load_unrestored(&config_path)?;
    let Some(mut postgres) = config.database.clone() else {
        return Err(invalid(format!(
            "{}: rift.config.database does not select backend = \"postgres\"; configure the PostgreSQL database to copy to or from",
            config_path.display()
        )));
    };
    // A copy never waits for a running Rift.
    postgres.standby = false;
    let postgres = Location::Postgres(postgres);
    let (source, target) = match to {
        Some(file) => (postgres, Location::Sqlite(file)),
        None => (
            Location::Sqlite(from.unwrap_or_else(|| config.instance_database.clone())),
            postgres,
        ),
    };
    if let Location::Sqlite(path) = &source
        && !Path::new(path).exists()
    {
        return Err(invalid(format!("{}: no such file", path.display())));
    }
    println!(
        "rift: copying {} into {}",
        source.describe(),
        target.describe()
    );
    let source = prepare(Database::connect(&source)?, &config)?;
    let target = prepare(Database::connect(&target)?, &config)?;
    let report = rift::database::copy::copy(&source, &target, replace)?;
    let total: u64 = report.values().sum();
    for (table, rows) in &report {
        println!("rift: {table}: {rows} rows");
    }
    println!("rift: copied {total} rows into {}", target.label());
    Ok(())
}
