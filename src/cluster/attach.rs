//! Wiring a cluster into a starting Rift. Each part of Rift that the cluster
//! replicates or federates has one function here or beside it; `main`
//! calls them in order before `runtime::serve` opens the stores.
//!
//! [`database`] comes first: it opens the instance database for
//! replication, starts the peer with it (and the components the other
//! steps register), installs the cluster's [`ClusterExecutor`] and waits
//! until the replica holds the cluster's state at this build's schema. The
//! stores (instance registry, permission authority, player directory,
//! extension store) open afterwards, check their schema instead of
//! migrating it, and follow the replica through their apply hooks.
use super::{Cluster, Components, Options, Settings, operations::ClusterExecutor, replicated};
use crate::{
    config::Config,
    database::{Database, Mode},
};
use std::{
    io,
    sync::{Arc, OnceLock},
    time::Duration,
};

static DATABASE: OnceLock<Arc<Database>> = OnceLock::new();

/// A started peer and the instance database it replicates.
pub struct Attached {
    pub cluster: Cluster,
    pub database: Arc<Database>,
}

/// The instance database [`database`] attached, for the stores that
/// `runtime::serve` opens; `None` on a single Rift.
pub fn attached_database() -> Option<Arc<Database>> {
    DATABASE.get().cloned()
}

/// Start this peer on the instance database of `config` (see the module
/// documentation). `source` is the text of `rift.lua`, whose hash the
/// handshakes compare; `components` are those of the other steps
/// (presence, bus), registered before the peer starts.
pub async fn database(
    config: &Config,
    settings: Settings,
    source: Option<&[u8]>,
    components: Components,
) -> io::Result<Attached> {
    check(config)?;
    let path = config.instance_database.clone();
    let database =
        tokio::task::spawn_blocking(move || Database::open_with(&path, Mode::Replicated))
            .await
            .map_err(io::Error::other)??;
    let database = Arc::new(database);
    let mut options = Options::from_config(config, source)?;
    options.components = components;
    options.database = Some(database.clone());
    let cluster = Cluster::start(settings, options).await?;
    database.set_executor(Arc::new(ClusterExecutor::new(&cluster)));
    wait_for_state(&cluster).await?;
    let _ = DATABASE.set(database.clone());
    Ok(Attached { cluster, database })
}

/// What a cluster cannot run with.
fn check(config: &Config) -> io::Result<()> {
    if config.database.is_some() {
        return Err(super::invalid(
            "cluster: database.backend = \"postgres\" or \"mariadb\" cannot be combined with cluster.toml; a cluster replicates SQLite (run `rift database copy --to rift.sqlite3` first)",
        ));
    }
    if let Some(path) = crate::extensions::storage::file_target(config) {
        return Err(super::invalid(format!(
            "extensions.storage.path ({}): a store in a file of its own cannot be replicated; set extensions.storage.database = true instead (`rift cluster init` copies the file's entries into the instance database)",
            path.display()
        )));
    }
    Ok(())
}

/// Wait until the replica holds the cluster's state (a new peer installs a
/// snapshot once a member adds it) and every component's schema is the one
/// this build writes (the leader migrates once every member supports it).
async fn wait_for_state(cluster: &Cluster) -> io::Result<()> {
    let mut announced: Option<String> = None;
    loop {
        let state = cluster.state();
        let reason = if state.applied_index() == 0 {
            Some(format!(
                "{} holds no cluster state yet; it installs a snapshot once a member runs `rift admin cluster add {} {}`",
                state.database().label(),
                cluster.name(),
                cluster.settings().advertise
            ))
        } else {
            let database = state.database().clone();
            let behind = tokio::task::spawn_blocking(move || replicated::schema_behind(&database))
                .await
                .map_err(io::Error::other)??;
            (!behind.is_empty()).then(|| {
                let list: Vec<String> = behind
                    .iter()
                    .map(|(component, from, to)| format!("{component} {from} -> {to}"))
                    .collect();
                format!(
                    "waiting for the leader to migrate the schema ({}); it does once every peer supports it",
                    list.join(", ")
                )
            })
        };
        let Some(reason) = reason else {
            return Ok(());
        };
        if announced.as_deref() != Some(reason.as_str()) {
            log_info!("cluster: {reason}");
            announced = Some(reason);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
