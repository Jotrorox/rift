//! `rift cluster init|recover` and `rift witness`.
use super::{
    Cluster, PeerNode,
    log_store::LogStore,
    node::Options,
    settings::{self, Settings},
    state::{self, Entry, FeaturesEntry, PeerEntry},
    transport::Offline,
};
use crate::config::{ClusterConfig, Config};
use openraft::ServerState;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub const USAGE: &str = "Usage: rift cluster init --peer <name> [--from-backup <file>] [--config <path>]\n\
    \x20   Create a cluster whose only voter is this peer (cluster.toml beside the configuration names it).\n\
    \x20   With --from-backup, its state starts as a `rift admin cluster backup` copy.\n\
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
    data_loss: bool,
}

fn flags(args: &[String]) -> io::Result<Flags> {
    let mut flags = Flags {
        peer: None,
        config: PathBuf::from("rift.lua"),
        backup: None,
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
    std::fs::create_dir_all(&settings.directory)?;
    let log = LogStore::open(&settings.raft_file())?;
    if !log.is_pristine()? || log.identity()?.raft_id.is_some() {
        return Err(super::invalid(format!(
            "{}: this peer already belongs to a cluster",
            settings.raft_file().display()
        )));
    }
    match &flags.backup {
        Some(backup) => state::prepare_from_backup(&settings.state_file(), backup)?,
        None if state::has_applied(&settings.state_file())? => {
            return Err(super::invalid(format!(
                "{}: holds the state of another cluster; move it away first",
                settings.state_file().display()
            )));
        }
        None => {}
    }
    let mut id = [0u8; 16];
    aws_lc_rs::rand::fill(&mut id).map_err(|_| io::Error::other("random source failed"))?;
    let cluster_id = super::hex(&id);
    log.adopt(&cluster_id, FIRST_RAFT_ID, &settings.peer)?;
    drop(log);
    let mut options = Options::new(ClusterConfig::default());
    options.transport = Some(Arc::new(Offline::default()));
    // Nothing is reachable, so no token is ever checked.
    options.token = Some("offline-initialization".to_owned());
    let cluster = Cluster::start(settings.clone(), options).await?;
    let result = initialize(&cluster).await;
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
    Ok(())
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

/// `rift witness`: a voter without listeners, until Ctrl-C.
pub async fn witness(args: &[String]) -> io::Result<()> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let flags = flags(args)?;
    let mut settings = load_settings(&flags.config, None)?;
    settings.witness = true;
    settings::set_local_peer(&settings.peer);
    // The shared settings come from rift.lua when this host has one.
    let shared = match Config::load(&flags.config) {
        Ok(config) => config.cluster.unwrap_or_default(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => ClusterConfig::default(),
        Err(error) => return Err(error),
    };
    let name = settings.peer.clone();
    let cluster = Cluster::start(settings, Options::new(shared)).await?;
    super::install_global(cluster.clone());
    log_info!("cluster: witness {name} running; Ctrl-C stops it");
    tokio::signal::ctrl_c().await?;
    cluster.shutdown().await;
    Ok(())
}
