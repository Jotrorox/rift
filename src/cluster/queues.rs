//! Network-wide server queues (docs/queues.md, docs/clustering.md plan step
//! 9) and the plugin-facing cluster API of decisions 8 and 9: named leases
//! ([`leases`]) and job scopes ([`jobs`]).
//!
//! # Queues
//!
//! A player's ticket lives on the peer that holds the player's session, as
//! on a single Rift (`crate::extensions`). The **queue service runs on the
//! leader**:
//!
//! - Every peer reports its live tickets and the slots its sessions hold
//!   (its leases, by destination) to the leader over the `queue` stream:
//!   at once when they change and at least every [`SYNC_INTERVAL`]. A report
//!   is the peer's whole state, so it doubles as the heartbeat.
//! - The leader stamps each ticket it sees for the first time with its
//!   arrival and orders the tickets of every peer exactly as a single Rift
//!   orders its own ([`crate::extensions::queue::insertion_point`]): by
//!   priority, then arrival. It answers each report with the network-wide
//!   view for that peer ([`NetworkView`]): the order, the head of each
//!   queue, the slots other peers hold and the positions of the peer's own
//!   tickets.
//! - Slots count network-wide: the leases of every live peer, or the
//!   players the replicated registry puts on the destination, whichever is
//!   more. A peer's leases go with its tickets when the leader drops it.
//! - When the head of a queue may enter (a slot is free), the leader tells
//!   the head's peer at once with a fresh view; that peer's session then
//!   connects as on a single Rift. One head completes its entry before the
//!   next is considered, because the head stays first until its ticket is
//!   gone, and the slot it took counts from the peer's next report on.
//! - After a leader change every peer submits its tickets again with the
//!   arrivals the old leader stamped, so the new leader rebuilds the same
//!   order. It collects for [`SETTLE`] before its views count, so a ticket
//!   that has not been resubmitted yet is not jumped. A ticket whose peer
//!   disappears (the peer expired, restarted or stopped reporting for
//!   `cluster.peer_timeout`) is dropped with that peer's leases.
//!
//! Admission decisions stay on the peer that holds the session and use the
//! leader's last view; a view older than
//! [`crate::extensions::queue::VIEW_LIFETIME`] admits nobody from a queue
//! (direct entries still check the last known slot counts). Two peers can
//! therefore admit a direct entry each in the same moment and exceed the
//! capacity by one per peer until the next report; queue order itself is
//! decided only by the leader.
//!
//! # Wiring
//!
//! Register [`components`] (the `lease` entry kind) on every peer before
//! [`Cluster::start`], and call [`attach`] once the first generation of the
//! extension host exists; keep the returned [`Attached`] while the cluster
//! runs.
pub mod jobs;
pub mod leases;
#[cfg(test)]
mod tests;

use super::{Cluster, StreamKind, node, state::Component};
use crate::extensions::{
    QueueHost,
    queue::{self, DestinationView, LocalReport, NetworkView, QueueLink, TicketRef},
};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::{sync::Notify, task::JoinHandle, time::Instant};

/// How often a peer reports to the leader at least.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(1);
/// How long a new leader collects resubmitted tickets before its views
/// count and it lets heads in.
pub const SETTLE: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// A change waits this long for others before it is reported.
const DEBOUNCE: Duration = Duration::from_millis(20);
/// Arrivals are the leader's wall clock in milliseconds times this, plus a
/// sequence, so they stay unique and increasing within one leader.
const ARRIVALS_PER_MS: u64 = 1024;

/// The components every peer registers before the cluster starts: `lease`
/// entries. Queues need no entry kind: tickets are not replicated.
pub fn components() -> Vec<Arc<dyn Component>> {
    vec![Arc::new(leases::LeaseMachine)]
}

/// What travels on the `queue` stream.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Message {
    /// A peer's whole queue state, for the leader.
    Sync {
        incarnation: u64,
        report: LocalReport,
    },
    /// From the leader: the head of a queue may enter, with a fresh view.
    Turn { view: NetworkView },
}

/// The leader's answer to a report.
#[derive(Debug, Serialize, Deserialize)]
struct SyncReply {
    /// False while a new leader still collects resubmitted tickets: the
    /// view is not to be admitted by yet.
    settled: bool,
    /// The arrival stamped on each of the peer's tickets, by connection id.
    arrivals: Vec<(u64, u64)>,
    view: NetworkView,
}

/// A ticket's place among tickets of the same priority: its arrival, then
/// (for two leaders that stamped the same arrival) peer and connection id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Stamp {
    arrival: u64,
    peer: String,
    id: u64,
}

#[derive(Debug)]
struct Ticket {
    priority: i64,
    stamp: Stamp,
}

impl Ticket {
    fn is(&self, peer: &str, id: u64) -> bool {
        self.stamp.peer == peer && self.stamp.id == id
    }

    fn reference(&self) -> TicketRef {
        TicketRef {
            peer: self.stamp.peer.clone(),
            id: self.stamp.id,
        }
    }
}

/// What the leader knows about one peer.
struct Reporter {
    incarnation: u64,
    heard: Instant,
    used: BTreeMap<String, usize>,
}

/// The queue service while this peer leads.
struct Leading {
    term: u64,
    settled_at: Instant,
    next_arrival: u64,
    peers: BTreeMap<String, Reporter>,
    lines: BTreeMap<String, VecDeque<Ticket>>,
    /// The head each destination's peer was last told it may enter.
    turns: BTreeMap<String, TicketRef>,
}

impl Leading {
    fn new(term: u64, now: Instant) -> Self {
        Self {
            term,
            settled_at: now + SETTLE,
            next_arrival: 0,
            peers: BTreeMap::new(),
            lines: BTreeMap::new(),
            turns: BTreeMap::new(),
        }
    }

    fn settled(&self, now: Instant) -> bool {
        now >= self.settled_at
    }

    /// A new arrival: after every arrival seen, and not before the wall
    /// clock.
    fn arrival(&mut self) -> u64 {
        let clock = super::wall_clock_ms().saturating_mul(ARRIVALS_PER_MS);
        let arrival = self.next_arrival.max(clock);
        self.next_arrival = arrival + 1;
        arrival
    }

    fn drop_peer(&mut self, peer: &str) {
        self.peers.remove(peer);
        for line in self.lines.values_mut() {
            line.retain(|ticket| ticket.stamp.peer != peer);
        }
        self.lines.retain(|_, line| !line.is_empty());
        self.turns.retain(|_, head| head.peer != peer);
    }

    /// Take `peer`'s report: its tickets replace the ones it had.
    fn report(&mut self, peer: &str, incarnation: u64, report: LocalReport, now: Instant) {
        if self
            .peers
            .get(peer)
            .is_some_and(|known| known.incarnation != incarnation)
        {
            // A restarted peer: its old sessions are gone.
            self.drop_peer(peer);
        }
        let reported: BTreeMap<u64, &queue::LocalTicket> = report
            .tickets
            .iter()
            .map(|ticket| (ticket.id, ticket))
            .collect();
        for (destination, line) in &mut self.lines {
            line.retain(|ticket| {
                ticket.stamp.peer != peer
                    || reported
                        .get(&ticket.stamp.id)
                        .is_some_and(|reported| reported.destination == *destination)
            });
        }
        for ticket in &report.tickets {
            let known = self
                .lines
                .get(&ticket.destination)
                .is_some_and(|line| line.iter().any(|known| known.is(peer, ticket.id)));
            if known {
                continue;
            }
            let arrival = match ticket.arrival {
                // Submitted again after a leader change: its old place.
                Some(arrival) => {
                    self.next_arrival = self.next_arrival.max(arrival.saturating_add(1));
                    arrival
                }
                None => self.arrival(),
            };
            let stamp = Stamp {
                arrival,
                peer: peer.to_owned(),
                id: ticket.id,
            };
            let line = self.lines.entry(ticket.destination.clone()).or_default();
            let position = queue::insertion_point(line, ticket.priority, &stamp, |ticket| {
                (ticket.priority, &ticket.stamp)
            });
            line.insert(
                position,
                Ticket {
                    priority: ticket.priority,
                    stamp,
                },
            );
        }
        self.lines.retain(|_, line| !line.is_empty());
        self.peers.insert(
            peer.to_owned(),
            Reporter {
                incarnation,
                heard: now,
                used: report.used,
            },
        );
    }

    /// The arrivals of `peer`'s tickets.
    fn arrivals(&self, peer: &str) -> Vec<(u64, u64)> {
        self.lines
            .values()
            .flatten()
            .filter(|ticket| ticket.stamp.peer == peer)
            .map(|ticket| (ticket.stamp.id, ticket.stamp.arrival))
            .collect()
    }

    /// Slots of `destination` held by every live peer but `except`.
    fn used(&self, destination: &str, except: Option<&str>) -> usize {
        self.peers
            .iter()
            .filter(|(name, _)| Some(name.as_str()) != except)
            .map(|(_, reporter)| reporter.used.get(destination).copied().unwrap_or(0))
            .sum()
    }

    /// The network-wide view for `peer`.
    fn view(&self, peer: &str, slots: &BTreeMap<String, usize>) -> NetworkView {
        let destinations: BTreeSet<&String> = slots
            .keys()
            .chain(self.lines.keys())
            .chain(
                self.peers
                    .values()
                    .flat_map(|reporter| reporter.used.keys()),
            )
            .collect();
        NetworkView {
            destinations: destinations
                .into_iter()
                .map(|destination| {
                    let line = self.lines.get(destination);
                    let view = DestinationView {
                        priorities: line
                            .map(|line| line.iter().map(|ticket| ticket.priority).collect())
                            .unwrap_or_default(),
                        head: line.and_then(|line| line.front()).map(Ticket::reference),
                        others_used: self.used(destination, Some(peer)),
                        mine: line
                            .map(|line| {
                                line.iter()
                                    .enumerate()
                                    .filter(|(_, ticket)| ticket.stamp.peer == peer)
                                    .map(|(index, ticket)| (ticket.stamp.id, index + 1))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    };
                    (destination.clone(), view)
                })
                .collect(),
        }
    }

    /// Heads that may enter now and were not told yet: a slot is free,
    /// counting every peer's leases or the registry, whichever is more.
    fn turns(
        &mut self,
        slots: &BTreeMap<String, usize>,
        registry: &BTreeMap<String, usize>,
        now: Instant,
    ) -> Vec<TicketRef> {
        if !self.settled(now) {
            return Vec::new();
        }
        let mut turns = Vec::new();
        let heads: Vec<(String, TicketRef)> = self
            .lines
            .iter()
            .filter_map(|(destination, line)| {
                Some((destination.clone(), line.front()?.reference()))
            })
            .collect();
        self.turns
            .retain(|destination, head| heads.iter().any(|(d, h)| d == destination && *h == *head));
        for (destination, head) in heads {
            let Some(slots) = slots.get(&destination).copied() else {
                continue;
            };
            let used = self
                .used(&destination, None)
                .max(registry.get(&destination).copied().unwrap_or(0));
            if queue::must_wait(used, slots, false) {
                continue;
            }
            if self.turns.get(&destination) == Some(&head) {
                continue;
            }
            self.turns.insert(destination, head.clone());
            turns.push(head);
        }
        turns
    }
}

/// The queue service of one peer: idle unless it leads.
struct Service {
    me: String,
    host: QueueHost,
    cluster: Weak<node::Inner>,
    leading: Mutex<Option<Leading>>,
}

impl Service {
    fn cluster(&self) -> Option<Cluster> {
        Some(Cluster {
            inner: self.cluster.upgrade()?,
        })
    }

    fn gain(&self, term: u64) {
        *self.leading.lock().unwrap() = Some(Leading::new(term, Instant::now()));
    }

    fn end(&self, term: u64) {
        let mut leading = self.leading.lock().unwrap();
        if leading.as_ref().is_some_and(|leading| leading.term <= term) {
            *leading = None;
        }
    }

    /// Whether this peer leads with a lease in the term the service runs.
    fn leads(&self, term: u64) -> bool {
        self.cluster()
            .is_some_and(|cluster| cluster.is_leader_with_lease() && cluster.current_term() == term)
    }

    /// The network-wide lengths, once settled.
    fn lengths(&self) -> Option<BTreeMap<String, usize>> {
        let leading = self.leading.lock().unwrap();
        let leading = leading.as_ref()?;
        if !leading.settled(Instant::now()) || !self.leads(leading.term) {
            return None;
        }
        let mut lengths: BTreeMap<String, usize> = self
            .host
            .slots()
            .into_keys()
            .map(|destination| (destination, 0))
            .collect();
        for (destination, line) in &leading.lines {
            lengths.insert(destination.clone(), line.len());
        }
        Some(lengths)
    }

    /// Take a report of `peer` and answer it.
    fn sync(&self, peer: &str, incarnation: u64, report: LocalReport) -> Result<SyncReply, String> {
        let slots = self.host.slots();
        let now = Instant::now();
        let mut guard = self.leading.lock().unwrap();
        let leading = guard
            .as_mut()
            .filter(|leading| self.leads(leading.term))
            .ok_or_else(|| format!("{} does not run the queue service", self.me))?;
        leading.report(peer, incarnation, report, now);
        Ok(SyncReply {
            settled: leading.settled(now),
            arrivals: leading.arrivals(peer),
            view: leading.view(peer, &slots),
        })
    }

    /// Drop peers that stopped reporting, expired or restarted, then tell
    /// the heads that may enter. Returns the views to send, by peer.
    fn maintain(&self, timeout: Duration) -> Vec<(String, NetworkView)> {
        let Some(cluster) = self.cluster() else {
            return Vec::new();
        };
        let rows = match cluster.state().peers() {
            Ok(rows) => rows,
            Err(error) => {
                log_warn!("cluster queues: reading the peer table: {error}");
                return Vec::new();
            }
        };
        let slots = self.host.slots();
        let registry = self.host.registry();
        let now = Instant::now();
        let mut guard = self.leading.lock().unwrap();
        let Some(leading) = guard.as_mut().filter(|leading| self.leads(leading.term)) else {
            return Vec::new();
        };
        let gone: Vec<String> = leading
            .peers
            .iter()
            .filter(|(name, reporter)| {
                now.saturating_duration_since(reporter.heard) > timeout
                    || rows
                        .iter()
                        .find(|row| row.name == **name)
                        .is_none_or(|row| row.expired || row.incarnation > reporter.incarnation)
            })
            .map(|(name, _)| name.clone())
            .collect();
        for peer in gone {
            log_info!("cluster queues: dropping the tickets and slots of peer {peer}");
            leading.drop_peer(&peer);
        }
        let turns = leading.turns(&slots, &registry, now);
        turns
            .into_iter()
            .map(|head| (head.peer.clone(), leading.view(&head.peer, &slots)))
            .collect()
    }
}

/// The extension host's side of the link.
struct Link {
    me: String,
    notify: Notify,
    service: Weak<Service>,
}

impl QueueLink for Link {
    fn peer(&self) -> &str {
        &self.me
    }

    fn changed(&self) {
        self.notify.notify_one();
    }

    fn leader_lengths(&self) -> Option<BTreeMap<String, usize>> {
        self.service.upgrade()?.lengths()
    }
}

/// The queues of a peer, attached to its cluster. Dropping it stops its
/// tasks and lets the extension host decide alone again.
pub struct Attached {
    host: QueueHost,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Attached {
    fn drop(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
        self.host.detach();
    }
}

/// Make `host`'s queues network-wide on `cluster`: report this peer's
/// tickets and leases to the leader, run the queue service while this peer
/// leads, and take the leader's views and turns.
pub fn attach(cluster: &Cluster, host: QueueHost) -> Attached {
    let service = Arc::new(Service {
        me: cluster.name().to_owned(),
        host: host.clone(),
        cluster: Arc::downgrade(&cluster.inner),
        leading: Mutex::new(None),
    });
    let link = Arc::new(Link {
        me: cluster.name().to_owned(),
        notify: Notify::new(),
        service: Arc::downgrade(&service),
    });
    host.attach(link.clone());
    {
        let service = service.clone();
        let host = host.clone();
        let weak = Arc::downgrade(&cluster.inner);
        cluster
            .links()
            .on_request(StreamKind::Queue, move |peer, body: Bytes| {
                let service = service.clone();
                let host = host.clone();
                let weak = weak.clone();
                async move { handle(&service, &host, &weak, &peer, &body) }
            });
    }
    {
        let gain = service.clone();
        cluster.on_leader_gain(move |context| {
            let service = gain.clone();
            async move { lead(service, context).await }
        });
        let loss = service.clone();
        cluster.on_leader_loss(move |term| loss.end(term));
    }
    let tasks = vec![tokio::spawn(report(
        cluster.clone(),
        host.clone(),
        service.clone(),
        link,
    ))];
    Attached { host, tasks }
}

/// Answer a message on the `queue` stream.
fn handle(
    service: &Service,
    host: &QueueHost,
    cluster: &Weak<node::Inner>,
    peer: &str,
    body: &[u8],
) -> Result<Bytes, String> {
    let message: Message = serde_json::from_slice(body)
        .map_err(|error| format!("malformed queue message: {error}"))?;
    match message {
        Message::Sync {
            incarnation,
            report,
        } => {
            let reply = service.sync(peer, incarnation, report)?;
            serde_json::to_vec(&reply)
                .map(Bytes::from)
                .map_err(|error| error.to_string())
        }
        Message::Turn { view } => {
            // Only the leader's word counts.
            let leader = cluster
                .upgrade()
                .and_then(|inner| Cluster { inner }.leadership().leader_name);
            if leader.as_deref() == Some(peer) {
                host.apply(Some(view), &[]);
            }
            Ok(Bytes::new())
        }
    }
}

/// The queue service's leader task: settle, then drop vanished peers and
/// let heads in, once per [`SYNC_INTERVAL`] and whenever the leader's own
/// tickets change through a report.
async fn lead(service: Arc<Service>, context: node::LeaderContext) {
    service.gain(context.term);
    let timeout = context.cluster().shared().peer_timeout;
    let links = context.cluster().links().clone();
    loop {
        tokio::time::sleep(SYNC_INTERVAL / 4).await;
        if !context.has_lease() {
            continue;
        }
        for (peer, view) in service.maintain(timeout) {
            if peer == service.me {
                service.host.apply(Some(view), &[]);
                continue;
            }
            let Ok(body) = serde_json::to_vec(&Message::Turn { view }) else {
                continue;
            };
            let links = links.clone();
            tokio::spawn(async move {
                if let Err(error) = links
                    .request(&peer, StreamKind::Queue, Bytes::from(body), REQUEST_TIMEOUT)
                    .await
                {
                    log_debug!("cluster queues: telling {peer} its turn: {error}");
                }
            });
        }
    }
}

/// Report this peer's queue state to the leader whenever it changes, at
/// least every [`SYNC_INTERVAL`] and at once when the leader changes (which
/// resubmits every ticket with its stamped arrival).
async fn report(cluster: Cluster, host: QueueHost, service: Arc<Service>, link: Arc<Link>) {
    let mut leadership = cluster.subscribe_leadership();
    loop {
        tokio::select! {
            () = link.notify.notified() => tokio::time::sleep(DEBOUNCE).await,
            () = tokio::time::sleep(SYNC_INTERVAL) => {}
            changed = leadership.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
        if let Err(error) = sync_once(&cluster, &host, &service).await {
            log_debug!("cluster queues: report to the leader: {error}");
        }
    }
}

/// One report and its answer.
async fn sync_once(cluster: &Cluster, host: &QueueHost, service: &Service) -> Result<(), String> {
    // Every peer runs the same queues: without any there is nothing to say.
    if host.slots().is_empty() {
        return Ok(());
    }
    let Some(leader) = cluster.leadership().leader_name else {
        return Err("no leader".into());
    };
    let report = host.report();
    let reply = if leader == cluster.name() {
        service.sync(cluster.name(), cluster.incarnation(), report)?
    } else {
        let body = serde_json::to_vec(&Message::Sync {
            incarnation: cluster.incarnation(),
            report,
        })
        .map_err(|error| error.to_string())?;
        let answer = cluster
            .links()
            .request(
                &leader,
                StreamKind::Queue,
                Bytes::from(body),
                REQUEST_TIMEOUT,
            )
            .await
            .map_err(|error| error.to_string())?;
        serde_json::from_slice::<SyncReply>(&answer).map_err(|error| error.to_string())?
    };
    host.apply(reply.settled.then_some(reply.view), &reply.arrivals);
    Ok(())
}
