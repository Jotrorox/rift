//! Subject scopes: whether and how a subject crosses the peers of a cluster
//! (`docs/clustering.md`, "Subject scopes"). On a single Rift every scope
//! behaves the same: the broker delivers to its own subscribers.

use rift_permissions::wire;

pub use rift_messaging::SubscriptionScope;

/// How messages on one subject travel between the peers of a cluster.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Built-in request subjects. Answered by the peer the publisher is
    /// connected to and never exported.
    Local,
    /// Events about replicated state. Every peer publishes them when it
    /// applies the entry, to its own subscribers only; never forwarded.
    Applied,
    /// Events about sessions, published on the peer where they happen.
    /// Forwarded only to peers whose remote (QUIC) clients have interest;
    /// in-process subscribers see only their own peer's events.
    Origin,
    /// `rift.node.<n>.cmd` and `rift.node.<n>.state`: routed to the peer
    /// that holds the node's session (commands) or to the leader (states)
    /// by an [`OwnerRouter`](super::OwnerRouter). Local without one.
    Owner,
    /// Everything else: user and plugin subjects, federated by interest.
    Cluster,
}

impl Scope {
    /// The scope of a literal `subject`.
    pub fn of(subject: &str) -> Self {
        subject_scope(subject)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Applied => "applied",
            Self::Origin => "origin",
            Self::Owner => "owner",
            Self::Cluster => "cluster",
        }
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `rift.events.*` subjects about replicated state.
const APPLIED_EVENTS: [&str; 5] = [
    "rift.events.instances",
    wire::EVENTS, // rift.events.permissions
    "rift.events.reload",
    "rift.events.servers",
    "rift.events.cluster",
];

/// Request namespaces Rift answers itself, besides the permission requests.
const LOCAL_NAMESPACES: [&str; 3] = ["rift.control", "rift.players", "rift.network"];

/// Backends report their state here; the peer that receives a report
/// shares it with the others itself.
const SERVER_STATE: &str = "rift.server.state";

/// The scope of a literal subject, per the table in `docs/clustering.md`.
pub fn subject_scope(subject: &str) -> Scope {
    if LOCAL_NAMESPACES
        .iter()
        .any(|namespace| within(subject, namespace))
        || subject == SERVER_STATE
        || wire::REQUESTS.contains(&subject)
    {
        return Scope::Local;
    }
    // The change event after every committed permission write, and the
    // retained change log, are derived from the replicated store on each
    // peer. Pushes for one player (`rift.perms.update.<uuid>`) and the
    // resync broadcast come from one peer and are `Cluster`.
    if APPLIED_EVENTS.contains(&subject) || subject == wire::UPDATE || subject == wire::CHANGES {
        return Scope::Applied;
    }
    if within(subject, "rift.events") {
        return Scope::Origin;
    }
    if within(subject, "rift.node") {
        let mut tokens = subject.split('.').skip(2);
        return match (tokens.next(), tokens.next(), tokens.next()) {
            (Some(_), Some("cmd" | "state"), None) => Scope::Owner,
            // The rest of the node namespace is reserved for the controller
            // and its agents, and its commands carry secrets.
            _ => Scope::Local,
        };
    }
    Scope::Cluster
}

/// Whether `subject` is `namespace` or below it.
pub(crate) fn within(subject: &str, namespace: &str) -> bool {
    subject == namespace
        || subject
            .strip_prefix(namespace)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// Whether a subscription to `pattern` can receive messages from other
/// peers: some subject it matches is `Cluster`, or `Origin` for a remote
/// client. Patterns that cannot are not announced as interest.
pub(crate) fn pattern_may_federate(pattern: &str, client: bool) -> bool {
    const NEVER: [&str; 10] = [
        "rift.control",
        "rift.control.>",
        "rift.players",
        "rift.players.>",
        "rift.network",
        "rift.network.>",
        SERVER_STATE,
        "rift.node",
        "rift.node.>",
        wire::CHANGES,
    ];
    if NEVER
        .iter()
        .chain(&wire::REQUESTS)
        .chain(&APPLIED_EVENTS)
        .chain(&[wire::UPDATE])
        .any(|namespace| covers(namespace, pattern))
    {
        return false;
    }
    // In-process subscribers never receive another peer's events.
    client || !(covers("rift.events", pattern) || covers("rift.events.>", pattern))
}

/// Whether every subject matched by `requested` is also matched by `allowed`.
pub(crate) fn covers(allowed: &str, requested: &str) -> bool {
    let mut a = allowed.split('.');
    let mut r = requested.split('.');
    loop {
        match (a.next(), r.next()) {
            (None, None) => return true,
            (Some(">"), Some(_)) => return true,
            (Some("*"), Some(x)) if x != ">" => {}
            (Some(x), Some(y)) if x == y && y != ">" => {}
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_subjects_have_the_scopes_of_the_design() {
        for subject in [
            "rift.control.create",
            "rift.control.status",
            "rift.control",
            "rift.players.kick",
            "rift.players.list",
            "rift.network.connect",
            "rift.network.players",
            crate::players_service::PREFIX,
            crate::network_service::PREFIX,
            crate::service_state::SUBJECT,
            wire::HELLO,
            wire::SUBJECT_GET,
            wire::SUBJECT_BATCH,
            wire::GROUPS_GET,
            wire::APPLY,
            wire::COMMAND,
            wire::REGISTER,
            wire::CONTEXT,
            "rift.node.a.files",
            "rift.node",
        ] {
            assert_eq!(subject_scope(subject), Scope::Local, "{subject}");
        }
        for subject in [
            "rift.events.instances",
            "rift.events.permissions",
            "rift.events.reload",
            "rift.events.servers",
            "rift.events.cluster",
            crate::service_state::EVENTS,
            wire::EVENTS,
            wire::UPDATE,
            wire::CHANGES,
        ] {
            assert_eq!(subject_scope(subject), Scope::Applied, "{subject}");
        }
        for subject in [
            "rift.events.players",
            "rift.events.control",
            "rift.events.lifecycle",
            "rift.events.plugins",
            crate::extensions::players::EVENTS,
            "rift.events",
        ] {
            assert_eq!(subject_scope(subject), Scope::Origin, "{subject}");
        }
        for subject in ["rift.node.lobby.cmd", "rift.node.lobby.state"] {
            assert_eq!(subject_scope(subject), Scope::Owner, "{subject}");
        }
        for subject in [
            "game.score",
            "_INBOX.abc",
            "_INBOX.eu-1.abc",
            "rift.perms.update.0123456789abcdef0123456789abcdef",
            wire::RESYNC,
            "rift.controlled",
            "rift.eventsx",
        ] {
            assert_eq!(subject_scope(subject), Scope::Cluster, "{subject}");
        }
    }

    #[test]
    fn only_patterns_that_can_cross_peers_are_interest() {
        assert!(pattern_may_federate("game.>", false));
        assert!(pattern_may_federate(">", false));
        assert!(pattern_may_federate("rift.>", false));
        assert!(pattern_may_federate("rift.perms.update.>", false));
        assert!(!pattern_may_federate("rift.control.*", true));
        assert!(!pattern_may_federate("rift.node.*.state", false));
        assert!(!pattern_may_federate(wire::UPDATE, true));
        assert!(!pattern_may_federate("rift.events.instances", true));
        assert!(!pattern_may_federate("rift.events.>", false));
        assert!(pattern_may_federate("rift.events.>", true));
        assert!(pattern_may_federate("rift.events.players", true));
        assert!(!pattern_may_federate("rift.events.players", false));
    }
}
