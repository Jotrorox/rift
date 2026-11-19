//! `rift check --strict`: what plain `rift check` cannot see from the
//! configuration text alone. Secrets in the environment, the instance database
//! and extension store, port binds, whether a running Rift could reload to
//! this configuration, and whether its node agents can run what it asks.
use rift::{
    config::Config,
    extensions::ExtensionScript,
    instance_store::{DatabaseCheck, Store},
    preflight::{self, Transport},
};
use std::{net::SocketAddr, path::Path};

#[derive(Default)]
pub struct Report {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub notes: Vec<String>,
    /// A running Rift holds the instance database.
    pub running: bool,
}

impl Report {
    fn check(&mut self, result: Result<(), String>) {
        if let Err(message) = result {
            self.errors.push(message);
        }
    }
}

/// Every plugin with its execution mode, so the trust difference is visible:
/// the configuration script runs sandboxed, persistent plugins run trusted
/// with the listed capabilities. Plugins are never initialized here. Returns
/// lines to print and warnings (engines this build lacks).
/// One line per managed server or service group that asks for isolation or
/// a memory limit: what this host's kernel gives it, and the fallback where
/// it gives nothing. Servers on nodes are confined by their agent's kernel.
pub fn isolation(config: &Config) -> Vec<String> {
    use rift::config::IsolationMode;
    #[cfg(target_os = "linux")]
    let available = rift::managed::landlock::abi().map(rift::managed::landlock::describe);
    #[cfg(not(target_os = "linux"))]
    let available: Result<String, String> = Err("Landlock exists only on Linux".into());
    #[cfg(target_os = "linux")]
    let groups = rift::managed::cgroup::describe();
    #[cfg(not(target_os = "linux"))]
    let groups: Result<String, String> = Err("cgroups exist only on Linux".into());
    let mut lines = Vec::new();
    let definitions = config
        .managed_servers
        .iter()
        .filter(|(name, _)| !config.instances.contains_key(*name))
        .map(|(name, server)| {
            (
                format!("managed_servers.{name}"),
                server,
                server.node.is_some(),
            )
        })
        .chain(config.service_groups.iter().map(|(name, group)| {
            (
                format!("service_groups.{name}"),
                &group.server,
                group.nodes.is_some(),
            )
        }));
    for (path, server, remote) in definitions {
        if let Some(limit) = server.memory_limit {
            let megabytes = limit.megabytes;
            lines.push(if remote {
                let fallback = if limit.required {
                    "Rift refuses nodes whose agent cannot enforce it"
                } else {
                    "an agent that cannot enforce it starts the server without"
                };
                format!("memory_limit {path}: {megabytes} MiB in each node's cgroup, enforced by its agent; {fallback}")
            } else {
                match (&groups, limit.required) {
                    (Ok(description), _) => {
                        format!("memory_limit {path}: {megabytes} MiB, {description}")
                    }
                    (Err(reason), true) => format!(
                        "memory_limit {path}: unavailable ({reason}); the required limit refuses to start it"
                    ),
                    (Err(reason), false) => format!(
                        "memory_limit {path}: unavailable ({reason}); starts without a limit"
                    ),
                }
            });
        }
        let mode = server.isolation.mode;
        if mode == IsolationMode::Off {
            continue;
        }
        let line = if remote {
            let fallback = if mode == IsolationMode::Required {
                "Rift refuses nodes whose agent cannot apply it"
            } else {
                "an agent that cannot apply it starts the server unconfined"
            };
            format!(
                "isolation {path}: {} on each node's kernel, applied by its agent; {fallback}",
                mode.as_str()
            )
        } else {
            match (&available, mode) {
                (Ok(description), _) => format!("isolation {path}: {description}"),
                (Err(reason), IsolationMode::Required) => format!(
                    "isolation {path}: unavailable ({reason}); isolation = \"required\" refuses to start it"
                ),
                (Err(reason), _) => format!(
                    "isolation {path}: unavailable ({reason}); runs unconfined as Rift's user and can read local/instance-key and other instances' tokens"
                ),
            }
        };
        lines.push(line);
    }
    if config
        .service_groups
        .values()
        .any(|group| group.memory_mb.is_some() && group.server.memory_limit.is_none())
    {
        lines.push(
            "memory_mb sets -Xmx and placement only; set memory_limit to enforce a process memory limit"
                .into(),
        );
    }
    lines
}

/// The warning for a configuration that does not authenticate players, which
/// is the default: anyone who can reach a listener can join under any name,
/// including an operator's.
pub fn offline_mode(config: &Config) -> Option<&'static str> {
    (!config.authentication.online_mode).then_some(
        "players are not authenticated (authentication.online_mode is off, the default): anyone who can reach a listener can join under any name, including an operator's. Set rift.config.authentication = { online_mode = true }, or allow only trusted clients to connect",
    )
}

/// Warnings about instance bus logins (see [`rift::instance_login`]):
/// a group whose template ships RiftPerms but that issues no login (its
/// instances would need a principal and token each, or fail to start the
/// plugin), and groups on nodes that would be told a loopback bus address.
pub fn instance_logins(config: &Config) -> Vec<String> {
    let mut warnings = Vec::new();
    for (name, group) in &config.service_groups {
        let Some(template) = group
            .template
            .as_ref()
            .and_then(|t| config.templates.get(t))
        else {
            continue;
        };
        if group.server.messaging.is_none()
            && let Some(jar) = riftperms_jar(template)
        {
            warnings.push(format!(
                "service_groups.{name}: template ships RiftPerms ({}) but the group has no messaging block, so its instances get no bus login; add messaging = {{ preset = \"backend\" }} to give each instance its own",
                jar.display()
            ));
        }
    }
    // A cluster peer's instances are told its own bus address, which comes
    // from its cluster.toml (`bus_address`, else its advertise host).
    let advertised = rift::cluster::settings::local_peer().is_some()
        || config
            .messaging
            .as_ref()
            .is_some_and(|messaging| messaging.advertise.is_some());
    let loopback = config
        .messaging
        .as_ref()
        .and_then(|messaging| messaging.listen)
        .is_some_and(|listen| listen.ip().is_loopback() || listen.ip().is_unspecified());
    if !advertised && loopback {
        let remote = config
            .service_groups
            .iter()
            .filter(|(_, group)| group.nodes.is_some() && group.server.messaging.is_some())
            .map(|(name, _)| format!("service_groups.{name}"))
            .chain(
                config
                    .managed_servers
                    .iter()
                    .filter(|(_, server)| server.node.is_some() && server.messaging.is_some())
                    .map(|(name, _)| format!("managed_servers.{name}")),
            );
        for path in remote {
            warnings.push(format!(
                "{path}: runs on nodes with a bus login, but without messaging.advertise its instances are told 127.0.0.1; set messaging.advertise to an address the nodes reach"
            ));
        }
    }
    warnings
}

/// A RiftPerms jar among a template's plugins or in a config layer's
/// `plugins/` folder.
fn riftperms_jar(template: &rift::config::ServerTemplate) -> Option<std::path::PathBuf> {
    let riftperms = |path: &std::path::Path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                let name = name.to_ascii_lowercase();
                name.starts_with("riftperms") && name.ends_with(".jar")
            })
    };
    template
        .plugins
        .iter()
        .find(|path| riftperms(path))
        .cloned()
        .or_else(|| {
            template.configs.iter().find_map(|layer| {
                std::fs::read_dir(layer.join("plugins"))
                    .ok()?
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .find(|path| riftperms(path))
            })
        })
}

pub fn plugins(config: &Config) -> (Vec<String>, Vec<String>) {
    let mut lines = Vec::new();
    let mut warnings = Vec::new();
    let script = rift::plugins::script::ScriptPlugin::new(config);
    if !script.is_empty() {
        let hooks: Vec<_> = script.hooks().map(|hook| hook.name()).collect();
        let mut line = format!(
            "plugin (configuration script): mode script, sandboxed; hooks {}",
            hooks.join(", ")
        );
        if !script.commands().is_empty() {
            let commands: Vec<_> = script.commands().iter().map(String::as_str).collect();
            line.push_str(&format!("; commands {}", commands.join(", ")));
        }
        lines.push(line);
    }
    if let Some(extensions) = &config.extensions {
        let hooks: Vec<_> = extensions.decision_hooks().collect();
        if !hooks.is_empty() {
            lines.push(format!(
                "plugin (bus responders): mode remote; hooks {}",
                hooks.join(", ")
            ));
        }
    }
    let engines = rift::plugins::worker::Engines::builtin();
    for spec in &config.plugins {
        let manifest = &spec.manifest;
        let engine = manifest.engine.map_or("none", |engine| engine.name());
        lines.push(format!(
            "plugin {} {}: mode persistent, engine {engine}, trusted; restart {}{}; capabilities {}{}",
            spec.name,
            manifest.version,
            manifest.restart.name(),
            if manifest.hot_reload { ", hot reload" } else { "" },
            manifest.describe_capabilities(),
            config
                .text
                .namespace(&spec.name)
                .map(message_counts)
                .unwrap_or_default()
        ));
        if let Some(kind) = manifest.engine
            && !engines.available(kind)
        {
            warnings.push(format!(
                "plugin {}: {}; startup and reload will refuse it",
                spec.name,
                rift::plugins::worker::Engines::unavailable(kind)
            ));
        }
    }
    // Script-mode plugins run inside the configuration script; only their
    // catalogues are listed on their own.
    for (name, messages) in config.text.namespaces() {
        if !messages.is_empty() && !config.plugins.iter().any(|spec| spec.name == name) {
            lines.push(format!(
                "plugin {name}: mode script, sandboxed{}",
                message_counts(messages)
            ));
        }
    }
    // A cluster.toml beside the configuration (which sets the peer name
    // before the check) or a cluster section makes this a cluster peer.
    let clustered = rift::cluster::settings::local_peer().is_some() || config.cluster.is_some();
    for spec in &config.plugins {
        if let Some(warning) = rift::plugins::cluster_warning(&spec.manifest, clustered) {
            warnings.push(format!("plugin {}: {warning}", spec.name));
        }
    }
    if !config.plugins.is_empty() && !config.authentication.online_mode {
        warnings.push(
            "persistent plugins get no player hooks (login, commands, join, ...) without authentication.online_mode; a plugin that registers one fails to start".into(),
        );
    }
    (lines, warnings)
}

/// `; messages 3, languages 1` for a plugin with a `messages.toml`.
fn message_counts(messages: &rift::text::messages::Messages) -> String {
    if messages.is_empty() {
        return String::new();
    }
    format!(
        "; messages {}, languages {}",
        messages.len(),
        messages.languages().count()
    )
}

/// A cluster peer's own settings (`cluster.toml` beside `path`): the token
/// variable is set and long enough, and the certificate, key and CA load.
/// Nothing to report on a Rift without `cluster.toml`.
pub fn cluster(config: &Config, path: &Path, report: &mut Report) {
    let settings = match rift::cluster::Settings::load_beside(path) {
        Ok(Some(settings)) => settings,
        Ok(None) => return,
        Err(error) => {
            report.errors.push(error.to_string());
            return;
        }
    };
    if config.database.is_some() {
        report.errors.push(
            "cluster.toml: a cluster replicates SQLite; database.backend = \"postgres\" or \"mariadb\" cannot be combined with it".into(),
        );
    }
    match settings.token() {
        Ok(_) => report
            .notes
            .push(format!("cluster: {} is set", settings.token_env)),
        Err(error) => report.errors.push(error.to_string()),
    }
    if settings.public_address.is_none() && !settings.witness {
        report.warnings.push(format!(
            "{}: no public_address: a peer that drains cannot send 1.20.5+ players to {}, and when no peer has one they get a reconnect message instead",
            rift::cluster::Settings::path_beside(path).display(),
            settings.peer
        ));
    }
    match rift::cluster::quic::check(&settings) {
        Ok(()) => report.notes.push(format!(
            "cluster: peer {} loads {}, {} and {}",
            settings.peer,
            settings.certificate.display(),
            settings.private_key.display(),
            settings.ca.display()
        )),
        Err(error) => report.errors.push(error.to_string()),
    }
    // Shared sections as every member evaluates them, against the last
    // deployment and the peers' reports in this peer's replica.
    match std::fs::read_to_string(path) {
        Ok(source) => {
            let found = rift::cluster::files::check(
                path,
                &source,
                &config.instance_database,
                &settings.peer,
            );
            report.notes.extend(found.notes);
            report.warnings.extend(found.warnings);
            report.errors.extend(found.errors);
        }
        Err(error) => report.errors.push(format!("{}: {error}", path.display())),
    }
}

/// Strict checks of a cluster peer against the running cluster
/// (docs/clustering.md, "Getting there from one Rift"): a key-value store
/// in a file of its own, and, through the running peer's admin listener,
/// peers that are unreachable or refused (another cluster token, other
/// shared secrets or an older version than the cluster requires), behind
/// the leader, on another version, or with node agents that cannot be
/// fenced.
pub async fn cluster_peers(config: &Config, path: &Path, report: &mut Report) {
    if !matches!(rift::cluster::Settings::load_beside(path), Ok(Some(_))) {
        return;
    }
    if let Some(store) = rift::cluster::attach::file_store(config) {
        report.errors.push(format!(
            "extensions.storage.path ({}): a cluster cannot replicate a store in a file of its own; set extensions.storage.database = true (`rift cluster init` copies its entries into the instance database)",
            store.display()
        ));
    }
    let skipped = |reason: &str| format!("cluster peers were not checked: {reason}");
    if !report.running {
        report.notes.push(skipped("Rift is not running"));
        return;
    }
    let Some(admin) = &config.admin else {
        report
            .notes
            .push(skipped("admin is not configured; see GET /api/cluster"));
        return;
    };
    let secret = match crate::admin::token(admin) {
        Ok(secret) => secret,
        Err(error) => {
            report.notes.push(skipped(&error.to_string()));
            return;
        }
    };
    let listen = admin.listen;
    let ask = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let secret = secret.clone();
        async move {
            crate::admin::query(listen, &secret, &args, std::time::Duration::from_secs(5)).await
        }
    };
    let status = match ask(&["cluster"]).await {
        Ok(status) => status,
        Err(error) => {
            report.notes.push(skipped(&format!("admin: {error}")));
            return;
        }
    };
    let own = env!("CARGO_PKG_VERSION");
    if status["joined"] != true {
        report.errors.push(format!(
            "cluster: peer {} is not in a cluster yet; add it on a member with rift admin cluster add",
            status["peer"].as_str().unwrap_or("?")
        ));
    }
    if status["diverged"] == true {
        report
            .errors
            .push("cluster: this peer's replica diverged; it reinstalls a snapshot".into());
    }
    if status["leader"].is_null() {
        report
            .errors
            .push("cluster: there is no leader: no logins, writes or scaling".into());
    }
    // Replication lag is known on the leader only: a follower asks it.
    let leader_view = if status["is_leader"] == true {
        Some(status.clone())
    } else if status["leader"].is_null() {
        None
    } else {
        ask(&["cluster", "status", "--leader"]).await.ok()
    };
    let lags: std::collections::BTreeMap<String, u64> = leader_view
        .iter()
        .flat_map(|view| view["peers"].as_array().into_iter().flatten())
        .filter_map(|peer| {
            let name = peer["name"].as_str()?.to_owned();
            Some((name, peer["lag"].as_u64()?))
        })
        .collect();
    if leader_view.is_none() && !status["leader"].is_null() {
        report.notes.push(
            "cluster: the leader's status was not available; how far peers are behind was not checked".into(),
        );
    }
    for peer in status["peers"].as_array().into_iter().flatten() {
        let name = peer["name"].as_str().unwrap_or("?");
        let lag = lags.get(name).copied().filter(|lag| *lag > 1000);
        if peer["this_peer"] == true {
            if let Some(lag) = lag {
                report.warnings.push(format!(
                    "cluster: this peer ({name}) is {lag} entries behind the leader"
                ));
            }
            continue;
        }
        if peer["expired"] == true {
            report.warnings.push(format!(
                "cluster: peer {name} is expired; it rejoins when it runs again, or remove it with rift admin cluster remove {name}"
            ));
            continue;
        }
        if peer["up"] != true {
            report.errors.push(format!(
                "cluster: peer {name} is unreachable or refused this peer (another cluster token, other forwarding secret or node tokens, or another cluster); see its log"
            ));
            continue;
        }
        if let Some(version) = peer["version"].as_str()
            && !version.is_empty()
            && version != own
        {
            report.warnings.push(format!(
                "cluster: peer {name} runs rift {version}, this one {own}; finish the rolling upgrade"
            ));
        }
        if let Some(lag) = lag {
            report.warnings.push(format!(
                "cluster: peer {name} is {lag} entries behind the leader"
            ));
        }
    }
    // The leader sends node agents control commands only once fenced.
    if let Ok(data) = ask(&["nodes"]).await {
        for node in data["nodes"].as_array().into_iter().flatten() {
            let fenced = node["features"].as_array().is_some_and(|features| {
                features
                    .iter()
                    .any(|feature| feature == rift::node::protocol::feature::FENCE)
            });
            if node["version"].is_string() && !fenced {
                report.errors.push(format!(
                    "node {}: its agent cannot be fenced, so the cluster sends it no control commands; upgrade rift on that node to {own}",
                    node["name"].as_str().unwrap_or("?")
                ));
            }
        }
    }
}

/// Run every strict check and collect the results. Nothing is created or
/// changed except a probe file that is removed at once and the compiled
/// WebAssembly plugins in `local/plugincache` (and that cache's key), which
/// startup then loads.
pub fn strict(config: &Config, path: &Path) -> Report {
    let mut report = Report::default();
    environment(config, &mut report);
    // Paths are relative to the working directory, as at startup.
    if let Some((certificate, private_key)) = config.web_tls_files() {
        match crate::web::tls_acceptor(certificate, private_key) {
            Ok(_) => report.notes.push(format!(
                "web.tls: {} and {} load",
                certificate.display(),
                private_key.display()
            )),
            Err(error) => report.errors.push(error.to_string()),
        }
    }
    report.errors.extend(
        plugins(config)
            .1
            .into_iter()
            .filter(|warning| warning.contains("is not available")),
    );
    // Replacements for a plugin that is not enabled: warnings in plain
    // `rift check` and at load.
    report.errors.extend(config.text.warnings().iter().cloned());
    #[cfg(feature = "wasm")]
    for spec in &config.plugins {
        if spec.manifest.engine == Some(rift::plugins::manifest::EngineKind::Wasm) {
            match rift::plugins::wasm::precompile(spec) {
                Ok(note) => report.notes.push(note),
                Err(error) => report.errors.push(format!("plugin {}: {error}", spec.name)),
            }
        }
    }
    let location = config.database_location();
    let described = location.describe();
    let running = match Store::check(&location, config) {
        Ok(DatabaseCheck::Missing) => {
            report.notes.push(match &location {
                rift::database::Location::Sqlite(_) => format!(
                    "instance database {described} does not exist yet; startup will create it"
                ),
                rift::database::Location::Server(_) => format!(
                    "instance database {described} connects and has no Rift tables yet; startup will create them"
                ),
            });
            false
        }
        Ok(DatabaseCheck::Ready { instances }) => {
            report.notes.push(format!(
                "instance database {described} opens ({instances} registered instances fit the configuration)"
            ));
            false
        }
        Ok(DatabaseCheck::InUse) => {
            report.notes.push(format!(
                "instance database {described} is locked by a running Rift; port binds were not checked"
            ));
            true
        }
        Err(error) => {
            report.errors.push(error.to_string());
            false
        }
    };
    if let Some(extensions) = &config.extensions {
        match extensions.check_storage(running) {
            Ok(Some(note)) => report.notes.push(note),
            Ok(None) => {}
            Err(error) => report.errors.push(format!("extensions.storage: {error}")),
        }
        command_permissions(config, extensions, &mut report);
    }
    if !running {
        for (field, address, transport) in addresses(config) {
            report.check(
                preflight::check_bind(address, transport)
                    .map_err(|error| format!("{field}: cannot bind {address}: {error}")),
            );
        }
    }
    reload(config, path, running, &mut report);
    report.running = running;
    report
}

/// Ask a running Rift over `admin` which agents its nodes run, and compare
/// them with what this configuration's servers need from them. Rift refuses
/// to start a server on an agent that would silently drop a setting it
/// requires, so such a pair is an error here.
pub async fn agents(config: &Config, report: &mut Report) {
    if config.nodes.is_empty() {
        return;
    }
    let skipped = |reason: &str| format!("node agents were not checked: {reason}");
    if !report.running {
        report.notes.push(skipped("Rift is not running"));
        return;
    }
    let Some(admin) = &config.admin else {
        report
            .notes
            .push(skipped("admin is not configured; see GET /api/nodes"));
        return;
    };
    let secret = match crate::admin::token(admin) {
        Ok(secret) => secret,
        Err(error) => {
            report.notes.push(skipped(&error.to_string()));
            return;
        }
    };
    let data = match crate::admin::query(
        admin.listen,
        &secret,
        &["nodes".to_owned()],
        std::time::Duration::from_secs(5),
    )
    .await
    {
        Ok(data) => data,
        Err(error) => {
            report.notes.push(skipped(&format!("admin: {error}")));
            return;
        }
    };
    let own = env!("CARGO_PKG_VERSION");
    for node in data["nodes"].as_array().into_iter().flatten() {
        let (Some(name), Some(version)) = (node["name"].as_str(), node["version"].as_str()) else {
            continue;
        };
        let features: Vec<&str> = node["features"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|feature| feature.as_str())
            .collect();
        let missing: Vec<&str> = rift::node::protocol::FEATURES
            .iter()
            .copied()
            .filter(|feature| !features.contains(feature))
            .collect();
        let refused: Vec<String> = hosted(config, name)
            .filter_map(|(path, server)| {
                let lacking: Vec<_> = rift::node::server_requirements(server)
                    .into_iter()
                    .filter(|requirement| {
                        requirement.mandatory && missing.contains(&requirement.feature)
                    })
                    .map(|requirement| requirement.feature)
                    .collect();
                (!lacking.is_empty()).then(|| format!("{path} ({})", lacking.join(", ")))
            })
            .collect();
        if !refused.is_empty() {
            report.errors.push(format!(
                "node {name}: agent {version} cannot apply settings of {}, so Rift refuses to start them there; upgrade rift on {name} to {own}",
                refused.join(", ")
            ));
        } else if !missing.is_empty() {
            report.warnings.push(format!(
                "node {name}: agent {version} lacks {}; servers that need them are refused there; upgrade rift on {name} to {own}",
                missing.join(", ")
            ));
        } else if version != own {
            report.warnings.push(format!(
                "node {name}: agent {version} differs from this rift {own}; finish the upgrade so both run the same release, controller first"
            ));
        }
    }
}

/// The server definitions that may run on `node`: named servers placed
/// there and service groups whose selector matches it.
fn hosted<'a>(
    config: &'a Config,
    node: &'a str,
) -> impl Iterator<Item = (String, &'a rift::config::ManagedServer)> + 'a {
    let definition = config.nodes.get(node);
    config
        .managed_servers
        .iter()
        .filter(move |(name, server)| {
            !config.instances.contains_key(*name) && server.node.as_deref() == Some(node)
        })
        .map(|(name, server)| (format!("managed_servers.{name}"), server))
        .chain(
            config
                .service_groups
                .iter()
                .filter(move |(_, group)| {
                    group
                        .nodes
                        .as_ref()
                        .zip(definition)
                        .is_some_and(|(selector, definition)| selector.matches(node, definition))
                })
                .map(|(name, group)| (format!("service_groups.{name}"), &group.server)),
        )
}

/// Extension commands nobody can run yet: their permission is neither
/// registered by a backend nor granted anywhere. A warning, since grants are
/// often added after the configuration.
fn command_permissions(config: &Config, extensions: &ExtensionScript, report: &mut Report) {
    let commands = extensions.command_permissions();
    if commands.is_empty() {
        return;
    }
    match rift::permissions::ungranted_command_permissions(
        Some(&config.database_location()),
        extensions.groups(),
        &commands,
    ) {
        Ok(missing) => {
            for (command, permission) in missing {
                report.notes.push(format!(
                    "warning: extensions.commands.{command}: permission {permission} is neither registered by a backend nor granted anywhere, so nobody can run /{command} yet"
                ));
            }
        }
        Err(error) => report.notes.push(format!(
            "warning: command permissions were not checked: {error}"
        )),
    }
}

/// A secret read from `variable` must be set and contain `min..=1024` bytes
/// without control characters, as startup requires. Never prints the value.
fn secret(field: &str, variable: &str, min: usize) -> Result<(), String> {
    let value = std::env::var(variable).map_err(|_| format!("{field}: {variable} is not set"))?;
    if !(min..=1024).contains(&value.len()) || value.chars().any(char::is_control) {
        return Err(format!(
            "{field}: {variable} must contain {min}..=1024 bytes without control characters"
        ));
    }
    Ok(())
}

fn environment(config: &Config, report: &mut Report) {
    for (field, variable) in config.secret_variables() {
        // Integration headers are checked below with their own rules; the
        // database URL is checked by connecting to it.
        if field.starts_with("extensions.") || field.starts_with("database.") {
            continue;
        }
        let minimum = if field == "forwarding.secret_env" {
            1
        } else if field.starts_with("web.") {
            // What the web service accepts as a token.
            16
        } else {
            32
        };
        report.check(secret(&field, &variable, minimum));
    }
    if let Some(extensions) = &config.extensions {
        report.check(extensions.check_environment());
    }
}

/// Every address Rift binds at startup.
fn addresses(config: &Config) -> Vec<(String, SocketAddr, Transport)> {
    let mut addresses: Vec<_> = config
        .listeners
        .iter()
        .map(|(name, address)| (format!("listeners.{name}"), *address, Transport::Tcp))
        .collect();
    if let Some(admin) = &config.admin {
        addresses.push(("admin.listen".into(), admin.listen, Transport::Tcp));
    }
    if let Some(web) = &config.web {
        addresses.push(("web.listen".into(), web.listen, Transport::Tcp));
    }
    if let Some(status) = &config.status {
        addresses.push(("status.listen".into(), status.listen, Transport::Tcp));
    }
    if let Some(metrics) = config.metrics {
        addresses.push(("metrics".into(), metrics, Transport::Tcp));
    }
    if let Some(listen) = config.messaging.as_ref().and_then(|m| m.listen) {
        addresses.push(("messaging.listen".into(), listen, Transport::Udp));
    }
    addresses
}

/// Compare with the last deployment in the operator records, which is what a
/// running Rift has active. Incompatibility is an error only while Rift runs;
/// otherwise the next start applies everything.
fn reload(config: &Config, path: &Path, running: bool, report: &mut Report) {
    let directory = config
        .web
        .as_ref()
        .and_then(|web| web.records_directory.clone())
        .unwrap_or_else(|| {
            path.with_file_name(format!(
                "{}.operators",
                path.file_name().unwrap_or_default().to_string_lossy()
            ))
        });
    let history = directory.join("deployments.json");
    let Ok(data) = std::fs::read(&history) else {
        report
            .notes
            .push("no deployment history found; reload compatibility was not checked".into());
        return;
    };
    let source = serde_json::from_slice::<Vec<serde_json::Value>>(&data)
        .ok()
        .and_then(|values| values.last()?["source"].as_str().map(str::to_owned));
    let Some(source) = source else {
        report.errors.push(format!(
            "{}: unreadable deployment history",
            history.display()
        ));
        return;
    };
    // Modules are read from disk now, so this approximates the deployed
    // configuration when modules changed since then.
    let previous = match Config::from_lua_at_unrestored(&source, path) {
        Ok(previous) => previous,
        Err(error) => {
            report.notes.push(format!(
                "the last deployed configuration no longer evaluates ({error}); reload compatibility was not checked"
            ));
            return;
        }
    };
    let result = crate::control::live_compatible(config, &previous)
        .and_then(|()| {
            ExtensionScript::live_compatible(
                previous.extensions.as_ref(),
                config.extensions.as_ref(),
            )
        })
        .and_then(|()| {
            if config.web.as_ref().is_none_or(|web| {
                web.records_directory
                    == previous
                        .web
                        .as_ref()
                        .and_then(|web| web.records_directory.clone())
            }) {
                Ok(())
            } else {
                Err("web.records_directory changes require a restart".into())
            }
        });
    match (result, running) {
        (Ok(()), _) => report
            .notes
            .push("a reload from the last deployed configuration would be accepted".into()),
        (Err(reason), true) => report.errors.push(format!(
            "reload from the last deployed configuration would be rejected: {reason}"
        )),
        (Err(reason), false) => report.notes.push(format!(
            "a reload from the last deployed configuration would be rejected ({reason}); a restart applies it"
        )),
    }
}
