//! Server queues: the ordering and admission rules a single Rift applies to
//! its own tickets, which the cluster's queue service
//! (`crate::cluster::queues`) applies to the tickets of every peer, and the
//! seam through which a cluster peer learns the network-wide queues.
//!
//! A single Rift keeps its tickets and leases in memory and decides alone.
//! A cluster peer keeps its own players' tickets the same way, reports them
//! with its leases to the leader, and gets back a [`NetworkView`]: the
//! network-wide order, the head of each queue and the slots other peers
//! hold. The session code decides from that view with the same functions.
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::Duration,
};

/// Tickets one queue holds.
pub const MAX_TICKETS: usize = 1024;
/// How long a ticket waits before it expires.
pub const TICKET_LIFETIME: Duration = Duration::from_secs(300);
/// How long a peer trusts the leader's last view for queue admissions.
pub const VIEW_LIFETIME: Duration = Duration::from_secs(5);

/// Where a ticket with `priority` and `stamp` enters `line`: behind every
/// ticket with a higher priority, and behind those with the same priority
/// and an earlier or equal stamp. A single Rift stamps nothing (`()`), so a
/// new ticket goes behind every ticket of the same or a higher priority; the
/// leader stamps arrivals, so a ticket submitted again after a leader change
/// takes its old place.
pub fn insertion_point<T, S: Ord>(
    line: &VecDeque<T>,
    priority: i64,
    stamp: &S,
    key: impl Fn(&T) -> (i64, &S),
) -> usize {
    line.partition_point(|ticket| {
        let (ahead, ahead_stamp) = key(ticket);
        ahead > priority || (ahead == priority && ahead_stamp <= stamp)
    })
}

/// The slots of a destination with `capacity`: per registered instance for
/// a service group (`members`), so a group without instances has none.
pub fn slots(capacity: usize, members: Option<&BTreeSet<String>>) -> usize {
    capacity * members.map_or(1, BTreeSet::len)
}

/// Whether a session holding `servers` holds a slot of `destination`: the
/// destination itself or, for a service group, one of its instances.
pub fn contains_destination(
    servers: &BTreeSet<String>,
    destination: &str,
    members: Option<&BTreeSet<String>>,
) -> bool {
    servers.contains(destination) || members.is_some_and(|members| !servers.is_disjoint(members))
}

/// Players `counts` (by server) puts on `destination` or its instances.
pub fn destination_count(
    counts: &BTreeMap<String, usize>,
    destination: &str,
    members: Option<&BTreeSet<String>>,
) -> usize {
    counts.get(destination).copied().unwrap_or(0)
        + members.map_or(0, |members| {
            members
                .iter()
                .map(|server| counts.get(server).copied().unwrap_or(0))
                .sum()
        })
}

/// Whether a player who does not bypass the queue must wait: every slot is
/// used, or another ticket is first in line.
pub fn must_wait(used: usize, slots: usize, someone_else_first: bool) -> bool {
    used >= slots || someone_else_first
}

/// A ticket anywhere in the network: the peer whose player holds it and
/// that player's connection id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TicketRef {
    pub peer: String,
    pub id: u64,
}

/// One queue as the leader sees it, told to one peer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestinationView {
    /// The priority of every ticket in the network, in queue order.
    pub priorities: Vec<i64>,
    /// The ticket first in line.
    pub head: Option<TicketRef>,
    /// Slots that sessions on other peers hold (connecting or joined).
    pub others_used: usize,
    /// The network-wide position (from 1) of each of this peer's tickets,
    /// by connection id.
    pub mine: BTreeMap<u64, usize>,
}

/// The network-wide queues as the leader last told this peer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkView {
    pub destinations: BTreeMap<String, DestinationView>,
}

/// One of this peer's tickets, as reported to the leader.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalTicket {
    pub id: u64,
    pub destination: String,
    pub priority: i64,
    /// The arrival a leader stamped on it; `None` until a leader saw it.
    pub arrival: Option<u64>,
}

/// What a peer reports to the leader: its live tickets and the slots its
/// sessions hold, by destination.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalReport {
    pub tickets: Vec<LocalTicket>,
    pub used: BTreeMap<String, usize>,
}

/// What a cluster attaches to the extension host (`QueueHost::attach`).
pub trait QueueLink: Send + Sync {
    /// This peer's name.
    fn peer(&self) -> &str;
    /// This peer's tickets or leases changed: report them soon.
    fn changed(&self);
    /// The network-wide queue lengths, on the leader once it has heard from
    /// every peer; `None` elsewhere.
    fn leader_lengths(&self) -> Option<BTreeMap<String, usize>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unstamped_tickets_go_behind_their_priority_and_stamped_ones_take_their_place() {
        let line: VecDeque<(i64, ())> = [(5, ()), (5, ()), (0, ())].into();
        assert_eq!(insertion_point(&line, 5, &(), |t| (t.0, &t.1)), 2);
        assert_eq!(insertion_point(&line, 9, &(), |t| (t.0, &t.1)), 0);
        assert_eq!(insertion_point(&line, -1, &(), |t| (t.0, &t.1)), 3);
        let stamped: VecDeque<(i64, u64)> = [(5, 10), (5, 30), (0, 20)].into();
        assert_eq!(insertion_point(&stamped, 5, &20, |t| (t.0, &t.1)), 1);
        assert_eq!(insertion_point(&stamped, 0, &5, |t| (t.0, &t.1)), 2);
        assert_eq!(insertion_point(&stamped, 0, &25, |t| (t.0, &t.1)), 3);
    }

    #[test]
    fn group_slots_and_counts_cover_every_instance() {
        let members = BTreeSet::from(["g-1".to_owned(), "g-2".to_owned()]);
        assert_eq!(slots(3, Some(&members)), 6);
        assert_eq!(slots(3, None), 3);
        let counts = BTreeMap::from([("g-1".to_owned(), 2), ("lobby".to_owned(), 4)]);
        assert_eq!(destination_count(&counts, "g", Some(&members)), 2);
        assert_eq!(destination_count(&counts, "lobby", None), 4);
        let held = BTreeSet::from(["g-2".to_owned()]);
        assert!(contains_destination(&held, "g", Some(&members)));
        assert!(must_wait(2, 2, false));
        assert!(must_wait(0, 2, true));
        assert!(!must_wait(1, 2, false));
    }
}
