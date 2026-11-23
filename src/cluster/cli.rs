//! `rift cluster init|recover` and `rift witness`.
use super::{
    Cluster, PeerNode,
    log_store::LogStore,
    node::Options,
    operations::ClusterExecutor,
    settings::{self, Settings},
    state::{self, Entry, FeaturesEntry, PeerEntry},
    transport::Offline,
};
use crate::{
    config::Config,
    database::{
        Database, Mode, Operation,
        operation::{MigrateOp, latest_versions},
    },
};
use openraft::ServerState;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub const USAGE: &str = "Usage: rift cluster init --peer <name> [--from-backup <file>] [--extension-store <file>] [--config <path>]\n\
    \x20   Create a cluster whose only voter is this peer (cluster.toml beside the configuration names it).\n\
    \x20   Its state starts as this Rift's instance database (SQLite; a PostgreSQL or MariaDB\n\
    \x20   installation runs `rift database copy --to rift.sqlite3` first).\n\
    \x20   With --from-backup, its state starts as a `rift admin cluster backup` copy.\n\
    \x20   With --extension-store, the entries of the extension store file this Rift used move into\n\
    \x20   the instance database; the configuration must already say extensions.storage.database = true.\n\
    rift cluster recover --peer <name> --i-understand-data-loss [--config <path>]\n\
    \x20   Make this peer the only voter when the others are gone for good. Writes only\n\
    \x20   the lost peers acknowledged are lost. Run it while Rift is stopped.\n\
    rift witness [--config <path>]\n\
    \x20   Run this host as a witness: a voter that takes no players (see docs/clustering.md).";

/// The raft id of the peer that initializes a cluster.
const FIRST_RAFT_ID: u64 = 1;

struct Flags {
    peer: Option<String>,
    config: PathBuf,
    backup: Option<PathBuf>,
    extension_store: Option<PathBuf>,
    data_loss: bool,
}

fn flags(args: &[String]) -> io::Result<Flags> {
    let mut flags = Flags {
        peer: None,
        config: PathBuf::from("rift.lua"),
        backup: None,
        extension_store: None,
        data_loss: false,
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .cloned()
                .ok_or_else(|| super::invalid(format!("{arg} needs a value\n{USAGE}")))
        };
        match arg.as_str() {
            "--peer" => flags.peer = Some(value()?),
            "--config" => flags.config = PathBuf::from(value()?),
            "--from-backup" => flags.backup = Some(PathBuf::from(value()?)),
            "--extension-store" => flags.extension_store = Some(PathBuf::from(value()?)),
            "--i-understand-data-loss" => flags.data_loss = true,
            _ => return Err(super::invalid(USAGE)),
        }
    }
    Ok(flags)
}

fn load_settings(config: &Path, peer: Option<&str>) -> io::Result<Settings> {
    let settings = Settings::load_beside(config)?.ok_or_else(|| {
        super::invalid(format!(
            "{}: missing; a cluster peer needs its cluster.toml (see docs/clustering.md)",
            Settings::path_beside(config).display()
        ))
    })?;
    if let Some(peer) = peer
        && peer != settings.peer
    {
        return Err(super::invalid(format!(
            "--peer {peer} does not match peer = {:?} in {}",
            settings.peer,
            Settings::path_beside(config).display()
        )));
    }
    Ok(settings)
}

/// `rift cluster ...`
pub async fn run(args: &[String]) -> io::Result<()> {
    match args.first().map(String::as_str) {
        Some("init") => init(flags(&args[1..])?).await,
        Some("recover") => recover(flags(&args[1..])?),
        Some("--help" | "-h") | None => {
            println!("{USAGE}");
            Ok(())
        }
        _ => Err(super::invalid(USAGE)),
    }
}

async fn init(flags: Flags) -> io::Result<()> {
    let peer = flags
        .peer
        .ok_or_else(|| super::invalid(format!("rift cluster init needs --peer <name>\n{USAGE}")))?;
    let settings = load_settings(&flags.config, Some(&peer))?;
    // The configuration is read as this peer reads it: `rift.peer` is its
    // name, and servers and groups may name peer-nodes, which are checked
    // once the peer runs. Init reports servers still on `local` itself.
    settings::set_local_peer(&settings.peer);
    settings::report_placement();
    let config = load_config(&flags.config)?;
    if config
        .as_ref()
        .is_some_and(|config| config.database.is_some())
    {
        return Err(super::invalid(
            "rift cluster init: a cluster replicates SQLite; copy the instance database with `rift database copy --to rift.sqlite3`, remove database.backend from the configuration, then run this again",
        ));
    }
    // Init makes this rift.lua the first deployment, so it must already be
    // what the cluster runs: a store in a file of its own is refused here,
    // before anything changes, rather than when the peer starts.
    let store_file = match &config {
        Some(config) => {
            crate::extensions::storage::init_store_file(config, flags.extension_store.as_deref())?
        }
        None => match &flags.extension_store {
            Some(file) => {
                return Err(super::invalid(format!(
                    "--extension-store {}: {} is missing",
                    file.display(),
                    flags.config.display()
                )));
            }
            None => None,
        },
    };
    if store_file.is_some() && flags.backup.is_some() {
        return Err(super::invalid(
            "--extension-store cannot be combined with --from-backup: the backup holds the cluster's extension store already",
        ));
    }
    let database_path = instance_database(config.as_ref(), &settings);
    std::fs::create_dir_all(&settings.directory)?;
    let log = LogStore::open(&settings.raft_file())?;
    if !log.is_pristine()? || log.identity()?.raft_id.is_some() {
        return Err(super::invalid(format!(
            "{}: this peer already belongs to a cluster",
            settings.raft_file().display()
        )));
    }
    match &flags.backup {
        Some(backup) => state::prepare_from_backup(&database_path, backup)?,
        None if state::has_applied(&database_path)? => {
            return Err(super::invalid(format!(
                "{}: holds the state of another cluster; move it away first",
                database_path.display()
            )));
        }
        None => {
            if let Some(config) = &config {
                let database = Database::open(&database_path)?;
                // A store in a file of its own cannot be replicated: its
                // entries move into the instance database, the first state.
                if let Some(file) = &store_file {
                    let count =
                        crate::extensions::storage::copy_store_file(config, file, &database)?;
                    println!(
                        "rift: copied {count} entries of the extension store {} into {}; the file is no longer used and can be removed once the cluster runs",
                        file.display(),
                        database_path.display()
                    );
                }
                // Servers on this host become servers on this peer's node,
                // which its embedded agent serves (unless `nodes.<peer>`
                // names a `rift node` agent of its own).
                let mut config = config.clone();
                super::peer_nodes::add(
                    &mut config.nodes,
                    &BTreeMap::from([(settings.peer.clone(), super::peer_nodes::own(&settings))]),
                );
                let store = crate::instance_store::Store::new(Arc::new(database))?;
                for converted in store.convert_local_instances(&config, &settings.peer)? {
                    print_converted(&settings, &config, &converted);
                }
            }
        }
    }
    if let Some(config) = &config
        && let Err(error) = config.check_cluster_placement()
    {
        println!("rift: change the configuration before starting this peer: {error}");
    }
    let mut id = [0u8; 16];
    aws_lc_rs::rand::fill(&mut id).map_err(|_| io::Error::other("random source failed"))?;
    let cluster_id = super::hex(&id);
    log.adopt(&cluster_id, FIRST_RAFT_ID, &settings.peer)?;
    drop(log);
    let database = Arc::new(Database::open_with(&database_path, Mode::Replicated)?);
    let shared = config
        .as_ref()
        .and_then(|config| config.cluster.clone())
        .unwrap_or_default();
    let mut options = Options::new(shared);
    // The same components as every peer, so the first features entry
    // enables their entry kinds.
    options.components = super::attach::cluster_components(config.as_ref())?;
    options.database = Some(database.clone());
    options.transport = Some(Arc::new(Offline::default()));
    // Nothing is reachable, so no token is ever checked.
    options.token = Some("offline-initialization".to_owned());
    let cluster = Cluster::start(settings.clone(), options).await?;
    database.set_executor(Arc::new(ClusterExecutor::new(&cluster)));
    let mut result = initialize(&cluster).await;
    if result.is_ok() {
        // Bus-login tokens this Rift issued stay valid on every peer.
        result = match super::bus::import_key(&cluster, &flags.config).await {
            Ok(true) => {
                println!(
                    "rift: moved the bus-login key into the cluster; every peer derives tokens from it"
                );
                Ok(())
            }
            Ok(false) => Ok(()),
            Err(error) => Err(error),
        };
    }
    if result.is_ok() {
        // Deployment history, audit trail and template folders.
        result = super::files::import(&cluster, &flags.config)
            .await
            .map(|summary| println!("rift: moved into the cluster: {summary:?}"));
    }
    cluster.shutdown().await;
    result?;
    println!(
        "rift: cluster {cluster_id} created; {} is its only voter. Start Rift, then add peers with: rift admin cluster add <name> <host:port>",
        settings.peer
    );
    Ok(())
}

/// Commit the first membership, this peer's row and the features entry.
pub(crate) async fn initialize(cluster: &Cluster) -> io::Result<()> {
    let raft = cluster.raft().ok_or_else(super::node::not_joined)?;
    let settings = cluster.settings();
    let node = PeerNode {
        name: settings.peer.clone(),
        address: settings.advertise.clone(),
    };
    raft.initialize(BTreeMap::from([(FIRST_RAFT_ID, node)]))
        .await
        .map_err(|error| io::Error::other(format!("cluster: initialize: {error}")))?;
    raft.wait(Some(Duration::from_secs(30)))
        .metrics(
            |metrics| metrics.state == ServerState::Leader,
            "this peer leads",
        )
        .await
        .map_err(|error| io::Error::other(format!("cluster: initialize: {error}")))?;
    let added = cluster
        .propose(Entry::Peer(PeerEntry::Added {
            v: 1,
            name: settings.peer.clone(),
            address: settings.advertise.clone(),
            witness: settings.witness,
        }))
        .await?
        .into_result()
        .map_err(io::Error::other)?;
    if added["raft_id"].as_u64() != Some(FIRST_RAFT_ID) {
        return Err(io::Error::other(format!(
            "cluster: the first peer got raft id {}, expected {FIRST_RAFT_ID}",
            added["raft_id"]
        )));
    }
    let kinds: BTreeSet<String> = cluster.links().identity().kinds.iter().cloned().collect();
    cluster
        .propose(Entry::Features(FeaturesEntry {
            v: 1,
            kinds,
            settings: BTreeMap::new(),
        }))
        .await?
        .into_result()
        .map_err(io::Error::other)?;
    record_schemas(cluster).await?;
    compact_first_log(cluster).await
}

/// Record every component's schema version with a `migrate` entry: a no-op
/// for components the instance database holds already, the creation of
/// those it does not. Peers that start later check their schema against
/// these.
async fn record_schemas(cluster: &Cluster) -> io::Result<()> {
    let now = super::wall_clock_ms() / 1000;
    for (component, version) in latest_versions() {
        let outcome = cluster
            .run_operation(Operation::Migrate(MigrateOp {
                component: component.to_owned(),
                version,
                now,
            }))
            .await?;
        if outcome.failed() {
            return Err(io::Error::other(format!(
                "cluster: {component} schema version {version}: {outcome:?}"
            )));
        }
    }
    Ok(())
}

/// Snapshot the first state and purge the log behind it. The first state
/// is the instance database as it was, which no log entry holds, so every
/// peer added later must start from a snapshot, never from entry 1.
async fn compact_first_log(cluster: &Cluster) -> io::Result<()> {
    let raft = cluster.raft().ok_or_else(super::node::not_joined)?;
    let applied = cluster.state().applied_index();
    let failed = |error: String| io::Error::other(format!("cluster: {error}"));
    raft.trigger()
        .snapshot()
        .await
        .map_err(|error| failed(error.to_string()))?;
    raft.wait(Some(Duration::from_secs(30)))
        .metrics(
            |metrics| {
                metrics
                    .snapshot
                    .is_some_and(|snapshot| snapshot.index >= applied)
            },
            "the first snapshot is built",
        )
        .await
        .map_err(|error| failed(error.to_string()))?;
    raft.trigger()
        .purge_log(applied)
        .await
        .map_err(|error| failed(error.to_string()))?;
    raft.wait(Some(Duration::from_secs(30)))
        .metrics(
            |metrics| metrics.purged.is_some_and(|purged| purged.index >= applied),
            "the log behind the first snapshot is purged",
        )
        .await
        .map_err(|error| failed(error.to_string()))?;
    Ok(())
}

/// Say where an instance that moved to the peer's node keeps its files.
fn print_converted(
    settings: &Settings,
    config: &Config,
    converted: &crate::instance_store::ConvertedInstance,
) {
    let peer = &settings.peer;
    let root = converted
        .from
        .to_str()
        .zip(converted.to.to_str())
        .and_then(|(from, to)| from.strip_suffix(to))
        .map(|root| root.trim_end_matches(['/', '\\']).to_owned())
        .filter(|root| !root.is_empty());
    let embedded = config.nodes.get(peer).is_none_or(|node| node.peer);
    if !embedded {
        // `nodes.<peer>`: a `rift node` agent of its own, with its --root.
        match root {
            Some(root) => println!(
                "rift: instance {} now runs on node {peer}; run that node's agent with --root {root} to keep its folder {}",
                converted.name,
                converted.from.display()
            ),
            None => println!(
                "rift: instance {} now runs on node {peer}, in {} below that node's --root; move its folder {} there",
                converted.name,
                converted.to.display(),
                converted.from.display()
            ),
        }
        return;
    }
    // The embedded agent keeps its servers below the peer's directory.
    let directory = settings.directory.join(&converted.to);
    let same = |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    if same(&converted.from, &directory) {
        println!(
            "rift: instance {} now runs on node {peer} (this peer's own node), in its folder {}",
            converted.name,
            converted.from.display()
        );
    } else {
        println!(
            "rift: instance {} now runs on node {peer} (this peer's own node), in {}; move its folder {} there before starting this peer",
            converted.name,
            directory.display(),
            converted.from.display()
        );
    }
}

/// The configuration beside `cluster.toml`, if there is one.
fn load_config(path: &Path) -> io::Result<Option<Config>> {
    match Config::load(path) {
        Ok(config) => Ok(Some(config)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The instance database: `instance_database` of the configuration, or
/// `rift.sqlite3` beside `cluster.toml`.
fn instance_database(config: Option<&Config>, settings: &Settings) -> PathBuf {
    config.map_or_else(
        || settings.default_database(),
        |config| config.instance_database.clone(),
    )
}

fn recover(flags: Flags) -> io::Result<()> {
    if !flags.data_loss {
        return Err(super::invalid(
            "rift cluster recover can lose writes that only the lost peers acknowledged; add --i-understand-data-loss",
        ));
    }
    let peer = flags.peer.ok_or_else(|| {
        super::invalid(format!("rift cluster recover needs --peer <name>\n{USAGE}"))
    })?;
    let settings = load_settings(&flags.config, Some(&peer))?;
    let log = LogStore::open(&settings.raft_file())?;
    let identity = log.identity()?;
    let raft_id = identity
        .raft_id
        .ok_or_else(|| super::invalid("this peer never joined a cluster; nothing to recover"))?;
    log.recover_single(
        raft_id,
        PeerNode {
            name: settings.peer.clone(),
            address: settings.advertise.clone(),
        },
    )?;
    println!(
        "rift: {} (raft id {raft_id}) is now the only voter of cluster {}. Start Rift, remove the lost peers with rift admin cluster remove <name>, and add new ones.",
        settings.peer,
        identity.cluster_id.as_deref().unwrap_or("?")
    );
    Ok(())
}

/// `rift witness`: a voter without listeners, until SIGINT or SIGTERM
/// (Ctrl-C on Windows), as the proxy stops.
pub async fn witness(args: &[String]) -> io::Result<()> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let flags = flags(args)?;
    // Registered first, so a stop that arrives while the peer starts is
    // not lost (and does not kill the process without a clean shutdown).
    let mut stop = Stop::new()?;
    let mut settings = load_settings(&flags.config, None)?;
    settings.witness = true;
    settings::set_local_peer(&settings.peer);
    // The shared settings come from rift.lua when this host has one.
    let config = load_config(&flags.config)?;
    let shared = config
        .as_ref()
        .and_then(|config| config.cluster.clone())
        .unwrap_or_default();
    // A witness holds the whole state machine: the instance database.
    let database = Arc::new(Database::open_with(
        &instance_database(config.as_ref(), &settings),
        Mode::Replicated,
    )?);
    let name = settings.peer.clone();
    let mut options = Options::new(shared);
    // A witness never uses the forwarding secret or node tokens and need
    // not hold them, so the handshakes do not compare them.
    options.secrets_unknown = true;
    // A witness applies every entry kind a player-serving peer applies.
    options.components = super::attach::cluster_components(config.as_ref())?;
    options.database = Some(database.clone());
    let cluster = Cluster::start(settings, options).await?;
    database.set_executor(Arc::new(ClusterExecutor::new(&cluster)));
    super::install_global(cluster.clone());
    // A witness holds template blobs, so it counts toward the voters an
    // upload must reach before its manifest commits.
    super::files::attach_witness(&cluster).await?;
    log_info!("cluster: witness {name} running; SIGINT or SIGTERM (Ctrl-C) stops it");
    stop.wait().await;
    log_info!("cluster: witness {name} stopping");
    cluster.shutdown().await;
    Ok(())
}

/// The signals that stop a witness: those that stop the proxy.
#[cfg(unix)]
struct Stop {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Stop {
    fn new() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    async fn wait(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
    }
}

/// The signals that stop a witness: those that stop the proxy.
#[cfg(windows)]
struct Stop {
    interrupt: tokio::signal::windows::CtrlC,
}

#[cfg(windows)]
impl Stop {
    fn new() -> io::Result<Self> {
        Ok(Self {
            interrupt: tokio::signal::windows::ctrl_c()?,
        })
    }

    async fn wait(&mut self) {
        self.interrupt.recv().await;
    }
}
