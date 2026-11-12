//! `rift check --strict`: what plain `rift check` cannot see from the
//! configuration text alone. Secrets in the environment, the instance database
//! and extension store, port binds, and whether a running Rift could reload to
//! this configuration.
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
    pub notes: Vec<String>,
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
/// One line per managed server or service group that asks for isolation:
/// what this host's kernel gives it, and the fallback where it gives
/// nothing. Servers on nodes are confined by their agent's kernel.
pub fn isolation(config: &Config) -> Vec<String> {
    use rift::config::IsolationMode;
    #[cfg(target_os = "linux")]
    let available = rift::managed::landlock::abi().map(rift::managed::landlock::describe);
    #[cfg(not(target_os = "linux"))]
    let available: Result<String, String> = Err("Landlock exists only on Linux".into());
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
        let mode = server.isolation.mode;
        if mode == IsolationMode::Off {
            continue;
        }
        let line = if remote {
            format!(
                "isolation {path}: {} on each node's kernel, applied by its agent",
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
        .any(|group| group.memory_mb.is_some())
    {
        lines.push(
            "memory_mb sets -Xmx and placement only; Rift does not enforce a process memory limit"
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
    let advertised = config
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
            "plugin {} {}: mode persistent, engine {engine}, trusted; restart {}{}; capabilities {}",
            spec.name,
            manifest.version,
            manifest.restart.name(),
            if manifest.hot_reload { ", hot reload" } else { "" },
            manifest.describe_capabilities()
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
    if !config.plugins.is_empty() && !config.authentication.online_mode {
        warnings.push(
            "persistent plugins get no player hooks (login, commands, join, ...) without authentication.online_mode; a plugin that registers one fails to start".into(),
        );
    }
    (lines, warnings)
}

/// Run every strict check and collect the results. Nothing is created or
/// changed except a probe file that is removed at once and the compiled
/// WebAssembly plugins in `local/plugincache` (and that cache's key), which
/// startup then loads.
pub fn strict(config: &Config, path: &Path) -> Report {
    let mut report = Report::default();
    environment(config, &mut report);
    report.errors.extend(
        plugins(config)
            .1
            .into_iter()
            .filter(|warning| warning.contains("is not available")),
    );
    #[cfg(feature = "wasm")]
    for spec in &config.plugins {
        if spec.manifest.engine == Some(rift::plugins::manifest::EngineKind::Wasm) {
            match rift::plugins::wasm::precompile(spec) {
                Ok(note) => report.notes.push(note),
                Err(error) => report.errors.push(format!("plugin {}: {error}", spec.name)),
            }
        }
    }
    let running = match Store::check(&config.instance_database, config) {
        Ok(DatabaseCheck::Missing) => {
            report.notes.push(format!(
                "instance database {} does not exist yet; startup will create it",
                config.instance_database.display()
            ));
            false
        }
        Ok(DatabaseCheck::Ready { instances }) => {
            report.notes.push(format!(
                "instance database {} opens ({instances} registered instances fit the configuration)",
                config.instance_database.display()
            ));
            false
        }
        Ok(DatabaseCheck::InUse) => {
            report.notes.push(format!(
                "instance database {} is locked by a running Rift; port binds were not checked",
                config.instance_database.display()
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
    report
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
        Some(&config.instance_database),
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
        // Integration headers are checked below with their own rules.
        if field.starts_with("extensions.") {
            continue;
        }
        let minimum = if field == "forwarding.secret_env" {
            1
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
