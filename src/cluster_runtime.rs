//! The runtime of a cluster peer (docs/clustering.md, plan step 7): what
//! `runtime::serve` adds when this process is one peer of a cluster.
//!
//! - [`prepare`] coordinates the managed servers before their workers
//!   launch ([`Coordination`]) and follows the instance registry and the
//!   moves of named servers that the leader writes.
//! - [`attach`] makes this peer's players and bus part of the cluster.
//! - [`lead`] registers the leader tasks: once this peer leads and has
//!   applied everything committed before its term, it recovers what the
//!   previous leader left half done, makes the writes derived from the
//!   configuration, fences and reconciles every node agent, and runs the
//!   permission expiry sweeper and scheduled backups, until it loses the
//!   leadership. The scaler and instance changes run in the control loop,
//!   gated by [`Coordination::acting_term`].
//! - [`follow`] keeps a follower's configuration and managed servers in
//!   step with the registry the leader writes.
//! - [`serve_requests`]: a follower hands moves, backups and restores and
//!   cache warming to the leader ([`forward`], [`forward_move`]), which
//!   runs them for the operator who asked and audits them under that name.
//!
//! A single Rift calls none of this.
use crate::{
    control,
    runtime::{InstanceResult, Snapshot, blocking, change_instance, instance_snapshot},
};
use rift::{
    cluster::{
        Cluster, bus,
        leader::{self, Coordination},
        node::LeaderContext,
        players::Presence,
    },
    config::{MovedServer, NodeConfig, ServiceInstance},
    instance_login::Logins,
    instance_store::{Store, events::InstanceEvents},
    managed::Coordinator,
    routing::Backend,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io, sync::Arc, time::Duration};
use tokio::sync::{mpsc, watch};

/// How long a follower waits for the leader to create or remove an
/// instance; creations may download software first.
const FORWARD_WAIT: Duration = Duration::from_secs(16 * 60);

/// A change the leader wrote that a follower follows.
pub enum Update {
    /// The ready registrations, as `Config.instances` holds them.
    Instances(BTreeMap<String, ServiceInstance>),
    /// The moves of named servers.
    Moved(BTreeMap<String, MovedServer>),
    /// The peer-nodes, as the peer table describes them
    /// (`rift::cluster::peer_nodes`).
    PeerNodes(BTreeMap<String, NodeConfig>),
}

/// What [`prepare`] set up, for [`attach`].
pub struct Prepared {
    coordination: Coordination,
    updates: mpsc::UnboundedReceiver<Update>,
}

/// Coordinate `snapshot`'s managed servers through `cluster`, and follow
/// the registry and the moves in `store`. Call before the workers launch.
pub fn prepare(cluster: &Cluster, snapshot: &Snapshot, store: &Arc<Store>) -> io::Result<Prepared> {
    let coordination = Coordination::start(cluster, &snapshot.managed, store)?;
    let (sender, updates) = mpsc::unbounded_channel();
    let instances = sender.clone();
    store.watch_instances(move |ready| {
        let _ = instances.send(Update::Instances(ready));
    });
    let peer_nodes = sender.clone();
    store.watch_moved_servers(move |moved| {
        let _ = sender.send(Update::Moved(moved));
    });
    // Peers that join, leave or change their labels change the nodes.
    let mut registry = rift::cluster::peer_nodes::subscribe();
    tokio::spawn(async move {
        while registry.changed().await.is_ok() {
            let nodes = registry.borrow_and_update().clone();
            if let Some(nodes) = nodes
                && peer_nodes.send(Update::PeerNodes(nodes)).is_err()
            {
                return;
            }
        }
    });
    // `rift.events.instances` is an applied event: every peer announces a
    // change when it applies here, so the control loop announces none.
    let nodes = snapshot.nodes.clone();
    let events = InstanceEvents::new(snapshot.messaging.clone(), move |node: &str, port: u16| {
        nodes
            .config()
            .get(node)
            .map(|node| node.backend_address(port))
    });
    store.watch_events(Arc::new(events))?;
    Ok(Prepared {
        coordination,
        updates,
    })
}

/// This process as a peer of its cluster, attached.
pub struct Peer {
    pub cluster: Cluster,
    pub coordination: Coordination,
    presence: Arc<Presence>,
    bus: Option<bus::Attached>,
    /// The queues' link to the leader's queue service; dropping it detaches.
    _queues: rift::cluster::queues::Attached,
    /// What the leader wrote that this peer follows ([`follow`]).
    pub updates: mpsc::UnboundedReceiver<Update>,
    /// The node agent this peer runs in its own process ([`embed_agent`]).
    agent: Option<EmbeddedAgent>,
}

/// Make this peer's players and bus part of `cluster`: presence with the
/// process-wide identity, routed player actions, the federated bus, node
/// sessions, bus logins and the leader's stream service. Call once the
/// broker, the streams and the QUIC listener exist, before the first login.
pub async fn attach(
    cluster: &Cluster,
    prepared: Prepared,
    snapshot: &Snapshot,
    logins: Option<Arc<Logins>>,
    address: Option<bus::BusAddress>,
    listening: bool,
) -> io::Result<Peer> {
    let parts = rift::cluster::attach::prepared().ok_or_else(|| {
        io::Error::other(
            "cluster: the components of this peer were not registered before it started",
        )
    })?;
    let presence = rift::cluster::players::attach(
        cluster,
        &parts.registry,
        snapshot.extensions.players(),
        snapshot.managed.service_states(),
        Some(&snapshot.perms),
        true,
    )
    .await?;
    let presence = Arc::new(presence);
    prepared.coordination.set_presence(presence.clone());
    let options = bus::AttachOptions {
        streams: (*snapshot.messaging_streams).clone(),
        nodes: listening.then(|| snapshot.nodes.logins().clone()),
        logins,
        address,
    };
    let bus = bus::attach(cluster, &snapshot.messaging, &parts.bus, options).await?;
    // Network-wide server queues, served by the leader (plan step 9).
    let queues = rift::cluster::queues::attach(cluster, snapshot.extensions.queue_host());
    // This peer is a node too, served by an agent in this process.
    let agent = embed_agent(cluster, snapshot)?;
    Ok(Peer {
        cluster: cluster.clone(),
        coordination: prepared.coordination,
        presence,
        bus: Some(bus),
        _queues: queues,
        updates: prepared.updates,
        agent,
    })
}

/// The node agent a cluster peer runs in its own process, with its tasks.
struct EmbeddedAgent {
    agent: Arc<rift::node::agent::Agent>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// Run this peer's node (docs/clustering.md, "Servers on the peers' own
/// hosts"): an agent named after the peer, whose servers live below the
/// directory of `rift.lua`, where servers on the proxy's host lived before
/// the cluster (so `rift cluster init`'s converted instances stay in
/// place), reading templates from this peer's materialized template
/// storage. It answers on this peer's broker; the cluster routes the
/// leader's commands to it and its state to the leader. Witnesses run none.
fn embed_agent(cluster: &Cluster, snapshot: &Snapshot) -> io::Result<Option<EmbeddedAgent>> {
    let settings = cluster.settings();
    if settings.witness {
        return Ok(None);
    }
    // A `nodes.<peer>` entry means a `rift node` agent of its own serves
    // this host under the peer's name; two agents must not answer.
    if snapshot
        .config
        .nodes
        .get(&settings.peer)
        .is_some_and(|node| !node.peer)
    {
        log_info!(
            "cluster: nodes.{} is configured, so its `rift node` agent serves this host instead of an embedded one",
            settings.peer
        );
        return Ok(None);
    }
    let secret = snapshot
        .forwarding_secret
        .as_ref()
        .and_then(|secret| String::from_utf8(secret.to_vec()).ok());
    let options = rift::node::agent::Options::embedded(
        &settings.peer,
        settings.directory.clone(),
        Some(snapshot.config.template_storage.clone()),
        secret,
    );
    let agent = rift::node::agent::Agent::new(options)?;
    let monitor = tokio::spawn(agent.clone().monitor());
    let serving = {
        let agent = agent.clone();
        let broker = snapshot.messaging.clone();
        let name = settings.peer.clone();
        tokio::spawn(async move {
            let error = agent.serve_broker(broker).await;
            log_error!("cluster: the node agent of peer {name} stopped: {error}");
        })
    };
    log_info!(
        "cluster: peer {} is a node; its servers live below {}",
        settings.peer,
        settings.directory.display()
    );
    Ok(Some(EmbeddedAgent {
        agent,
        tasks: vec![monitor, serving],
    }))
}

impl Peer {
    /// Let this peer's embedded agent start the servers whose definition in
    /// the published configuration (`current`) has `keep_running` as kept
    /// servers, which outlive this process.
    pub fn follow_keep_running(&self, current: watch::Receiver<Arc<Snapshot>>) {
        if let Some(embedded) = &self.agent {
            embedded.agent.set_keep(Arc::new(move |name: &str| {
                current
                    .borrow()
                    .config
                    .managed_servers
                    .get(name)
                    .is_some_and(|server| server.keep_running)
            }));
        }
    }

    /// The first steps of leaving (docs/clustering.md, "Rolling restarts"),
    /// once new logins stopped with the listeners: hand the leadership to
    /// another voter if this peer has it, then send the players to the
    /// other peers (the Transfer packet for 1.20.5+ clients, a reconnect
    /// message for the others).
    pub async fn hand_over(&self) {
        if self.cluster.leadership().is_leader {
            match self.cluster.admin(&["transfer-leader".to_owned()]).await {
                Ok(result) => log_info!("cluster: leaving: {result}"),
                Err(error) => {
                    log_warn!("cluster: leaving without handing over the leadership: {error}")
                }
            }
        }
        self.presence.hand_off().await;
    }

    /// Stop being a peer, once the sessions drained (their claims are
    /// released as they end): no more presence, bus federation, leader
    /// tasks or Raft.
    pub async fn shutdown(mut self) {
        *PEER.write().unwrap_or_else(|e| e.into_inner()) = None;
        self.coordination.stop();
        // The servers on this peer's host stop with it.
        if let Some(embedded) = self.agent.take() {
            for task in &embedded.tasks {
                task.abort();
            }
            log_info!("cluster: stopping the servers on this peer's host");
            embedded.agent.shutdown().await;
        }
        self.presence.stop();
        if let Some(bus) = self.bus.take() {
            bus.shutdown();
        }
        self.cluster.shutdown().await;
    }
}

/// Run the leader tasks whenever this peer leads (see the module
/// documentation); `current` follows the published snapshot.
pub fn lead(cluster: &Cluster, current: watch::Receiver<Arc<Snapshot>>) {
    cluster.on_leader_gain(move |context| lead_term(context, current.clone()));
}

async fn lead_term(context: LeaderContext, current: watch::Receiver<Arc<Snapshot>>) {
    if !leader::caught_up(&context).await {
        return;
    }
    let term = context.term;
    log_info!(
        "cluster: leading in term {term}: recovery, scaling, managed servers and node control run on this peer"
    );
    leader::in_term(term, async move {
        take_charge(&current).await;
        tokio::join!(
            fence_and_reconcile(context, current.clone()),
            sweep(current.clone()),
            crate::backups::schedule(current.clone()),
        );
    })
    .await;
}

/// What a new leader does once: finish or abort the creations, removals
/// and moves the previous leader left, and make the writes derived from the
/// configuration (pruned groups, nodes and server states, the permission
/// upgrade and the mirrored `extensions.groups`).
async fn take_charge(current: &watch::Receiver<Arc<Snapshot>>) {
    let snapshot = current.borrow().clone();
    let result = blocking(move || {
        if let Some(store) = &snapshot.instance_store {
            store.recover(&snapshot.config)?;
            let pruned = store.prune_server_states(snapshot.config.managed_servers.keys())?;
            if !pruned.is_empty() {
                log_info!(
                    "cluster: forgot the states of removed servers {}",
                    pruned.join(", ")
                );
            }
        }
        snapshot.prune_groups();
        snapshot.prune_nodes();
        let authority = snapshot.extensions.permissions().clone();
        match authority.migrate_once(|| rift::extensions::upgrade_nodes(&snapshot.config)) {
            Ok(Some(applied)) if applied.ops > 0 => log_info!(
                "copied {} permission changes from the extension store into revision {}",
                applied.ops,
                applied.revision
            ),
            Ok(_) => {}
            Err(error) => log_error!("the permission upgrade did not run: {error}"),
        }
        snapshot.extensions.mirror_permission_groups();
        Ok(())
    })
    .await;
    if let Err(error) = result {
        log_error!("cluster: recovery after gaining the leadership failed: {error}");
    }
}

/// Fence every node agent for this term, then reconcile it, as a restarted
/// controller adopts servers: a peer that held node states as a follower
/// would not see them reconnect. An agent that reconnects is reconciled
/// again; one without fencing gets no control command and is reported once.
async fn fence_and_reconcile(context: LeaderContext, current: watch::Receiver<Arc<Snapshot>>) {
    let nodes = current.borrow().nodes.clone();
    let configured = current.clone();
    leader::fence_and_reconcile(
        context,
        nodes,
        move || configured.borrow().config.nodes.keys().cloned().collect(),
        move |node| current.borrow().managed.names_on(node),
    )
    .await;
}

/// The permission expiry sweeper, on the leader only.
async fn sweep(current: watch::Receiver<Arc<Snapshot>>) {
    loop {
        let interval = current.borrow().config.permissions.sweep_interval;
        tokio::time::sleep(interval).await;
        let authority = current.borrow().extensions.permissions().clone();
        let result = blocking(move || {
            authority
                .sweep()
                .map_err(|error| io::Error::other(error.to_string()))
        })
        .await;
        if let Err(error) = result {
            log_error!("permission expiry sweep failed: {error}");
        }
    }
}

/// An instance request as a follower hands it to the leader; `None` for a
/// move, which runs where `rift admin move` prepared it.
pub fn encode_instance(operation: &control::Operation) -> Option<Value> {
    match operation {
        control::Operation::CreateInstance { group } => Some(json!({ "create": group })),
        control::Operation::RemoveInstance { name } => Some(json!({ "remove": name })),
        _ => None,
    }
}

/// The instance request [`encode_instance`] made.
pub fn decode_instance(request: &Value) -> Option<control::Operation> {
    if let Some(group) = request["create"].as_str() {
        return Some(control::Operation::CreateInstance {
            group: group.to_owned(),
        });
    }
    request["remove"]
        .as_str()
        .map(|name| control::Operation::RemoveInstance {
            name: name.to_owned(),
        })
}

/// On a follower: have the leader run `operation`. The answer is marked
/// `forwarded`: nothing changes here until the leader's write applies.
pub async fn forward_change(
    coordination: Coordination,
    operation: control::Operation,
    active: Arc<Snapshot>,
) -> InstanceResult {
    let leader = coordination
        .cluster()
        .leadership()
        .leader_name
        .unwrap_or_else(|| "the leader".to_owned());
    // A move this peer prepared while it led; followers hand moves over
    // whole (`forward_move`).
    let Some(request) = encode_instance(&operation) else {
        return Err(control::Error::conflict(format!(
            "this peer stopped leading the cluster during the move; run rift admin move again, and {leader} runs it"
        )));
    };
    match coordination.forward_instance(request, FORWARD_WAIT).await {
        Ok(mut response) => {
            if let Some(object) = response.as_object_mut() {
                object.insert("forwarded".to_owned(), json!(true));
                object.insert("leader".to_owned(), json!(leader));
            }
            Ok((active, response))
        }
        Err(error) => Err(control::Error {
            status: 503,
            message: format!("the cluster leader {leader} did not run it: {error}"),
        }),
    }
}

/// On the leader: run `operation`, decided in the current term. A removal
/// the scaler decided waits for every peer's consent first, like an idle
/// stop.
pub async fn leader_change(
    coordination: Coordination,
    operation: control::Operation,
    actor: String,
    active: Arc<Snapshot>,
) -> InstanceResult {
    if actor == "scaler"
        && let control::Operation::RemoveInstance { name } = &operation
        && !coordination.may_stop(name).await
    {
        return Err(control::Error::conflict(format!(
            "{name} stays: a peer has a player on the way there, was unreachable, or an expired peer's players may still be on it"
        )));
    }
    change_instance(operation, &active).await
}

/// On a follower: bring the configuration and the managed servers in step
/// with the registry (`instances`) and the moves (`moved`) the leader wrote.
/// The answer is marked `synced`, and `unchanged` when nothing differed.
pub async fn follow(
    active: Arc<Snapshot>,
    instances: Option<BTreeMap<String, ServiceInstance>>,
    moved: Option<BTreeMap<String, MovedServer>>,
    peer_nodes: Option<BTreeMap<String, NodeConfig>>,
) -> InstanceResult {
    let mut config = active.config.clone();
    let mut retired: Vec<String> = Vec::new();
    let mut added: Vec<String> = Vec::new();
    let mut relocated: Vec<String> = Vec::new();
    // First the nodes, which registrations on a new peer's node need.
    let mut renodes = false;
    if let Some(peers) = &peer_nodes {
        let mut nodes: BTreeMap<String, NodeConfig> = config
            .nodes
            .iter()
            .filter(|(_, node)| !node.peer)
            .map(|(name, node)| (name.clone(), node.clone()))
            .collect();
        rift::cluster::peer_nodes::add(&mut nodes, peers);
        if nodes != config.nodes {
            config.nodes = nodes;
            renodes = true;
        }
    }
    if let Some(ready) = &instances {
        for (name, instance) in &active.config.instances {
            if ready.get(name) != Some(instance) {
                retired.push(name.clone());
                config.instances.remove(name);
                config.backends.remove(name);
                config.managed_servers.remove(name);
                config.draining.remove(name);
            }
        }
        let fresh: BTreeMap<String, ServiceInstance> = ready
            .iter()
            .filter(|(name, instance)| active.config.instances.get(*name) != Some(*instance))
            .map(|(name, instance)| (name.clone(), instance.clone()))
            .collect();
        added.extend(fresh.keys().cloned());
        if !fresh.is_empty() {
            config.restore_instances(fresh)?;
        }
    }
    if let Some(moved) = &moved
        && *moved != active.config.moved_servers
    {
        // Undo the moves that ended or changed, then apply the current ones.
        for (name, entry) in &active.config.moved_servers {
            if moved.get(name) == Some(entry) {
                continue;
            }
            let backend = Backend::parse(&entry.configured_backend).map_err(|error| {
                control::Error::invalid(format!("managed_servers.{name}: {error}"))
            })?;
            config.backends.insert(name.clone(), backend);
            if let Some(server) = config.managed_servers.get_mut(name) {
                server.node = Some(entry.configured_node.clone());
            }
            config.moved_servers.remove(name);
            relocated.push(name.clone());
        }
        config.restore_moved_servers(moved)?;
        for name in moved.keys() {
            if active.config.moved_servers.get(name) != moved.get(name) && !relocated.contains(name)
            {
                relocated.push(name.clone());
            }
        }
    }
    if retired.is_empty() && added.is_empty() && relocated.is_empty() && !renodes {
        return Ok((active, json!({ "synced": true, "unchanged": true })));
    }
    let next = instance_snapshot(config, &active)?;
    if renodes {
        let logins = rift::node::identities(&next.config.nodes)?;
        next.nodes.reconfigure(&next.config.nodes, logins)?;
    }
    for name in &retired {
        if let Err(error) = next.managed.remove(name).await {
            log_warn!("cluster: {name} was removed by the leader; its worker here: {error}");
        }
    }
    for name in &relocated {
        let automatic = next
            .managed
            .snapshot()
            .into_iter()
            .find(|server| server.name == *name)
            .is_none_or(|server| server.automatic_enabled);
        if let Err(error) = next.managed.retire_moved(name).await {
            log_warn!("cluster: {name} moved; its old worker here: {error}");
        }
        if let Err(error) = next.managed.add_moved(&next.config, name, automatic) {
            log_warn!("cluster: {name} moved; its new worker here: {error}");
        }
    }
    for name in &added {
        if let Err(error) = next.managed.add(&next.config, name) {
            log_warn!("cluster: {name} was created by the leader; its worker here: {error}");
        }
    }
    Ok((
        Arc::new(next),
        json!({
            "synced": true,
            "removed": retired,
            "added": added,
            "moved": relocated,
            "nodes": renodes,
        }),
    ))
}

/// The kinds of the leader-only requests a follower hands the leader
/// ([`leader::ForwardedRequest`]). `move`: `{server, node, copy,
/// keep_source}`, as `rift admin move` takes them.
pub const MOVE: &str = "move";
/// `{server}`: the leader's `moving::describe` of a move, or `null`.
pub const MOVE_STATUS: &str = "move_status";
/// `{server}`: `rift admin move <server> --cancel`.
pub const MOVE_CANCEL: &str = "move_cancel";
/// `{instance}`: `rift admin backup`.
pub const BACKUP: &str = "backup";
/// `{instance, snapshot}`: `rift admin backups forget`, `snapshot` an id or
/// `all`.
pub const FORGET_BACKUPS: &str = "backups_forget";
/// `{instance, snapshot, node}`: `rift admin restore`; `snapshot` and
/// `node` are `null` for the newest snapshot and the instance's own node.
pub const RESTORE: &str = "restore";
/// `{template}`: `rift admin warm`; `template` is `null` for all of them.
pub const WARM: &str = "warm";

/// How long a follower waits for a cancel or a progress report.
pub const SHORT_WAIT: Duration = Duration::from_secs(10);
/// How often a follower asks the leader how a move it handed over is going.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

/// This process's coordination while it is a cluster peer, for the
/// requests it hands the leader ([`follower`]).
static PEER: std::sync::RwLock<Option<Coordination>> = std::sync::RwLock::new(None);

/// Run the leader-only requests followers hand this peer while it leads
/// (moves, backups and restores, cache warming), and hand the leader the
/// ones made here while this peer follows ([`follower`]). `commands` is the
/// administration queue of the control loop, which commits moves.
pub fn serve_requests(
    peer: &Peer,
    current: watch::Receiver<Arc<Snapshot>>,
    commands: mpsc::Sender<crate::admin::Command>,
    audit: Arc<crate::operator::Store>,
) {
    peer.coordination
        .set_request_handler(Arc::new(move |request: leader::ForwardedRequest| {
            let snapshot = current.borrow().clone();
            let commands = commands.clone();
            let audit = audit.clone();
            let answer: rift::managed::CoordinatorFuture<Result<Value, String>> =
                Box::pin(run_forwarded(snapshot, commands, audit, request));
            answer
        }));
    *PEER.write().unwrap_or_else(|e| e.into_inner()) = Some(peer.coordination.clone());
}

/// This peer's coordination while it follows: leader-only requests made
/// here go to the leader through it. `None` on a single Rift and on the
/// leader, which run them themselves.
pub fn follower() -> Option<Coordination> {
    let peer = PEER.read().unwrap_or_else(|e| e.into_inner()).clone();
    peer.filter(|peer| peer.acting_term().is_none())
}

/// On a follower: have the leader run the request `kind` with `body` for
/// `actor`, the operator this peer authenticated, waiting at most `wait`.
pub async fn forward(
    coordination: &Coordination,
    kind: &str,
    actor: &str,
    body: Value,
    wait: Duration,
) -> Result<Value, String> {
    let request = leader::ForwardedRequest {
        kind: kind.to_owned(),
        actor: actor.to_owned(),
        body,
    };
    coordination
        .forward_request(request, wait)
        .await
        .map_err(|error| match coordination.leader_name() {
            Ok(leader) => format!("the cluster leader {leader} did not run it: {error}"),
            Err(_) => error.to_string(),
        })
}

/// The body of a [`MOVE`] request.
pub fn move_body(request: &crate::moving::Request) -> Value {
    json!({
        "server": request.instance,
        "node": request.node,
        "copy": request.copy,
        "keep_source": request.keep_source,
    })
}

/// On a follower: have the leader run a move for `actor`. While it runs,
/// this peer asks the leader for its progress every second and shows it
/// under `move` like a move of its own (`moving::describe`); cancel it with
/// [`MOVE_CANCEL`] from any peer.
pub async fn forward_move(
    coordination: Coordination,
    actor: String,
    request: crate::moving::Request,
) -> Result<Value, String> {
    let shown = crate::moving::Forwarded::new(&request.instance);
    let status = json!({ "server": request.instance });
    let body = move_body(&request);
    let work = forward(&coordination, MOVE, &actor, body, crate::moving::DEADLINE);
    let mut work = std::pin::pin!(work);
    let mut tick = tokio::time::interval(PROGRESS_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            result = &mut work => return result,
            _ = tick.tick() => {
                let asked = leader::ForwardedRequest {
                    kind: MOVE_STATUS.to_owned(),
                    actor: actor.clone(),
                    body: status.clone(),
                };
                match coordination.forward_request(asked, SHORT_WAIT).await {
                    Ok(report) => shown.report(report),
                    Err(error) => log_debug!(
                        "cluster: no progress report of the move of {} from the leader: {error}",
                        request.instance
                    ),
                }
            }
        }
    }
}

/// On the leader: run a request a follower handed it, in the leader's term,
/// and record it in the audit trail under the operator who made it.
async fn run_forwarded(
    snapshot: Arc<Snapshot>,
    commands: mpsc::Sender<crate::admin::Command>,
    audit: Arc<crate::operator::Store>,
    request: leader::ForwardedRequest,
) -> Result<Value, String> {
    let leader::ForwardedRequest { kind, actor, body } = request;
    let text = |key: &str| field(&kind, &body, key);
    let optional = |key: &str| body[key].as_str().map(str::to_owned);
    let (target, result) = match kind.as_str() {
        MOVE_STATUS => {
            let report = crate::moving::describe(&text("server")?);
            return Ok(report.unwrap_or(Value::Null));
        }
        MOVE => {
            let request = crate::moving::Request {
                instance: text("server")?,
                node: text("node")?,
                copy: body["copy"].as_bool().unwrap_or(true),
                keep_source: body["keep_source"].as_bool().unwrap_or(false),
            };
            let target = request.instance.clone();
            let committer = crate::moving::Committer::Admin(&commands);
            let result = crate::moving::run(snapshot, committer, request).await;
            (target, result)
        }
        MOVE_CANCEL => {
            let server = text("server")?;
            let result = crate::moving::cancel(&server);
            (server, result)
        }
        BACKUP => {
            let instance = text("instance")?;
            let result = crate::backups::backup_now(snapshot, &instance).await;
            (instance, result)
        }
        FORGET_BACKUPS => {
            let instance = text("instance")?;
            let which = text("snapshot")?;
            let result = crate::backups::forget(snapshot, &instance, &which).await;
            (instance, result)
        }
        RESTORE => {
            let request = crate::backups::Restore {
                instance: text("instance")?,
                snapshot: optional("snapshot"),
                node: optional("node"),
            };
            let target = request.instance.clone();
            let result = crate::backups::restore(snapshot, &commands, request).await;
            (target, result)
        }
        WARM => {
            let template = optional("template");
            let result = crate::admin::warm_templates(&snapshot, template.as_deref()).await;
            (template.unwrap_or_default(), result)
        }
        other => {
            return Err(format!(
                "this leader does not know the request {other:?}; run the same rift version on every peer"
            ));
        }
    };
    let (outcome, status) = if result.is_ok() {
        ("ok", 200)
    } else {
        ("rejected", 400)
    };
    let action = format!("forwarded.{kind}");
    if let Err(error) = audit
        .audit_async(&actor, &action, &target, outcome, status)
        .await
    {
        log_error!(
            "audit of {kind} {target} for {actor}, handed over by a follower, failed: {error}"
        );
    }
    result
}

/// The text field `key` of the body of a forwarded `kind` request.
fn field(kind: &str, body: &Value, key: &str) -> Result<String, String> {
    match body[key].as_str() {
        Some(value) => Ok(value.to_owned()),
        None => Err(format!("{kind} without {key}; upgrade rift on every peer")),
    }
}
