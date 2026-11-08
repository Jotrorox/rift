//! Script mode: the sandboxed configuration script (`rift.lua` and the
//! modules and script-mode plugins it loads) at position 0 of every chain.
//!
//! Its callbacks keep running through their existing executors, unchanged:
//! `Extensions` for player hooks (per-session callback slot, 50 ms budget),
//! `Router` for `on_route`/`on_status`, `HttpScript` and `MessageHandler`.
//! Each re-evaluates the pinned source snapshot in a fresh VM. The host asks
//! position 0 first by awaiting that executor's future, and this descriptor
//! tells it which events and commands the script owns.
use super::api::Hook;
use crate::config::Config;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptPlugin {
    hooks: BTreeSet<Hook>,
    commands: BTreeSet<String>,
}

impl ScriptPlugin {
    pub fn new(config: &Config) -> Self {
        let mut hooks = BTreeSet::new();
        for (present, hook) in [
            (config.on_route.is_some(), Hook::Route),
            (config.on_status.is_some(), Hook::Status),
            (config.on_http.is_some(), Hook::Http),
            (config.on_message.is_some(), Hook::Message),
        ] {
            if present {
                hooks.insert(hook);
            }
        }
        let mut commands = BTreeSet::new();
        if let Some(extensions) = &config.extensions {
            hooks.extend(extensions.hook_names().filter_map(Hook::parse));
            if extensions.has_jobs() {
                hooks.insert(Hook::Timer);
            }
            commands.extend(extensions.command_names().map(str::to_owned));
            if !commands.is_empty() {
                hooks.insert(Hook::Command);
                hooks.insert(Hook::Suggest);
            }
        }
        Self { hooks, commands }
    }

    pub fn handles(&self, hook: Hook) -> bool {
        self.hooks.contains(&hook)
    }

    pub fn hooks(&self) -> impl Iterator<Item = Hook> + '_ {
        self.hooks.iter().copied()
    }

    pub fn commands(&self) -> &BTreeSet<String> {
        &self.commands
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }
}
