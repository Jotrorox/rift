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
/// WebAssembly plugins in `local/plugincache`, which startup then loads.
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
    if let Some(forwarding) = &config.forwarding {
        report.check(secret("forwarding.secret_env", &forwarding.secret_env, 1));
    }
    if let Some(admin) = &config.admin {
        report.check(secret("admin.token_env", &admin.token_env, 32));
    }
    if let Some(messaging) = &config.messaging {
        for principal in &messaging.principals {
            report.check(secret(
                &format!("messaging.principals.{}.token_env", principal.name),
                &principal.token_env,
                32,
            ));
        }
    }
    for (name, node) in &config.nodes {
        report.check(secret(
            &format!("nodes.{name}.token_env"),
            &node.token_env,
            32,
        ));
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
