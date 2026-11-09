//! Pumpkin's registered permissions, announced to the proxy with
//! `rift.perms.register` so their descriptions and defaults show up in the
//! dashboard and tab completion.
//!
//! Pumpkin's `PermissionRegistry` cannot be enumerated (its map is private),
//! so the adapter discovers nodes instead: it probes the command permissions
//! of every registered command at load, and looks up every node that passes
//! through a permission check afterwards. Pumpkin checks each command's
//! permission when it sends a joining player the command tree, so the first
//! join announces nearly everything.

use std::collections::{BTreeMap, HashSet};

use pumpkin_util::permission::{Permission, PermissionDefault};
use rift_permissions::{node::normalize_node, wire};

/// Most entries in one `rift.perms.register` message.
pub const MAX_REGISTERED: usize = 4096;
/// Most distinct nodes remembered as looked up, bounding memory when a
/// plugin checks generated node names.
const MAX_SEEN: usize = 16 * 1024;

/// Pumpkin's default for a node nobody has set, as the wire spells it.
pub fn wire_default(default: &PermissionDefault) -> wire::PermissionDefault {
    match default {
        PermissionDefault::Allow => wire::PermissionDefault::True,
        PermissionDefault::Deny => wire::PermissionDefault::False,
        PermissionDefault::Op(level) => wire::PermissionDefault::Op(Some(*level as u8)),
    }
}

/// Longest description the proxy accepts, in bytes.
pub const MAX_DESCRIPTION: usize = 1024;
/// Longest plugin label the proxy accepts, in bytes.
const MAX_LABEL: usize = 96;

/// A Pumpkin permission as a registration, or `None` when the node is not
/// valid in Rift's syntax. `plugin` is the node's namespace (`minecraft` in
/// `minecraft:command.gamemode`). Pumpkin's `children` have no equivalent and
/// are not sent.
///
/// The proxy rejects a whole batch over one bad entry, so the description is
/// cleaned to its limits (control characters become spaces, at most
/// [`MAX_DESCRIPTION`] bytes) and a namespace it would refuse as a label is
/// left out.
pub fn registration(permission: &Permission) -> Option<wire::RegisteredPermission> {
    let node = normalize_node(&permission.node)
        .inspect_err(|error| {
            log::debug!(
                "rift-permissions: not registering {}: {error}",
                permission.node
            );
        })
        .ok()?;
    let plugin = node
        .split_once(':')
        .map(|(namespace, _)| namespace)
        .filter(|namespace| valid_label(namespace))
        .map(str::to_owned);
    let description = clean_description(&permission.description);
    Some(wire::RegisteredPermission {
        node,
        description: (!description.is_empty()).then_some(description),
        default: wire_default(&permission.default),
        plugin,
    })
}

/// A description the proxy accepts: control characters replaced by spaces,
/// trimmed, cut to [`MAX_DESCRIPTION`] bytes on a character boundary.
pub fn clean_description(description: &str) -> String {
    let cleaned: String = description
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut cleaned = cleaned.trim();
    if cleaned.len() > MAX_DESCRIPTION {
        let mut end = MAX_DESCRIPTION;
        while !cleaned.is_char_boundary(end) {
            end -= 1;
        }
        cleaned = cleaned[..end].trim_end();
    }
    cleaned.to_owned()
}

/// Whether the proxy accepts `name` as a plugin label.
fn valid_label(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_LABEL
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b':'))
}

/// Candidate permission nodes for a command name, following Pumpkin's
/// naming (`minecraft:command.say`, `pumpkin:command.plugins`).
pub fn command_candidates(command: &str) -> [String; 2] {
    [
        format!("minecraft:command.{command}"),
        format!("pumpkin:command.{command}"),
    ]
}

/// The registrations discovered so far and whether the proxy has them.
#[derive(Default)]
pub struct Registrations {
    seen: HashSet<String>,
    known: BTreeMap<String, wire::RegisteredPermission>,
    dirty: bool,
}

impl Registrations {
    /// Whether `node` was looked up already; records it otherwise.
    pub fn seen(&mut self, node: &str) -> bool {
        if self.seen.contains(node) {
            return true;
        }
        if self.seen.len() < MAX_SEEN {
            self.seen.insert(node.to_owned());
        }
        false
    }

    /// Whether `node` was looked up already, without recording it.
    pub fn was_seen(&self, node: &str) -> bool {
        self.seen.contains(node) || self.seen.len() >= MAX_SEEN
    }

    /// Add a registration; true when it is new or changed.
    pub fn add(&mut self, registration: wire::RegisteredPermission) -> bool {
        if self.known.get(&registration.node) == Some(&registration) {
            return false;
        }
        if self.known.len() >= MAX_REGISTERED && !self.known.contains_key(&registration.node) {
            log::warn!(
                "rift-permissions: more than {MAX_REGISTERED} registered permissions; not announcing {}",
                registration.node
            );
            return false;
        }
        self.known.insert(registration.node.clone(), registration);
        self.dirty = true;
        true
    }

    /// Send everything again, as after a reconnect.
    pub fn mark_dirty(&mut self) {
        self.dirty = !self.known.is_empty();
    }

    /// The full list to send when something changed since the last call.
    /// Always the whole set, sorted by node, so the proxy ends up with the
    /// same rows whether it merges or replaces a backend's registrations.
    pub fn take_batch(&mut self) -> Option<Vec<wire::RegisteredPermission>> {
        if !std::mem::take(&mut self.dirty) {
            return None;
        }
        Some(self.known.values().cloned().collect())
    }

    pub fn len(&self) -> usize {
        self.known.len()
    }

    pub fn is_empty(&self) -> bool {
        self.known.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_util::PermissionLvl;

    #[test]
    fn pumpkin_permissions_map_to_registrations() {
        let gamemode = Permission::new(
            "minecraft:command.gamemode",
            "Change game modes",
            PermissionDefault::Op(PermissionLvl::Two),
        );
        let registered = registration(&gamemode).unwrap();
        assert_eq!(
            serde_json::to_value(&registered).unwrap(),
            serde_json::json!({"node": "minecraft:command.gamemode", "description": "Change game modes",
                               "default": "op:2", "plugin": "minecraft"})
        );
        let plain =
            registration(&Permission::new("Homes.Use", " ", PermissionDefault::Allow)).unwrap();
        assert_eq!(plain.node, "homes.use");
        assert_eq!(plain.description, None);
        assert_eq!(plain.default, wire::PermissionDefault::True);
        assert_eq!(plain.plugin, None);
        let deny = registration(&Permission::new("x:y", "", PermissionDefault::Deny)).unwrap();
        assert_eq!(deny.default, wire::PermissionDefault::False);
        assert!(registration(&Permission::new("bad node", "", PermissionDefault::Deny)).is_none());
        assert!(
            registration(&Permission::new("minecraft:*", "", PermissionDefault::Deny)).is_none()
        );
    }

    #[test]
    fn registrations_fit_the_proxys_limits() {
        let long = "é".repeat(MAX_DESCRIPTION);
        let cleaned = clean_description(&long);
        assert!(cleaned.len() <= MAX_DESCRIPTION);
        assert!(cleaned.chars().all(|c| c == 'é'));
        assert_eq!(clean_description(" two\nlines\t "), "two lines");
        let namespace = "n".repeat(MAX_LABEL + 1);
        let odd = registration(&Permission::new(
            &format!("{namespace}:fly"),
            "Fly\n",
            PermissionDefault::Allow,
        ))
        .unwrap();
        assert_eq!(odd.plugin, None);
        assert_eq!(odd.description.as_deref(), Some("Fly"));
    }

    #[test]
    fn command_candidates_follow_pumpkins_naming() {
        assert_eq!(
            command_candidates("say"),
            ["minecraft:command.say", "pumpkin:command.say"]
        );
    }

    #[test]
    fn batches_carry_the_whole_set_once_per_change() {
        let mut registrations = Registrations::default();
        assert!(!registrations.seen("a:b"));
        assert!(registrations.seen("a:b"));
        assert!(registrations.was_seen("a:b"));
        assert!(!registrations.was_seen("a:c"));
        assert_eq!(registrations.take_batch(), None);
        registrations.mark_dirty();
        assert_eq!(registrations.take_batch(), None);

        let b = registration(&Permission::new("p:b", "B", PermissionDefault::Allow)).unwrap();
        let a = registration(&Permission::new("p:a", "A", PermissionDefault::Deny)).unwrap();
        assert!(registrations.add(b.clone()));
        assert!(registrations.add(a.clone()));
        assert!(!registrations.add(a.clone()));
        assert_eq!(registrations.take_batch(), Some(vec![a.clone(), b.clone()]));
        assert_eq!(registrations.take_batch(), None);
        registrations.mark_dirty();
        assert_eq!(registrations.take_batch().map(|batch| batch.len()), Some(2));
        assert_eq!(registrations.len(), 2);
        assert!(!registrations.is_empty());
    }

    #[test]
    fn the_set_is_bounded() {
        let mut registrations = Registrations::default();
        for n in 0..MAX_REGISTERED + 10 {
            let permission = Permission::new(&format!("p:n{n}"), "", PermissionDefault::Allow);
            registrations.add(registration(&permission).unwrap());
        }
        assert_eq!(registrations.len(), MAX_REGISTERED);
        for n in 0..MAX_SEEN + 10 {
            registrations.seen(&format!("p:s{n}"));
        }
        assert!(registrations.was_seen("p:never-checked"));
    }
}
