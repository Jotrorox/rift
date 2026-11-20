//! `rift.config.cluster`: the settings every peer of a cluster shares. A
//! peer's own settings (name, addresses, certificates) are in `cluster.toml`
//! (see `crate::cluster::settings`); this section is inert without it.
use super::*;

pub const DEFAULT_PEER_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_PARTITION_GRACE: Duration = Duration::from_secs(60);
pub const DEFAULT_PARTITION_MESSAGE: &str =
    "This proxy lost contact with the rest of the network. Reconnect in a moment.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterConfig {
    /// The leader expires a peer it has not heard from for this long; a peer
    /// that has not heard from the leader for half of it stops admitting.
    pub peer_timeout: Duration,
    /// How long players on a cut-off peer stay connected.
    pub partition_grace: Duration,
    /// Network-wide player cap, enforced when claims apply.
    pub max_players: Option<u64>,
    /// Shown to players a cut-off peer refuses or disconnects.
    pub partition_message: String,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            peer_timeout: DEFAULT_PEER_TIMEOUT,
            partition_grace: DEFAULT_PARTITION_GRACE,
            max_players: None,
            partition_message: DEFAULT_PARTITION_MESSAGE.to_owned(),
        }
    }
}

pub(super) fn parse(root: &Table) -> Result<Option<ClusterConfig>, String> {
    let Some(t) = options(
        root,
        "cluster",
        &[
            "peer_timeout_ms",
            "partition_grace_ms",
            "max_players",
            "partition_message",
        ],
    )?
    else {
        return Ok(None);
    };
    let peer_timeout = Duration::from_millis(integer(
        &t,
        "cluster",
        "peer_timeout_ms",
        DEFAULT_PEER_TIMEOUT.as_millis() as usize,
        3_600_000,
    )? as u64);
    if peer_timeout < Duration::from_secs(2) {
        return Err("cluster.peer_timeout_ms: at least 2000, so an election fits inside it".into());
    }
    let partition_grace = Duration::from_millis(integer(
        &t,
        "cluster",
        "partition_grace_ms",
        DEFAULT_PARTITION_GRACE.as_millis() as usize,
        86_400_000,
    )? as u64);
    let max_players = match t.get::<Value>("max_players").map_err(|e| e.to_string())? {
        Value::Nil => None,
        _ => Some(integer(&t, "cluster", "max_players", 1, 100_000_000)? as u64),
    };
    let partition_message = match t
        .get::<Value>("partition_message")
        .map_err(|e| e.to_string())?
    {
        Value::Nil => DEFAULT_PARTITION_MESSAGE.to_owned(),
        value => {
            let message = string(value, "cluster.partition_message")?;
            if message.trim().is_empty() || message.len() > 1024 {
                return Err("cluster.partition_message: expected 1..=1024 bytes of text".into());
            }
            message
        }
    };
    Ok(Some(ClusterConfig {
        peer_timeout,
        partition_grace,
        max_players,
        partition_message,
    }))
}
