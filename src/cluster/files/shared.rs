//! Peer and shared sections of the configuration (docs/clustering.md, "One
//! configuration").
//!
//! Every peer runs the same `rift.lua`, and `rift.peer` lets one file serve
//! every host. Branching on it is allowed only in the **peer sections**:
//! listeners (with the metrics, admin and status listeners), routes (with
//! `on_route`), limits (with the rate limits and the PROXY protocol), health
//! checks, the server list (with its cache and `on_status`), the tab list
//! and the web listener. Every other section is **shared**: it writes
//! durable state or drives leader tasks, so every peer must evaluate it the
//! same way.
//!
//! A peer hashes the shared sections of its *evaluated* configuration: a
//! SHA-256 of a stable rendering of each section's parsed structures (their
//! `Debug` form, which uses ordered maps throughout), with the directory of
//! `rift.lua` and its file name replaced by placeholders so peers installed
//! under different paths agree. The hash of all sections is what handshakes
//! carry and peers report; the per-section hashes name what differs.
use crate::config::Config;
use aws_lc_rs::digest;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

/// The sections that may depend on `rift.peer`, as the documentation names
/// them (each stands for the related settings listed in the module
/// documentation).
pub const PEER_SECTIONS: &[&str] = &[
    "listeners",
    "routes",
    "limits",
    "health_check",
    "server_list",
    "tab_list",
    "web",
];

/// The shared sections of one evaluated configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shared {
    /// SHA-256 over every section's hash, in section order.
    pub hash: String,
    /// Section name → SHA-256 of its rendering.
    pub sections: BTreeMap<String, String>,
}

/// The shared sections of `config`, evaluated from the file at `path` (used
/// to normalize paths below its directory; `None` leaves them as they are).
pub fn of_config(config: &Config, path: Option<&Path>) -> Shared {
    let normalizer = Normalizer::new(path);
    let mut sections = BTreeMap::new();
    for (name, text) in renderings(config) {
        let text = normalizer.apply(text);
        sections.insert(name.to_owned(), sha256(text.as_bytes()));
    }
    let mut context = digest::Context::new(&digest::SHA256);
    for (name, hash) in &sections {
        context.update(name.as_bytes());
        context.update(b"=");
        context.update(hash.as_bytes());
        context.update(b"\n");
    }
    Shared {
        hash: crate::cluster::hex(context.finish().as_ref()),
        sections,
    }
}

/// Evaluate `source` as the file at `path` the way peer `peer` would
/// (`rift.peer` set to its name) and hash its shared sections.
pub fn evaluate(source: &str, path: &Path, peer: &str) -> Result<Shared, String> {
    let config =
        crate::cluster::settings::with_peer(peer, || Config::from_lua_at_unrestored(source, path))
            .map_err(|error| error.to_string())?;
    Ok(of_config(&config, Some(path)))
}

/// The names of the sections whose hashes differ between `a` and `b`.
pub fn differing(a: &Shared, b: &Shared) -> Vec<String> {
    let names: BTreeSet<&String> = a.sections.keys().chain(b.sections.keys()).collect();
    names
        .into_iter()
        .filter(|name| a.sections.get(*name) != b.sections.get(*name))
        .cloned()
        .collect()
}

/// Evaluate `source` once per member name and require that the shared
/// sections come out the same. Returns the evaluation (every member's is
/// the same) and each member's hash; the error names the peers and the
/// sections that differ.
pub fn validate(
    source: &str,
    path: &Path,
    members: &BTreeSet<String>,
) -> Result<(Shared, BTreeMap<String, String>), String> {
    let mut evaluations: BTreeMap<String, Shared> = BTreeMap::new();
    for member in members {
        let shared = evaluate(source, path, member).map_err(|error| {
            format!("evaluating the configuration as peer {member} failed: {error}")
        })?;
        evaluations.insert(member.clone(), shared);
    }
    let Some((reference_name, reference)) = evaluations.iter().next() else {
        return Err("the cluster has no members to evaluate the configuration for".into());
    };
    let problems: Vec<String> = evaluations
        .iter()
        .filter(|(_, shared)| shared.hash != reference.hash)
        .map(|(name, shared)| {
            format!(
                "peer {name} differs from peer {reference_name} in {}",
                differing(reference, shared).join(", ")
            )
        })
        .collect();
    if !problems.is_empty() {
        return Err(format!(
            "shared configuration sections must not depend on rift.peer (only {} and their related settings may): {}",
            PEER_SECTIONS.join(", "),
            problems.join("; ")
        ));
    }
    let hashes = evaluations
        .iter()
        .map(|(name, shared)| (name.clone(), shared.hash.clone()))
        .collect();
    Ok((reference.clone(), hashes))
}

/// Lowercase hex SHA-256 of `bytes`.
pub(crate) fn sha256(bytes: &[u8]) -> String {
    crate::cluster::hex(digest::digest(&digest::SHA256, bytes).as_ref())
}

/// Every shared section with its rendering. `instances` and
/// `moved_servers` are left out: they come from the instance database at
/// runtime, not from `rift.lua`.
fn renderings(config: &Config) -> Vec<(&'static str, String)> {
    vec![
        ("backends", format!("{:?}", config.backends)),
        ("managed_servers", format!("{:?}", config.managed_servers)),
        ("templates", format!("{:?}", config.templates)),
        ("template_sources", format!("{:?}", config.template_sources)),
        ("service_groups", format!("{:?}", config.service_groups)),
        ("nodes", format!("{:?}", config.nodes)),
        (
            "instance_database",
            format!("{:?}", config.instance_database),
        ),
        ("database", format!("{:?}", config.database)),
        ("software_cache", format!("{:?}", config.software_cache)),
        ("template_storage", format!("{:?}", config.template_storage)),
        ("authentication", format!("{:?}", config.authentication)),
        ("forwarding", format!("{:?}", config.forwarding)),
        ("extensions", format!("{:?}", config.extensions)),
        ("plugins", format!("{:?}", config.plugins)),
        ("on_http", format!("{:?}", config.on_http)),
        ("on_message", format!("{:?}", config.on_message)),
        ("messaging", format!("{:?}", config.messaging)),
        ("fallbacks", format!("{:?}", config.fallbacks)),
        ("network", format!("{:?}", config.network)),
        ("permissions", format!("{:?}", config.permissions)),
        (
            "maintenance",
            format!(
                "{:?} {:?}",
                config.maintenance, config.maintenance_permission
            ),
        ),
        ("draining", format!("{:?}", config.draining)),
        ("translation", format!("{:?}", config.translation)),
        ("shutdown_timeout", format!("{:?}", config.shutdown_timeout)),
        ("text", format!("{:?}", config.text)),
        ("players", format!("{:?}", config.record_ip)),
        ("cluster", format!("{:?}", config.cluster)),
        ("backups", format!("{:?}", config.backups)),
    ]
}

/// Replaces what differs between hosts that run the same configuration:
/// the directory of `rift.lua` (paths below it) and the name it was loaded
/// under.
struct Normalizer {
    replacements: Vec<(String, String)>,
}

impl Normalizer {
    fn new(path: Option<&Path>) -> Self {
        let mut replacements = Vec::new();
        if let Some(path) = path {
            // The entry file's name as the script source records it.
            let name = format!("{:?}", path.display().to_string());
            replacements.push((format!("name: {name}"), "name: \"<config>\"".to_owned()));
            // Its directory as the script source resolves it.
            let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
            if let Some(root) = resolved.parent().filter(|root| root.is_absolute()) {
                let debug = format!("{root:?}");
                if let Some(inner) = debug
                    .strip_prefix('"')
                    .and_then(|debug| debug.strip_suffix('"'))
                    .filter(|inner| !inner.is_empty())
                {
                    replacements.push((format!("\"{inner}"), "\"<root>".to_owned()));
                }
            }
        }
        Self { replacements }
    }

    fn apply(&self, mut text: String) -> String {
        for (from, to) in &self.replacements {
            if text.contains(from.as_str()) {
                text = text.replace(from.as_str(), to);
            }
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "rift.config.listeners.public = '127.0.0.1:25565'
rift.config.backends.lobby = '127.0.0.1:25566'
rift.config.backends.other = '127.0.0.1:25567'
rift.config.routes.public = 'lobby'
";

    fn members() -> BTreeSet<String> {
        ["p0", "p1", "p2"].into_iter().map(str::to_owned).collect()
    }

    #[test]
    fn peer_sections_may_differ_and_shared_ones_may_not() {
        let directory =
            std::env::temp_dir().join(format!("rift-shared-validate-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("rift.lua");
        let routes =
            format!("{BASE}if rift.peer == 'p2' then rift.config.routes.public = 'other' end\n");
        let (shared, hashes) = validate(&routes, &path, &members()).unwrap();
        assert_eq!(hashes.len(), 3);
        assert!(hashes.values().all(|hash| *hash == shared.hash));
        let backends = format!(
            "{BASE}if rift.peer == 'p2' then rift.config.backends.extra = '127.0.0.1:1' end\n"
        );
        let error = validate(&backends, &path, &members()).unwrap_err();
        assert!(
            error.contains("peer p2") && error.contains("backends"),
            "{error}"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn paths_below_the_configuration_directory_do_not_count() {
        let one = std::env::temp_dir().join(format!("rift-shared-a-{}", std::process::id()));
        let two = std::env::temp_dir().join(format!("rift-shared-b-{}", std::process::id()));
        std::fs::create_dir_all(&one).unwrap();
        std::fs::create_dir_all(&two).unwrap();
        let a = evaluate(BASE, &one.join("rift.lua"), "p0").unwrap();
        let b = evaluate(BASE, &two.join("rift.lua"), "p0").unwrap();
        assert_eq!(a, b, "{:?}", differing(&a, &b));
        let other = evaluate(
            &format!("{BASE}rift.config.backends.more = '127.0.0.1:2'\n"),
            &two.join("rift.lua"),
            "p0",
        )
        .unwrap();
        assert_eq!(differing(&a, &other), ["backends"]);
        std::fs::remove_dir_all(one).unwrap();
        std::fs::remove_dir_all(two).unwrap();
    }
}
