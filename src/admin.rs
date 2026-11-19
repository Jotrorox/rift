//! Bounded, authenticated local control plane. One JSON request/response per TCP connection.
use crate::{admission::Admission, events, metrics::Metrics, runtime::Snapshot};
use rift::config::{Admin, Config, GroupState, NodeSchedule};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, watch},
    task::JoinSet,
    time::timeout,
};

pub type Reply = oneshot::Sender<Result<Value, String>>;
pub enum Command {
    Reload(Reply),
    CreateInstance {
        group: String,
        reply: Reply,
    },
    RemoveInstance {
        name: String,
        reply: Reply,
    },
    /// Commit a move prepared by [`crate::moving::run`].
    MoveInstance {
        commit: crate::moving::Commit,
        reply: Reply,
    },
    // The handler confirms that the response has been written before shutdown begins.
    /// `kept` also stops local servers with `keep_running` (`shutdown all`).
    Shutdown {
        ack: oneshot::Receiver<()>,
        kept: bool,
    },
}

pub struct Transfer {
    pub snapshot: Arc<Snapshot>,
    pub backend: String,
    pub reply: TransferReply,
}

/// Backend plugin transfers have no wire acknowledgement. They still share the
/// bounded per-player queue and the same completion and admission path as admin.
pub enum TransferReply {
    Admin(Reply),
    BungeeCord,
}

impl TransferReply {
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Admin(reply) if reply.is_closed())
    }

    pub fn reason(&self) -> &'static str {
        match self {
            Self::Admin(_) => "admin",
            Self::BungeeCord => "bungeecord",
        }
    }

    pub fn send(self, value: Result<Value, String>) -> Result<(), Result<Value, String>> {
        match self {
            Self::Admin(reply) => reply.send(value),
            Self::BungeeCord => Ok(()),
        }
    }
}

struct Entry {
    name: String,
    backend: String,
    protocol: i32,
    peer: SocketAddr,
    transfer: mpsc::Sender<Transfer>,
}

#[derive(Default)]
pub struct Control {
    maintenance: Mutex<Option<bool>>,
    draining: Mutex<BTreeMap<String, bool>>,
    /// Operator state of service groups; groups absent here are active.
    groups: Mutex<BTreeMap<String, GroupState>>,
    /// Cordoned and draining nodes; nodes absent here are active.
    nodes: Mutex<BTreeMap<String, NodeSchedule>>,
    pub logins: Mutex<Admission>,
    players: Mutex<BTreeMap<u64, Entry>>,
    /// What backends report about themselves, including their own drain.
    states: rift::service_state::ServiceStates,
}

impl Control {
    /// Share `states` with the managed servers, which forget a report when
    /// its server starts again.
    pub fn with_states(states: rift::service_state::ServiceStates) -> Self {
        Self {
            states,
            ..Self::default()
        }
    }

    pub fn states(&self) -> &rift::service_state::ServiceStates {
        &self.states
    }

    pub fn maintenance(&self, config: &Config) -> bool {
        self.maintenance
            .lock()
            .unwrap()
            .unwrap_or(config.maintenance)
    }

    /// A backend drains when an operator or the configuration says so, when
    /// it asked to on `rift.server.state`, while its service group is
    /// stopped, or while it is a disposable instance on a draining node;
    /// `drain off` cannot override the last two.
    pub fn draining(&self, config: &Config, backend: &str) -> bool {
        self.draining
            .lock()
            .unwrap()
            .get(backend)
            .copied()
            .unwrap_or_else(|| config.draining.contains(backend))
            || self.states.draining(backend)
            || config
                .instances
                .get(backend)
                .is_some_and(|instance| self.group(&instance.group).suspended)
            || self.evacuating(config, backend)
    }

    pub fn node(&self, name: &str) -> NodeSchedule {
        self.nodes
            .lock()
            .unwrap()
            .get(name)
            .copied()
            .unwrap_or_default()
    }

    /// Whether `instance` is a disposable instance on a draining node: it
    /// takes no new players and is retired once empty.
    pub fn evacuating(&self, config: &Config, instance: &str) -> bool {
        config.instances.get(instance).is_some_and(|instance| {
            instance.storage.discards_data()
                && instance
                    .node
                    .as_deref()
                    .is_some_and(|node| self.node(node) == NodeSchedule::Draining)
        })
    }

    /// Replace the whole table, as restored from the instance database.
    pub fn restore_nodes(&self, nodes: BTreeMap<String, NodeSchedule>) {
        *self.nodes.lock().unwrap() = nodes;
    }

    /// Change one node's schedule under the table lock; `persist` runs
    /// first and can veto it, as in [`Control::update_group`].
    pub fn update_node(
        &self,
        name: &str,
        schedule: NodeSchedule,
        persist: impl FnOnce(NodeSchedule) -> io::Result<()>,
    ) -> io::Result<NodeSchedule> {
        let mut nodes = self.nodes.lock().unwrap();
        persist(schedule)?;
        if schedule == NodeSchedule::Active {
            nodes.remove(name);
        } else {
            nodes.insert(name.to_owned(), schedule);
        }
        Ok(schedule)
    }

    /// Forget nodes that are no longer configured; returns their names.
    pub fn retain_nodes(&self, keep: impl Fn(&str) -> bool) -> Vec<String> {
        let mut nodes = self.nodes.lock().unwrap();
        let removed: Vec<_> = nodes.keys().filter(|name| !keep(name)).cloned().collect();
        for name in &removed {
            nodes.remove(name);
        }
        removed
    }

    pub fn group(&self, name: &str) -> GroupState {
        self.groups
            .lock()
            .unwrap()
            .get(name)
            .copied()
            .unwrap_or_default()
    }

    /// Replace the whole table, as restored from the instance database.
    pub fn restore_groups(&self, groups: BTreeMap<String, GroupState>) {
        *self.groups.lock().unwrap() = groups;
    }

    /// Change one group's state under the table lock. `persist` runs before
    /// the change becomes visible and can veto it, so the durable record
    /// never lags behind what routing and the scaler see.
    pub fn update_group(
        &self,
        name: &str,
        change: impl FnOnce(&mut GroupState),
        persist: impl FnOnce(GroupState) -> io::Result<()>,
    ) -> io::Result<GroupState> {
        let mut groups = self.groups.lock().unwrap();
        let mut state = groups.get(name).copied().unwrap_or_default();
        change(&mut state);
        persist(state)?;
        if state.is_default() {
            groups.remove(name);
        } else {
            groups.insert(name.to_owned(), state);
        }
        Ok(state)
    }

    /// Forget groups whose definitions are gone; returns their names.
    pub fn retain_groups(&self, keep: impl Fn(&str) -> bool) -> Vec<String> {
        let mut groups = self.groups.lock().unwrap();
        let removed: Vec<_> = groups.keys().filter(|name| !keep(name)).cloned().collect();
        for name in &removed {
            groups.remove(name);
        }
        removed
    }

    pub fn register(
        self: &Arc<Self>,
        id: u64,
        backend: String,
        protocol: i32,
        name: String,
        peer: SocketAddr,
    ) -> (Registration, mpsc::Receiver<Transfer>) {
        let (transfer, receiver) = mpsc::channel(1);
        self.players.lock().unwrap().insert(
            id,
            Entry {
                name,
                backend,
                protocol,
                peer,
                transfer,
            },
        );
        (
            Registration {
                control: self.clone(),
                id,
            },
            receiver,
        )
    }

    pub fn moved(&self, id: u64, backend: &str) {
        if let Some(entry) = self.players.lock().unwrap().get_mut(&id) {
            entry.backend = backend.to_owned();
        }
    }

    pub fn player_peer(&self, name: &str) -> Option<SocketAddr> {
        self.players
            .lock()
            .unwrap()
            .values()
            .find(|entry| entry.name.eq_ignore_ascii_case(name))
            .map(|entry| entry.peer)
    }

    /// Queue a BungeeCord transfer for this process's player `name`; false
    /// when no such player is connected here.
    pub fn plugin_transfer(&self, name: &str, backend: &str, snapshot: Arc<Snapshot>) -> bool {
        let players = self.players.lock().unwrap();
        let Some(entry) = players
            .values()
            .find(|entry| entry.name.eq_ignore_ascii_case(name))
        else {
            return false;
        };
        if entry.backend != backend {
            // No task or unbounded queue per request, including self-transfers.
            // The recipient rechecks access, health, draining and extensions.
            let _ = entry.transfer.try_send(Transfer {
                snapshot,
                backend: backend.to_owned(),
                reply: TransferReply::BungeeCord,
            });
        }
        true
    }

    /// [`Control::plugin_transfer`] by connection id, for a BungeeCord
    /// `ConnectOther` that another cluster peer routed here.
    fn plugin_transfer_id(&self, id: u64, backend: &str, snapshot: Arc<Snapshot>) -> bool {
        let players = self.players.lock().unwrap();
        let Some(entry) = players.get(&id) else {
            return false;
        };
        if entry.backend != backend {
            let _ = entry.transfer.try_send(Transfer {
                snapshot,
                backend: backend.to_owned(),
                reply: TransferReply::BungeeCord,
            });
        }
        true
    }

    fn status(&self, snapshot: &Snapshot, metrics: &Metrics) -> Value {
        // Every peer's players in a cluster, each with its peer; the output
        // of a single Rift is unchanged.
        let identity = snapshot.players.identity();
        let own = identity
            .is_clustered()
            .then(|| rift::players::identity::peer_label(identity.peer));
        let mut players: Vec<Value> = self.players.lock().unwrap().iter().map(|(id, entry)| {
            let mut player = json!({"connection_id":id,"name":entry.name,"backend":entry.backend,"protocol":entry.protocol});
            if let Some(peer) = &own {
                player["peer"] = json!(peer);
            }
            player
        }).collect();
        players.extend(snapshot.players.remote_players().into_iter().map(|remote| {
            let player = remote.player;
            json!({"connection_id":player.connection,"name":player.name,"backend":player.server,"protocol":player.protocol,"peer":rift::players::identity::peer_label(player.peer)})
        }));
        let backends: Vec<Value> = snapshot.health.metrics().into_iter().map(|(name, up)| {
            let report = self.states.get(&name);
            json!({"draining":self.draining(&snapshot.config, &name),"service_state":report.state,"properties":report.properties,"backend":name,"up":up})
        }).collect();
        json!({"maintenance":self.maintenance(&snapshot.config),"connections":metrics.active.get(),"players_online":metrics.players.get(),"players":players,"backends":backends,"managed_servers":managed_servers(snapshot)})
    }
}

/// `rift admin transfer` of a session of this process.
pub(crate) async fn transfer_local(
    snapshot: Arc<Snapshot>,
    id: u64,
    backend: &str,
) -> Result<Value, String> {
    let control = &snapshot.control;
    if !snapshot.config.backends.contains_key(backend) {
        return Err(format!("unknown backend {backend:?}"));
    }
    if control.draining(&snapshot.config, backend) || !snapshot.reachable(backend) {
        return Err("target backend is draining, stopped or unhealthy".into());
    }
    let sender = {
        let players = control.players.lock().unwrap();
        let entry = players
            .get(&id)
            .ok_or("player is no longer online; refresh status")?;
        if !rift::protocol::ProtocolVersion::new(entry.protocol)
            .is_ok_and(rift::protocol::ProtocolVersion::supports_switching)
        {
            return Err("player transfers require Minecraft 1.8.9–26.3".into());
        }
        if entry.backend == backend {
            return Err("player is already on that backend".into());
        }
        entry.transfer.clone()
    };
    let (reply, receiver) = oneshot::channel();
    sender
        .try_send(Transfer {
            snapshot: snapshot.clone(),
            backend: backend.to_owned(),
            reply: TransferReply::Admin(reply),
        })
        .map_err(|_| "player disconnected or already has a queued transfer")?;
    receiver
        .await
        .map_err(|_| "player disconnected during transfer".to_owned())?
}

/// What other cluster peers need from this process's sessions: transfers
/// through a session's control queue (`rift admin transfer`, BungeeCord
/// `ConnectOther`) and clicks on dialogs this process showed. It reads the
/// newest configuration generation for each request.
pub struct RemoteSessions {
    live: Arc<Mutex<std::sync::Weak<Snapshot>>>,
}

impl RemoteSessions {
    pub fn new(live: Arc<Mutex<std::sync::Weak<Snapshot>>>) -> Self {
        Self { live }
    }

    fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.live.lock().unwrap().upgrade()
    }
}

impl rift::extensions::routing::SessionHooks for RemoteSessions {
    fn transfer(
        &self,
        connection: u64,
        backend: String,
        admin: bool,
    ) -> rift::extensions::routing::RouteFuture<Result<Value, String>> {
        let snapshot = self.snapshot();
        Box::pin(async move {
            let Some(snapshot) = snapshot else {
                return Err("this peer is not serving players".to_owned());
            };
            if admin {
                transfer_local(snapshot, connection, &backend).await
            } else {
                let control = snapshot.control.clone();
                Ok(Value::Bool(
                    control.plugin_transfer_id(connection, &backend, snapshot),
                ))
            }
        })
    }

    fn dialog(
        &self,
        mut context: rift::extensions::Context,
        owner: rift::dialog::Owner,
    ) -> rift::extensions::routing::RouteFuture<io::Result<()>> {
        let snapshot = self.snapshot();
        Box::pin(async move {
            let Some(snapshot) = snapshot else {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "this peer is not serving players",
                ));
            };
            let Some(response) = context.dialog.take() else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "dialog click without a response",
                ));
            };
            let jobs = Arc::new(tokio::sync::Semaphore::new(1));
            snapshot
                .extensions
                .dialog(context, owner, response, &jobs)
                .await
        })
    }
}

pub struct Registration {
    control: Arc<Control>,
    id: u64,
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.control.players.lock().unwrap().remove(&self.id);
    }
}

pub fn token(settings: &Admin) -> io::Result<String> {
    let token = std::env::var(&settings.token_env).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("admin.token_env: set {} to a random secret of at least 32 bytes before starting Rift", settings.token_env)))?;
    if token.len() < 32 || token.len() > 1024 || token.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "admin.token_env: secret must contain 32..=1024 bytes without control characters",
        ));
    }
    Ok(token)
}

fn authenticated(expected: &str, supplied: &str) -> bool {
    let mut difference = expected.len() ^ supplied.len();
    for (index, byte) in expected.bytes().enumerate() {
        difference |= usize::from(byte ^ supplied.as_bytes().get(index).copied().unwrap_or(0));
    }
    difference == 0
}

async fn read_line(stream: &mut TcpStream, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let byte = stream.read_u8().await?;
        if byte == b'\n' {
            return Ok(bytes);
        }
        if bytes.len() >= limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "admin request/response exceeds size limit",
            ));
        }
        bytes.push(byte);
    }
}

/// Whether the request carries the administrator token.
fn holds_token(value: &Value, secret: &str) -> bool {
    value
        .get("token")
        .and_then(Value::as_str)
        .is_some_and(|s| authenticated(secret, s))
}

fn request(value: &Value, settings: &Admin, secret: &str) -> Result<Vec<String>, String> {
    if !holds_token(value, secret) {
        return Err("authentication failed; supply RIFT_ADMIN_TOKEN".into());
    }
    let args = value
        .get("args")
        .and_then(Value::as_array)
        .ok_or("expected args array")?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or("command arguments must be strings".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let command = args.first().ok_or("missing command")?;
    let permission = match (command.as_str(), args.get(1).map(String::as_str)) {
        (
            "start" | "stop" | "groups" | "group" | "create" | "remove" | "move" | "templates"
            | "download" | "sync" | "warm" | "nodes" | "node" | "backup" | "backups" | "restore",
            _,
        ) => "servers",
        // Restarting a plugin changes the running proxy; its log tail may
        // carry whatever the plugin logs, as server logs do over HTTP;
        // reading its state is status.
        // So does running one of its commands as the console.
        ("plugins", Some("restart" | "run")) => "plugins",
        ("plugins", Some("logs")) => "logs",
        ("plugins", _) => "status",
        // Reading the cluster's state is status; changing it is `cluster`.
        ("cluster", None | Some("status")) => "status",
        (command, _) => command,
    };
    if !settings.permissions.contains(permission) {
        return Err(format!(
            "permission denied: {permission}; grant it in admin.permissions and restart"
        ));
    }
    Ok(args)
}

/// Operational state only: never include process arguments, environment or paths.
pub(crate) fn managed_servers(snapshot: &Snapshot) -> Value {
    let config = &snapshot.config;
    // Only a cluster adds `peer`; a single Rift's listing stays as it was.
    let clustered = rift::cluster::global().is_some();
    let servers: Vec<_> = snapshot
        .managed
        .snapshot()
        .into_iter()
        .map(|server| {
            let instance = config.instances.get(&server.name);
            let group = instance.map(|instance| snapshot.control.group(&instance.group));
            let report = snapshot.control.states().get(&server.name);
            let address = config
                .backends
                .get(&server.name)
                .map(|backend| backend.address());
            let mut value = json!({
                "name": server.name,
                "state": server.state.as_str(),
                "pid": server.pid,
                "players": server.players,
                "reservations": server.reservations,
                "automatic_start": server.automatic_start,
                "automatic_enabled": server.automatic_enabled,
                "last_error": server.last_error,
                "restart_attempts": server.restart_attempts,
                "restart_exhausted": server.restart_exhausted,
                "group": instance.map(|instance| &instance.group),
                "group_suspended": group.map(|group| group.suspended),
                "group_mode": group.map(|group| group.mode.as_str()),
                "draining": snapshot.control.draining(config, &server.name),
                "service_state": report.state,
                "properties": report.properties,
                "template": instance.and_then(|instance| instance.template.as_ref()),
                "storage": instance.map_or("persistent", |instance| instance.storage.as_str()),
                "node": server.node.as_deref().unwrap_or(rift::config::LOCAL_NODE),
                "moving": server.moving,
                // Phase and progress while `rift admin move` runs.
                "move": crate::moving::describe(&server.name),
                // Set while `rift admin move` overrides the configuration.
                "configured_node": config
                    .moved_servers
                    .get(&server.name)
                    .map(|moved| &moved.configured_node),
                "address": address,
                "port": address
                    .and_then(|address| address.parse::<SocketAddr>().ok())
                    .map(|address| address.port()),
            });
            if clustered {
                value["peer"] = json!(server_peer(config, server.node.as_deref()));
            }
            value
        })
        .collect();
    json!(servers)
}

/// The peer a server runs on in a cluster: a peer's own node is that peer.
/// Servers on a `rift node` agent belong to no peer.
fn server_peer<'a>(config: &Config, node: Option<&'a str>) -> Option<&'a str> {
    node.filter(|node| {
        config
            .nodes
            .get(*node)
            .is_some_and(|definition| definition.peer)
    })
}

/// Count each peer's connected players into a cluster status: `players` on
/// every entry of `peers`, from the player registry, which holds every
/// peer's players. Values without `peers` stay as they are.
pub(crate) fn peer_players(snapshot: &Snapshot, status: &mut Value) {
    let Some(peers) = status.get_mut("peers").and_then(Value::as_array_mut) else {
        return;
    };
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for player in snapshot.players.snapshot() {
        *counts
            .entry(rift::players::identity::peer_label(player.peer))
            .or_default() += 1;
    }
    for peer in peers {
        let count = peer["name"]
            .as_str()
            .and_then(|name| counts.get(name))
            .copied()
            .unwrap_or(0);
        peer["players"] = json!(count);
    }
}

/// Node agents with reachability, capacity and allocation; never tokens.
pub(crate) fn nodes(snapshot: &Snapshot) -> Value {
    let mut nodes = snapshot.nodes.describe(&snapshot.config);
    for node in nodes.as_array_mut().into_iter().flatten() {
        let Some(name) = node["name"].as_str().map(str::to_owned) else {
            continue;
        };
        node["schedule"] = json!(snapshot.control.node(&name).as_str());
        node["drained"] = json!(crate::cordon::drained(snapshot, &name));
    }
    json!({"nodes": nodes})
}

/// Group metadata only: templates contain executable arguments and filesystem paths.
pub(crate) fn service_groups(snapshot: &Snapshot) -> Value {
    let servers = snapshot.managed.snapshot();
    json!({"groups":snapshot.config.service_groups.iter().map(|(name, group)| {
        let state = snapshot.control.group(name);
        let members: Vec<_> = servers
            .iter()
            .filter(|server| {
                snapshot
                    .config
                    .instances
                    .get(&server.name)
                    .is_some_and(|instance| instance.group == *name)
            })
            .collect();
        let phase = |phase: rift::managed::Phase| members.iter().filter(|server| server.state == phase).count();
        // The client versions translated for the group's instances: its own
        // setting, or the proxy-wide one it inherits.
        let translation = &snapshot.config.translation;
        let clients = group.translation.as_ref().unwrap_or(&translation.clients);
        json!({
            "translation":{
                "enabled":translation.enabled,
                "inherited":group.translation.is_none(),
                "clients":clients.to_json(),
                "summary":if translation.enabled { clients.summary() } else { "Off for the whole proxy".to_owned() },
            },
            "suspended":state.suspended,
            "mode":state.mode.as_str(),
            "dev_permission":rift::config::GroupMode::dev_permission(name),
            "summary":{
                "running":phase(rift::managed::Phase::Running),
                "starting":phase(rift::managed::Phase::Starting),
                "stopping":phase(rift::managed::Phase::Stopping),
                "stopped":phase(rift::managed::Phase::Stopped),
                "failed":phase(rift::managed::Phase::Failed),
                "unknown":phase(rift::managed::Phase::Unknown),
                "players":members.iter().map(|server| server.players).sum::<usize>(),
                // Occupied members of a stopped group still waiting to stop.
                "draining":members.iter().filter(|server| {
                    state.suspended && server.automatic_enabled && (server.players > 0 || server.reservations > 0)
                }).count(),
            },
            "name":name,
            "template":group.template,
            "storage":group.storage.as_str(),
            "port_range":[group.port_start,group.port_end],
            "scaling":group.scaling.as_ref().map(scaling_values),
            "nodes":group.nodes.as_ref().map(|nodes| json!({"names":nodes.names,"labels":nodes.labels})),
            "placement":group.nodes.as_ref().map(|_| group.placement.as_str()),
            "balance":group.balance.as_str(),
            "instances":snapshot.config.instances.iter()
                .filter(|(_, instance)| instance.group == *name)
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
        })
    }).collect::<Vec<_>>()})
}

/// Creation may first download template software; reads have their own timeouts.
pub(crate) const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(15 * 60);

/// Template metadata and software cache state; asset paths stay private.
pub(crate) fn templates(snapshot: &Snapshot) -> Value {
    let config = &snapshot.config;
    json!({"templates":config.templates.iter().map(|(name, template)| {
        json!({
            "name": name,
            "software": template.software.as_ref()
                .map(|software| rift::software::status(&config.software_cache, software)),
            "config_layers": template.configs.len(),
            "plugins": template.plugins.len(),
            "map": template.map.is_some(),
            "accept_eula": template.accept_eula,
            "groups": config.service_groups.iter()
                .filter(|(_, group)| group.template.as_deref() == Some(name))
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
        })
    }).collect::<Vec<_>>(),
    "sources": config.template_sources.iter().map(|(name, source)| {
        rift::template_source::status(&config.template_storage, &config.software_cache, name, source)
    }).collect::<Vec<_>>()})
}

/// Fetch a git template source and replace its checkout, so the next
/// instances are built from the repository's current files.
pub(crate) async fn sync_template_source(snapshot: &Snapshot, name: &str) -> Result<Value, String> {
    let config = &snapshot.config;
    let source = config
        .template_sources
        .get(name)
        .ok_or_else(|| format!("unknown template source {name:?}"))?;
    rift::template_source::sync(
        &config.template_storage,
        &config.software_cache,
        name,
        source,
    )
    .await
    .map(|status| json!({"source": status}))
    .map_err(|error| format!("syncing template source {name} failed: {error}"))
}

/// Download a template's software into the cache, outside the instance
/// queue, so creation later starts without waiting for the network.
pub(crate) async fn download_template(
    snapshot: &Snapshot,
    name: &str,
    refresh: bool,
) -> Result<Value, String> {
    let template = snapshot
        .config
        .templates
        .get(name)
        .ok_or_else(|| format!("unknown template {name:?}"))?;
    let software = template
        .software
        .as_ref()
        .ok_or_else(|| format!("template {name:?} uses a local server_jar; nothing to download"))?;
    let status = rift::software::download(
        &snapshot.config.software_cache,
        software,
        refresh,
        &rift::software::Endpoints::default(),
    )
    .await
    .map_err(|error| format!("downloading {} failed: {error}", software.key()))?;
    Ok(json!({"template": name, "software": status}))
}

/// Templates each node needs: those of the service groups whose `nodes`
/// selector matches it, optionally only `only`. Nodes without any are left out.
pub(crate) fn node_templates(
    config: &Config,
    only: Option<&str>,
) -> BTreeMap<String, std::collections::BTreeSet<String>> {
    let mut plan: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    for (node, definition) in &config.nodes {
        for group in config.service_groups.values() {
            if let (Some(selector), Some(template)) = (&group.nodes, &group.template)
                && selector.matches(node, definition)
                && only.is_none_or(|only| only == template)
            {
                plan.entry(node.clone())
                    .or_default()
                    .insert(template.clone());
            }
        }
    }
    plan
}

/// Have every node cache the templates (files and software) its groups use,
/// so the first instance on a new or freshly synced node starts without
/// waiting for downloads. Nodes work in parallel; any failure, an
/// unreachable node included, fails the whole operation after the rest
/// finished, with every outcome in the message.
pub(crate) async fn warm_templates(
    snapshot: &Snapshot,
    only: Option<&str>,
) -> Result<Value, String> {
    let config = &snapshot.config;
    if let Some(name) = only
        && !config.templates.contains_key(name)
    {
        return Err(format!("unknown template {name:?}"));
    }
    let plan = node_templates(config, only);
    if plan.is_empty() {
        return Err(match only {
            Some(name) => format!("no service group on nodes uses template {name:?}"),
            None => "no service group with a template runs on nodes".into(),
        });
    }
    // Check every template once, before anything is sent: an asset outside
    // template_storage is a configuration problem, not a node's.
    let mut assets = BTreeMap::new();
    for name in plan.values().flatten() {
        if !assets.contains_key(name) {
            let checked = rift::provisioning::remote_template_assets(config, name)
                .map_err(|error| error.to_string());
            assets.insert(name.clone(), checked);
        }
    }
    let mut work = JoinSet::new();
    for (node, templates) in plan {
        let mut results = Vec::new();
        let mut command = Vec::new();
        for template in templates {
            match &assets[&template] {
                Ok(assets) => command.push(assets.clone()),
                Err(error) => {
                    results.push(json!({"template": template, "ok": false, "error": error}))
                }
            }
        }
        // An agent without the feature would refuse the command; say why
        // instead. A node that never connected fails as unreachable below.
        let warm_feature = [rift::node::protocol::Requirement {
            feature: rift::node::protocol::feature::WARM,
            mandatory: true,
        }];
        if !command.is_empty() && !snapshot.nodes.lacking(&node, &warm_feature).is_empty() {
            let error = format!(
                "the agent on {node} cannot warm templates; upgrade rift on that node to {}",
                env!("CARGO_PKG_VERSION")
            );
            work.spawn(async move {
                json!({"node": node, "ok": false, "error": error, "templates": results})
            });
            continue;
        }
        let nodes = snapshot.nodes.clone();
        work.spawn(async move {
            if !command.is_empty() {
                let warm = rift::node::protocol::Command::Warm(rift::node::protocol::Warm {
                    templates: command,
                });
                match nodes.request(&node, &warm, DOWNLOAD_DEADLINE).await {
                    Ok(reply) => results.extend(reply["templates"].as_array().cloned().unwrap_or_default()),
                    Err(error) => {
                        return json!({"node": node, "ok": false, "error": error.to_string(), "templates": results});
                    }
                }
            }
            let ok = results.iter().all(|result| result["ok"] == true);
            json!({"node": node, "ok": ok, "templates": results})
        });
    }
    let mut nodes = work.join_all().await;
    nodes.sort_by(|a, b| a["node"].as_str().cmp(&b["node"].as_str()));
    let mut failures = Vec::new();
    let mut warmed = 0;
    for node in &nodes {
        let name = node["node"].as_str().unwrap_or_default();
        if let Some(error) = node["error"].as_str() {
            failures.push(format!("{name}: {error}"));
        }
        for template in node["templates"].as_array().into_iter().flatten() {
            if template["ok"] == true {
                warmed += 1;
            } else {
                failures.push(format!(
                    "{name}/{}: {}",
                    template["template"].as_str().unwrap_or_default(),
                    template["error"].as_str().unwrap_or("failed")
                ));
            }
        }
    }
    if !failures.is_empty() {
        return Err(format!(
            "warming templates failed ({warmed} cached): {}",
            failures.join("; ")
        ));
    }
    Ok(json!({"nodes": nodes, "warmed": warmed}))
}

/// The same field names as `service_groups.*.scaling` in Lua.
pub(crate) fn scaling_values(policy: &rift::config::ServiceScaling) -> Value {
    json!({
        "min_instances": policy.min_instances,
        "max_instances": policy.max_instances,
        "spare_instances": policy.spare_instances,
        "capacity_per_instance": policy.capacity_per_instance,
        "target_occupancy_percent": policy.target_occupancy_percent,
        "queue_threshold": policy.queue_threshold,
        "cooldown_ms": policy.cooldown.as_millis(),
    })
}

pub(crate) fn managed_backend(snapshot: &Snapshot, backend: &str) -> io::Result<()> {
    if !snapshot.config.backends.contains_key(backend) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("unknown backend {backend:?}"),
        ));
    }
    if !snapshot.managed.is_managed(backend) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("backend {backend:?} is not managed; configure managed_servers and restart"),
        ));
    }
    Ok(())
}

const PLUGINS_USAGE: &str = "usage: plugins list | plugins status <name> | plugins logs <name> | plugins restart <name> | plugins run <command> [arguments]";

/// `plugins list|status|logs|restart` (`plugins run` is handled by
/// [`execute`], for the administration service only), shared by the local administration
/// service and `rift.control.plugins`. `actor` is recorded with a restart.
pub(crate) fn plugins(snapshot: &Snapshot, args: &[String], actor: &str) -> Result<Value, String> {
    let host = snapshot.extensions.plugins();
    let unknown = |name: &str| format!("no plugin named {name:?}; use plugins list");
    match args {
        [action] if action == "list" => Ok(json!({"plugins": host.statuses()})),
        [action, name] if action == "status" => host
            .statuses()
            .into_iter()
            .find(|status| status.name == *name)
            .map(|status| json!(status))
            .ok_or_else(|| unknown(name)),
        [action, name] if action == "logs" => host
            .monitor()
            .logs(name)
            .map(|lines| json!({"plugin": name, "lines": lines}))
            .ok_or_else(|| unknown(name)),
        [action, name] if action == "restart" => {
            let stopping = host
                .restart(name, actor)
                .map_err(|error| error.to_string())?;
            tokio::spawn(stopping);
            Ok(
                json!({"plugin":name,"operation":"restart","accepted":true,"message":"Restart accepted; use plugins status to check completion"}),
            )
        }
        _ => Err(PLUGINS_USAGE.into()),
    }
}

/// The audit action and target of a command: `plugins <action>` and the
/// plugin name, or the command alone.
pub(crate) fn audited(args: &[String]) -> (String, String) {
    match args {
        // A plan changes nothing and names no node.
        [command, action, ..] if command == "node" && action == "plan" => {
            ("node plan".into(), String::new())
        }
        [command, action, rest @ ..]
            if command == "plugins" || command == "group" || command == "node" =>
        {
            (
                format!("{command} {action}").chars().take(64).collect(),
                rest.first().cloned().unwrap_or_default(),
            )
        }
        [command, action, instance, ..] if command == "backups" && action == "forget" => {
            (format!("{command} {action}"), instance.clone())
        }
        [command, instance, ..] if matches!(command.as_str(), "move" | "backup" | "restore") => {
            (command.clone(), instance.clone())
        }
        [command, ..] => (command.clone(), String::new()),
        [] => (String::new(), String::new()),
    }
}

/// A command that runs on the cluster leader only, as a follower hands it
/// over (`cluster_runtime::forward`).
enum LeaderOnly {
    Move(crate::moving::Request),
    /// The request kind, its body and how long to wait for the leader.
    Other(&'static str, Value, Duration),
}

/// The leader-only command `args` names, if it is one: moves, backups and
/// restores, and cache warming.
fn leader_only(args: &[String]) -> Result<Option<LeaderOnly>, String> {
    use crate::cluster_runtime::{BACKUP, FORGET_BACKUPS, MOVE_CANCEL, RESTORE, SHORT_WAIT, WARM};
    Ok(Some(match args {
        [command, rest @ ..] if command == "move" => match crate::moving::Command::parse(rest)? {
            crate::moving::Command::Move(request) => LeaderOnly::Move(request),
            crate::moving::Command::Cancel(server) => {
                LeaderOnly::Other(MOVE_CANCEL, json!({ "server": server }), SHORT_WAIT)
            }
        },
        [command, instance] if command == "backup" => LeaderOnly::Other(
            BACKUP,
            json!({ "instance": instance }),
            crate::backups::DEADLINE,
        ),
        [command, action, instance, which] if command == "backups" && action == "forget" => {
            LeaderOnly::Other(
                FORGET_BACKUPS,
                json!({ "instance": instance, "snapshot": which }),
                crate::backups::DEADLINE,
            )
        }
        [command, rest @ ..] if command == "restore" => {
            let request = crate::backups::Restore::parse(rest)?;
            LeaderOnly::Other(
                RESTORE,
                json!({
                    "instance": request.instance,
                    "snapshot": request.snapshot,
                    "node": request.node,
                }),
                crate::backups::DEADLINE,
            )
        }
        [command] if command == "warm" => {
            LeaderOnly::Other(WARM, json!({ "template": null }), DOWNLOAD_DEADLINE)
        }
        [command, template] if command == "warm" => {
            LeaderOnly::Other(WARM, json!({ "template": template }), DOWNLOAD_DEADLINE)
        }
        _ => return Ok(None),
    }))
}

pub(crate) async fn execute(
    args: &[String],
    snapshot: Arc<Snapshot>,
    metrics: &Metrics,
    commands: &mpsc::Sender<Command>,
    actor: &str,
) -> Result<Value, String> {
    // A cluster follower hands leader-only work to the leader, which runs
    // it for `actor`.
    if let Some(coordination) = crate::cluster_runtime::follower()
        && let Some(work) = leader_only(args)?
    {
        return match work {
            LeaderOnly::Move(request) => {
                crate::cluster_runtime::forward_move(coordination, actor.to_owned(), request).await
            }
            LeaderOnly::Other(kind, body, wait) => {
                crate::cluster_runtime::forward(&coordination, kind, actor, body, wait).await
            }
        };
    }
    let control = &snapshot.control;
    let enabled = |value: &str| match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err("expected on or off".to_owned()),
    };
    match args {
        [command] if command == "status" => Ok(control.status(&snapshot, metrics)),
        [command] if command == "servers" => Ok(json!({"servers":managed_servers(&snapshot)})),
        [command] if command == "groups" => Ok(service_groups(&snapshot)),
        [command] if command == "nodes" => Ok(nodes(&snapshot)),
        [command] if command == "templates" => Ok(templates(&snapshot)),
        // The console sender: a plugin command registered with `console`.
        [command, action, line @ ..] if command == "plugins" && action == "run" && !line.is_empty() => {
            let line = line.join(" ");
            let output = snapshot.extensions.plugins().console(&line).await?;
            let component = match &output {
                rift::text::Text::Component(component) => component.clone(),
                rift::text::Text::Plain(text) => Value::String(text.clone()),
            };
            Ok(json!({"command": line, "output": output.to_string(), "component": component}))
        }
        [command, rest @ ..] if command == "plugins" => plugins(&snapshot, rest, actor),
        [command, template] if command == "download" => download_template(&snapshot, template, false).await,
        [command, template, refresh] if command == "download" && refresh == "refresh" => download_template(&snapshot, template, true).await,
        [command, source] if command == "sync" => sync_template_source(&snapshot, source).await,
        [command] if command == "warm" => warm_templates(&snapshot, None).await,
        [command, template] if command == "warm" => warm_templates(&snapshot, Some(template)).await,
        [command, group] if command == "create" => {
            let (reply, receiver) = oneshot::channel();
            commands.try_send(Command::CreateInstance { group: group.clone(), reply })
                .map_err(|_| "administrator command queue is full or shutting down")?;
            receiver.await.map_err(|_| "proxy is shutting down".to_owned())?
        }
        [command, name] if command == "remove" => {
            let (reply, receiver) = oneshot::channel();
            commands.try_send(Command::RemoveInstance { name: name.clone(), reply })
                .map_err(|_| "administrator command queue is full or shutting down")?;
            receiver.await.map_err(|_| "proxy is shutting down".to_owned())?
        }
        [command, rest @ ..] if command == "move" => match crate::moving::Command::parse(rest)? {
            crate::moving::Command::Move(request) => {
                crate::moving::run(snapshot, crate::moving::Committer::Admin(commands), request).await
            }
            crate::moving::Command::Cancel(instance) => crate::moving::cancel(&instance),
        },
        [command, instance] if command == "backup" => crate::backups::backup_now(snapshot, instance).await,
        [command] if command == "backups" => crate::backups::list(snapshot, None).await,
        [command, action, instance, which] if command == "backups" && action == "forget" => {
            crate::backups::forget(snapshot, instance, which).await
        }
        [command, instance] if command == "backups" => crate::backups::list(snapshot, Some(instance)).await,
        [command, rest @ ..] if command == "restore" => {
            let request = crate::backups::Restore::parse(rest)?;
            crate::backups::restore(snapshot, commands, request).await
        }
        [command, backend] if command == "start" || command == "stop" => {
            managed_backend(&snapshot, backend).map_err(|error| error.to_string())?;
            if command == "start" {
                snapshot.managed.request_start(backend)
            } else {
                snapshot.managed.request_stop(backend)
            }.map_err(|error| error.to_string())?;
            Ok(json!({"backend":backend,"operation":command,"accepted":true,"message":"Request accepted; use servers to check completion"}))
        }
        [command, value] if command == "maintenance" => {
            let value = enabled(value)?;
            *control.maintenance.lock().unwrap() = Some(value);
            Ok(json!({"maintenance":value}))
        }
        [command, backend, value] if command == "drain" => {
            if !snapshot.config.backends.contains_key(backend) { return Err(format!("unknown backend {backend:?}; use status to list backends")); }
            let value = enabled(value)?;
            control.draining.lock().unwrap().insert(backend.clone(), value);
            // The operator decides from here: `drain off` also withdraws
            // the server's own drain, until it asks again.
            control.states.undrain(backend);
            Ok(json!({"backend":backend,"draining":value}))
        }
        [command, id, backend] if command == "transfer" => {
            let id: u64 = id.parse().map_err(|_| "connection ID must be a positive integer")?;
            // The connection id names the peer that holds the session.
            if snapshot.players.identity().owns(id) {
                transfer_local(snapshot.clone(), id, backend).await
            } else {
                snapshot.extensions.players().route_transfer(id, backend, true).await
            }
        }
        [command, args @ ..] if command == "perms" => {
            let sender = rift::permissions::command::Sender::Console(rift_permissions::Actor::Cli);
            let output = rift::permissions::command::execute(&snapshot.perms.env(), &sender, args).await;
            if output.ok {
                Ok(json!({"revision": output.revision, "text": output.text(), "data": output.data}))
            } else {
                Err(output.text())
            }
        }
        [command, action, group] if command == "group" && action == "start" => {
            crate::groups::start(&snapshot, group).map_err(|error| error.message)
        }
        [command, action, group] if command == "group" && action == "stop" => {
            crate::groups::stop(&snapshot, group).map_err(|error| error.message)
        }
        [command, action, group, mode] if command == "group" && action == "mode" => {
            crate::groups::set_mode(&snapshot, group, mode).map_err(|error| error.message)
        }
        [command, action, rest @ ..] if command == "node" && action == "plan" => {
            crate::rebalance::parse(rest).map(|spread| crate::rebalance::plan(&snapshot, spread))
        }
        [command, action, node] if command == "node" => {
            crate::cordon::apply(&snapshot, node, action).map_err(|error| error.message)
        }
        [command, rest @ ..] if command == "cluster" => match rift::cluster::global() {
            // A status answer counts each peer's players; others stay as
            // they are.
            Some(cluster) => cluster.admin(rest).await.map(|mut value| {
                peer_players(&snapshot, &mut value);
                value
            }),
            None => Err("this Rift is not a cluster peer: it has no cluster.toml".into()),
        },
        [command] if command == "reload" => {
            let (reply, receiver) = oneshot::channel();
            commands.try_send(Command::Reload(reply)).map_err(|_| "administrator command queue is full or shutting down")?;
            receiver.await.map_err(|_| "proxy is shutting down".to_owned())?
        }
        _ => Err("usage: status | servers | groups | nodes | templates | download <template> [refresh] | sync <template-source> | warm [template] | create <group> | remove <instance> | move <server> <node> [--no-copy] [--keep-source] | move <server> --cancel | backup <instance> | backups [instance] | backups forget <instance> <snapshot>|all | restore <instance> [snapshot] [--node <node>] | start <backend> | stop <backend> | group start|stop <group> | group mode <group> active|dev | node cordon|drain|uncordon <node> | node plan [--spread <percent>] | maintenance on/off | drain <backend> on/off | transfer <connection-id> <backend> | plugins list | plugins status <name> | plugins logs <name> | plugins restart <name> | plugins run <command> [arguments] | perms <command> | cluster [<command>] | reload | shutdown".into()),
    }
}

pub async fn serve(
    listener: TcpListener,
    settings: Admin,
    secret: String,
    current: watch::Receiver<Arc<Snapshot>>,
    metrics: Arc<Metrics>,
    commands: mpsc::Sender<Command>,
    workflows: Arc<crate::operator::Store>,
) {
    let settings = Arc::new(settings);
    let secret = Arc::new(secret);
    let mut handlers = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = handlers.join_next(), if !handlers.is_empty() => {},
            result = listener.accept() => {
                let (mut stream, _) = match result { Ok(result) => result, Err(_) => {tokio::time::sleep(Duration::from_millis(100)).await; continue;} };
                if handlers.len() >= 16 { continue; }
                let settings = settings.clone();
                let secret = secret.clone();
                let current = current.clone();
                let metrics = metrics.clone();
                let commands = commands.clone();
                let workflows = workflows.clone();
                handlers.spawn(async move {
                    let _ = async {
                        let bytes = timeout(Duration::from_secs(2), read_line(&mut stream, 8192)).await??;
                        let value = serde_json::from_slice::<Value>(&bytes).map_err(|_| "invalid JSON request".to_owned());
                        let trusted = value.as_ref().is_ok_and(|value| holds_token(value, &secret));
                        let parsed = value.and_then(|value| request(&value, &settings, &secret));
                        let mut permission = String::new();
                        let mut target = String::new();
                        let mut shutdown_ack = None;
                        let result = match parsed {
                            Ok(args) => {
                                (permission, target) = audited(&args);
                                let audit = workflows.audit_async("local-admin", &permission, &target, "requested", 0).await;
                                if audit.is_err() {
                                    Err("operator audit unavailable; command was not submitted".into())
                                } else if args == ["shutdown"] || args == ["shutdown", "all"] {
                                    let kept = args.len() == 2;
                                    let (ack, receiver) = oneshot::channel();
                                    match commands.try_send(Command::Shutdown { ack: receiver, kept }) {
                                        Ok(()) => {shutdown_ack = Some(ack); Ok(if kept { json!({"shutdown":"draining","kept_servers":"stopping"}) } else { json!({"shutdown":"draining"}) })}
                                        Err(_) => Err("proxy is already shutting down or busy".into()),
                                    }
                                } else {
                                    let snapshot = current.borrow().clone();
                                    let mut deadline = Duration::from_secs(40);
                                    if let [command, _, backend] = args.as_slice()
                                        && command == "transfer"
                                        && let Some(server) = snapshot.config.managed_servers.get(backend)
                                    {
                                        deadline += server.start_timeout;
                                    }
                                    if matches!(args.first().map(String::as_str), Some("create" | "download" | "sync" | "warm")) {
                                        deadline += DOWNLOAD_DEADLINE;
                                    }
                                    if args.first().is_some_and(|command| command == "move") {
                                        deadline += crate::moving::DEADLINE;
                                    }
                                    if matches!(args.first().map(String::as_str), Some("backup" | "backups" | "restore")) {
                                        deadline += crate::backups::DEADLINE;
                                    }
                                    if let [command, name] = args.as_slice()
                                        && command == "remove"
                                        && let Some(server) = snapshot.config.managed_servers.get(name)
                                    {
                                        deadline += server.stop_timeout;
                                    }
                                    timeout(deadline, execute(&args, snapshot, &metrics, &commands, "local-admin")).await
                                        .unwrap_or_else(|_| Err("administrator operation deadline exceeded; refresh status before retrying".into()))
                                }
                            }
                            Err(error) => Err(error),
                        };
                        let (outcome, status) = if result.is_ok() { ("accepted", 200) } else { ("rejected", 400) };
                        // Any local process can send requests without the
                        // token. Those are counted as denial summaries, apart
                        // from operator records, so they cannot flush them.
                        let audit = if trusted {
                            workflows.audit_async("local-admin", &permission, &target, outcome, status).await
                        } else {
                            workflows.deny_async(401).await
                        };
                        if let Err(error) = audit {
                            log_error!("local administrator audit failed: {error}");
                        }
                        let response = match result {
                            Ok(data) => { events::admin("admin_command", "ok", &permission, &"command completed"); json!({"ok":true,"data":data}) }
                            Err(error) => { events::admin("admin_command", "rejected", &permission, &error); json!({"ok":false,"error":error}) }
                        };
                        timeout(Duration::from_secs(2), stream.write_all(format!("{response}\n").as_bytes())).await??;
                        timeout(Duration::from_secs(2), stream.shutdown()).await??;
                        if let Some(ack) = shutdown_ack { let _ = ack.send(()); }
                        Ok::<_,io::Error>(())
                    }.await;
                });
            }
        }
    }
}

pub async fn client(args: &[String]) -> io::Result<()> {
    let (address, args) = match args {
        [flag, address, args @ ..] if flag == "--address" => (address.as_str(), args),
        _ => ("127.0.0.1:9091", args),
    };
    if args.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: rift admin [--address 127.0.0.1:9091] status|servers|groups|nodes|templates|download <template> [refresh]|sync <template-source>|warm [template]|create <group>|remove <instance>|move <server> <node> [--no-copy] [--keep-source]|move <server> --cancel|backup <instance>|backups [instance]|backups forget <instance> <snapshot>|all|restore <instance> [snapshot] [--node <node>]|start <backend>|stop <backend>|node cordon|drain|uncordon <node>|node plan [--spread <percent>]|maintenance on/off|drain <backend> on/off|transfer <connection-id> <backend>|plugins list|plugins status <name>|plugins logs <name>|plugins restart <name>|plugins run <command> [arguments]|perms <command>|cluster [<command>]|reload|shutdown",
        ));
    }
    let address: SocketAddr = address.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "admin address requires a loopback IP and port",
        )
    })?;
    if !address.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "admin address must be loopback; run this command on the proxy host",
        ));
    }
    let secret = std::env::var("RIFT_ADMIN_TOKEN").map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "set RIFT_ADMIN_TOKEN to the server's admin secret",
        )
    })?;
    // Import and export paths are read on the proxy host; make relative
    // paths relative to where this command runs.
    let mut args = args.to_vec();
    if let [command, verb, file, ..] = args.as_mut_slice()
        && command == "perms"
        && (verb == "import" || verb == "export")
        && let Ok(absolute) = std::path::absolute(&*file)
    {
        *file = absolute.display().to_string();
    }
    // So is the file of `cluster backup`, written on the proxy host.
    if let [command, verb, file] = args.as_mut_slice()
        && command == "cluster"
        && verb == "backup"
        && let Ok(absolute) = std::path::absolute(&*file)
    {
        *file = absolute.display().to_string();
    }
    let perms = args.first().is_some_and(|command| command == "perms");
    let deadline = if args.first().is_some_and(|command| {
        matches!(
            command.as_str(),
            "transfer"
                | "remove"
                | "create"
                | "move"
                | "download"
                | "sync"
                | "warm"
                | "backup"
                | "backups"
                | "restore"
                | "cluster"
        )
    }) {
        Duration::from_secs(24 * 60 * 60 + 45)
    } else {
        Duration::from_secs(40)
    };
    let data = query(address, &secret, &args, deadline).await?;
    match data["text"].as_str() {
        Some(text) if perms => println!("{text}"),
        _ => println!("{}", serde_json::to_string_pretty(&data)?),
    }
    Ok(())
}

/// Run one admin command against a running Rift and return its `data`.
pub async fn query(
    address: SocketAddr,
    secret: &str,
    args: &[String],
    deadline: Duration,
) -> io::Result<Value> {
    timeout(deadline, async {
        let mut stream = timeout(Duration::from_secs(2), TcpStream::connect(address)).await??;
        timeout(
            Duration::from_secs(2),
            stream.write_all(format!("{}\n", json!({"token":secret,"args":args})).as_bytes()),
        )
        .await??;
        let mut response: Value =
            serde_json::from_slice(&read_line(&mut stream, 16 * 1024 * 1024).await?)
                .map_err(io::Error::other)?;
        if response["ok"] != true {
            return Err(io::Error::other(
                response["error"]
                    .as_str()
                    .unwrap_or("invalid admin response"),
            ));
        }
        Ok(response["data"].take())
    })
    .await?
}

#[cfg(test)]
mod tests;
