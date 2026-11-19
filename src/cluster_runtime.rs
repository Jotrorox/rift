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
    config::{MovedServer, ServiceInstance},
    instance_login::Logins,
    instance_store::Store,
    managed::Coordinator,
    routing::Backend,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::Arc,
    time::Duration,
};
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
    store.watch_moved_servers(move |moved| {
        let _ = sender.send(Update::Moved(moved));
    });
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
    Ok(Peer {
        cluster: cluster.clone(),
        coordination: prepared.coordination,
        presence,
        bus: Some(bus),
        _queues: queues,
        updates: prepared.updates,
    })
}

impl Peer {
    /// Hand the leadership to another voter if this peer has it: the first
    /// step of leaving (docs/clustering.md, "Rolling restarts"). New logins
    /// already stopped with the listeners.
    pub async fn hand_over(&self) {
        if !self.cluster.leadership().is_leader {
            return;
        }
        match self.cluster.admin(&["transfer-leader".to_owned()]).await {
            Ok(result) => log_info!("cluster: leaving: {result}"),
            Err(error) => {
                log_warn!("cluster: leaving without handing over the leadership: {error}")
            }
        }
    }

    /// Stop being a peer, once the sessions drained (their claims are
    /// released as they end): no more presence, bus federation, leader
    /// tasks or Raft.
    pub async fn shutdown(mut self) {
        self.coordination.stop();
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
    let mut done: BTreeSet<String> = BTreeSet::new();
    let mut reported: BTreeSet<String> = BTreeSet::new();
    loop {
        if context.has_lease() {
            let snapshot = current.borrow().clone();
            for node in snapshot.config.nodes.keys() {
                if done.contains(node) || !nodes.online(node) {
                    continue;
                }
                if let Err(error) = nodes.fence(node, context.term).await {
                    if reported.insert(node.clone()) {
                        log_warn!(
                            "cluster: node {node} is not fenced for term {}: {error}",
                            context.term
                        );
                    }
                    continue;
                }
                reported.remove(node);
                let command = rift::node::protocol::Command::Reconcile {
                    servers: snapshot.managed.names_on(node),
                };
                match nodes
                    .request(node, &command, Duration::from_secs(120))
                    .await
                {
                    Ok(result) => {
                        done.insert(node.clone());
                        let changed = result["stopped"].as_array().is_some_and(|v| !v.is_empty())
                            || result["removed"].as_array().is_some_and(|v| !v.is_empty());
                        if changed {
                            log_info!("node {node} reconciled: {result}");
                        }
                    }
                    Err(error) => log_error!("node {node} reconciliation failed: {error}"),
                }
            }
        }
        tokio::select! {
            reconnected = nodes.reconnected() => {
                for node in reconnected {
                    done.remove(&node);
                }
            }
            () = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
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
    let Some(request) = encode_instance(&operation) else {
        return Err(control::Error::conflict(format!(
            "instance moves run on the cluster leader; run rift admin move on {leader}"
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
) -> InstanceResult {
    let mut config = active.config.clone();
    let mut retired: Vec<String> = Vec::new();
    let mut added: Vec<String> = Vec::new();
    let mut relocated: Vec<String> = Vec::new();
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
    if retired.is_empty() && added.is_empty() && relocated.is_empty() {
        return Ok((active, json!({ "synced": true, "unchanged": true })));
    }
    let next = instance_snapshot(config, &active)?;
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
        }),
    ))
}
