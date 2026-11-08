//! `permissions`: limits of the permission store, the custom context keys
//! backends may report for their players, and the expiry sweep interval.
use super::*;
use rift_permissions::{Group, Limits, NodeKind, context};

/// Most keys in `permissions.contexts.external`.
const MAX_EXTERNAL_CONTEXTS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionsConfig {
    /// Store limits; the defaults keep a compiled subject under 64 KiB.
    pub limits: Limits,
    /// Custom context keys a backend may report for a player on it.
    pub external_contexts: BTreeSet<String>,
    /// How often expired nodes are deleted. Checks treat them as absent at
    /// once; the sweep only removes the rows and tells subscribers.
    pub sweep_interval: Duration,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            external_contexts: BTreeSet::new(),
            sweep_interval: Duration::from_secs(60),
        }
    }
}

pub(super) fn parse(root: &Table) -> Result<PermissionsConfig, String> {
    let Some(values) = options(
        root,
        "permissions",
        &["limits", "contexts", "sweep_interval_ms"],
    )?
    else {
        return Ok(PermissionsConfig::default());
    };
    let defaults = Limits::default();
    let limits = match options(
        &values,
        "limits",
        &[
            "groups",
            "group_nodes",
            "user_nodes",
            "parents",
            "contexts",
            "tracks",
            "compiled_entries",
        ],
    )? {
        None => defaults,
        Some(t) => {
            let limit = |key: &str, default: usize, max: usize| {
                integer(&t, "permissions.limits", key, default, max)
            };
            Limits {
                groups: limit("groups", defaults.groups, 65_536)?,
                group_nodes: limit("group_nodes", defaults.group_nodes, 65_536)?,
                user_nodes: limit("user_nodes", defaults.user_nodes, 65_536)?,
                parents: limit("parents", defaults.parents, 1024)?,
                contexts: limit("contexts", defaults.contexts, 256)?,
                tracks: limit("tracks", defaults.tracks, 4096)?,
                compiled_entries: limit("compiled_entries", defaults.compiled_entries, 65_536)?,
                batch: defaults.batch,
            }
        }
    };
    let mut external_contexts = BTreeSet::new();
    if let Some(contexts) = options(&values, "contexts", &["external"])? {
        let path = "permissions.contexts.external";
        match contexts
            .raw_get::<Value>("external")
            .map_err(|e| e.to_string())?
        {
            Value::Nil => {}
            value => {
                for key in string_list(value, path, MAX_EXTERNAL_CONTEXTS)? {
                    if context::RESERVED.contains(&key.as_str()) {
                        return Err(format!(
                            "{path}: {key:?} is filled by Rift and the adapters themselves"
                        ));
                    }
                    if rift_permissions::ContextSet::of(&key, "x").is_err()
                        || key != key.to_ascii_lowercase()
                    {
                        return Err(format!(
                            "{path}: {key:?} must be 1 to 64 bytes of [a-z0-9_.:-]"
                        ));
                    }
                    if !external_contexts.insert(key.clone()) {
                        return Err(format!("{path}: {key:?} is listed twice"));
                    }
                }
            }
        }
    }
    let sweep_interval = Duration::from_millis(integer(
        &values,
        "permissions",
        "sweep_interval_ms",
        60_000,
        86_400_000,
    )? as u64);
    if sweep_interval < Duration::from_secs(1) {
        return Err("permissions.sweep_interval_ms: expected at least 1000".into());
    }
    Ok(PermissionsConfig {
        limits,
        external_contexts,
        sweep_interval,
    })
}

/// `extensions.groups` must fit the store limits it is mirrored into.
pub(super) fn validate_groups(
    groups: &BTreeMap<String, Group>,
    limits: &Limits,
) -> Result<(), String> {
    if groups.len() > limits.groups {
        return Err(format!(
            "extensions.groups: at most {} groups (permissions.limits.groups)",
            limits.groups
        ));
    }
    for (name, group) in groups {
        if group.nodes.len() > limits.group_nodes {
            return Err(format!(
                "extensions.groups.{name}: at most {} nodes (permissions.limits.group_nodes)",
                limits.group_nodes
            ));
        }
        let parents = group
            .nodes
            .iter()
            .filter(|node| node.kind() == NodeKind::Inherit)
            .count();
        if parents > limits.parents {
            return Err(format!(
                "extensions.groups.{name}.inherits: at most {} groups (permissions.limits.parents)",
                limits.parents
            ));
        }
    }
    Ok(())
}
