//! `plugin.toml`: what an installed plugin is, how it runs and what it may
//! touch. Parsing is strict: unknown keys, keys that do not apply to the
//! selected mode or engine, and out-of-range values are errors.
use super::api::{API_VERSION, Hook};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    time::Duration,
};

pub const FILE: &str = "plugin.toml";
/// Manifests are small; a larger file is almost certainly a mistake.
pub const MAX_BYTES: usize = 64 * 1024;

/// Defaults from the plugin contract. Observers have no deadline: they are
/// queued and never awaited.
const INIT: Duration = Duration::from_secs(30);
const RELOAD: Duration = Duration::from_secs(5);
const SHUTDOWN: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Trusted, one OS thread per plugin, installed on disk.
    Persistent,
    /// The folder's `init.lua` loads inside the configuration sandbox.
    Script,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EngineKind {
    Lua,
    Wasm,
}

impl EngineKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Lua => "lua",
            Self::Wasm => "wasm",
        }
    }
}

/// What the host does when a plugin misses a deadline or cannot answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Refuse: the same outcome as an error from the configuration script.
    Deny,
    /// Skip this plugin and ask the next one.
    Continue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    Never,
    /// Restart after a runtime failure (trap, error outside a handler, thread
    /// death), and keep retrying a restart whose `init` fails.
    OnFailure,
    /// Like `OnFailure`, and also retry a first `init` that failed after a
    /// reload, which `OnFailure` leaves failed until the next change. A
    /// failed `init` at startup fails startup either way.
    Always,
}

impl Restart {
    pub fn name(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::OnFailure => "on-failure",
            Self::Always => "always",
        }
    }
}

/// A filesystem grant. Engines enforce these: WASI preopens, Lua `io`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Filesystem {
    /// The plugin's private `data/` directory inside its folder.
    Data,
    /// The configuration directory (the folder holding `rift.lua`).
    Host,
    Path(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub filesystem: Vec<Filesystem>,
    pub network: bool,
    /// `None` inherits Rift's whole environment.
    pub env: Option<Vec<String>>,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            filesystem: vec![Filesystem::Data],
            network: true,
            env: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LuaOptions {
    pub ffi: bool,
    pub native_modules: bool,
    /// 0 means unlimited.
    pub memory_limit_mb: u32,
}

/// `[wasm]`: limits for one WebAssembly instance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WasmOptions {
    /// Linear memory per instance in MiB; 0 means only WebAssembly's
    /// own 4 GiB bound.
    pub memory_limit_mb: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub api: String,
    pub mode: Mode,
    /// `None` only for `mode = "script"`.
    pub engine: Option<EngineKind>,
    /// Relative to the plugin folder; `None` only for `mode = "script"`.
    pub entry: Option<PathBuf>,
    pub hot_reload: bool,
    deadlines: BTreeMap<String, Duration>,
    on_timeout: Option<Policy>,
    on_failure: Policy,
    pub restart: Restart,
    pub capabilities: Capabilities,
    pub lua: Option<LuaOptions>,
    /// `Some` exactly for `engine = "wasm"`.
    pub wasm: Option<WasmOptions>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    name: String,
    version: String,
    api: String,
    mode: Option<String>,
    engine: Option<String>,
    entry: Option<String>,
    hot_reload: Option<bool>,
    deadlines: Option<BTreeMap<String, i64>>,
    failure: Option<RawFailure>,
    capabilities: Option<RawCapabilities>,
    lua: Option<RawLua>,
    wasm: Option<RawWasm>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFailure {
    on_timeout: Option<String>,
    on_failure: Option<String>,
    restart: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCapabilities {
    filesystem: Option<Vec<String>>,
    network: Option<bool>,
    env: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWasm {
    memory_limit_mb: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLua {
    ffi: Option<bool>,
    native_modules: Option<bool>,
    memory_limit_mb: Option<i64>,
}

/// Plugin and folder names: what `rift.plugin` accepts.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn policy(value: Option<String>, field: &str) -> Result<Option<Policy>, String> {
    match value.as_deref() {
        None => Ok(None),
        Some("deny") => Ok(Some(Policy::Deny)),
        Some("continue") => Ok(Some(Policy::Continue)),
        Some(_) => Err(format!("failure.{field} must be \"deny\" or \"continue\"")),
    }
}

/// Upper bounds per deadline key, in milliseconds.
fn deadline_limit(key: &str) -> Option<u64> {
    match key {
        "init" => Some(300_000),
        "reload" | "shutdown" | "http" => Some(60_000),
        _ => match Hook::parse(key) {
            Some(hook) if !hook.is_observer() => Some(10_000),
            _ => None,
        },
    }
}

impl Manifest {
    /// Parse `plugin.toml` text for the folder `plugins/<folder>/`.
    pub fn parse(text: &str, folder: &str) -> Result<Self, String> {
        if text.len() > MAX_BYTES {
            return Err(format!("{FILE} exceeds {} KiB", MAX_BYTES / 1024));
        }
        let raw: Raw = toml::from_str(text).map_err(|error| {
            // toml's messages span several lines with a source excerpt.
            let message = error.message().trim().to_owned();
            match error.span() {
                Some(span) => {
                    let line = text[..span.start.min(text.len())].lines().count().max(1);
                    format!("line {line}: {message}")
                }
                None => message,
            }
        })?;
        if !valid_name(&raw.name) {
            return Err("name must be 1..=128 letters, digits, '_' or '-'".into());
        }
        if raw.name != folder {
            return Err(format!(
                "name {:?} must match its folder plugins/{folder}",
                raw.name
            ));
        }
        if raw.version.is_empty()
            || raw.version.len() > 64
            || !raw
                .version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
        {
            return Err(
                "version must be 1..=64 letters, digits, '.', '_', '+' or '-' (for example \"0.3.1\")"
                    .into(),
            );
        }
        if raw.api != API_VERSION {
            return Err(format!(
                "api {:?} is not supported; this Rift implements rift:plugin {API_VERSION}",
                raw.api
            ));
        }
        let mode = match raw.mode.as_deref() {
            None | Some("persistent") => Mode::Persistent,
            Some("script") => Mode::Script,
            Some(_) => return Err("mode must be \"persistent\" or \"script\"".into()),
        };
        if mode == Mode::Script {
            for (present, key) in [
                (raw.engine.is_some(), "engine"),
                (raw.entry.is_some(), "entry"),
                (raw.hot_reload.is_some(), "hot_reload"),
                (raw.deadlines.is_some(), "deadlines"),
                (raw.failure.is_some(), "failure"),
                (raw.capabilities.is_some(), "capabilities"),
                (raw.lua.is_some(), "lua"),
                (raw.wasm.is_some(), "wasm"),
            ] {
                if present {
                    return Err(format!(
                        "{key} does not apply to mode = \"script\": the folder's init.lua runs in the configuration sandbox"
                    ));
                }
            }
            return Ok(Self {
                name: raw.name,
                version: raw.version,
                api: raw.api,
                mode,
                engine: None,
                entry: None,
                hot_reload: false,
                deadlines: BTreeMap::new(),
                on_timeout: None,
                on_failure: Policy::Deny,
                restart: Restart::Never,
                capabilities: Capabilities::default(),
                lua: None,
                wasm: None,
            });
        }
        let engine = match raw.engine.as_deref() {
            Some("lua") => EngineKind::Lua,
            Some("wasm") => EngineKind::Wasm,
            Some(_) => return Err("engine must be \"lua\" or \"wasm\"".into()),
            None => return Err("engine is required: \"lua\" or \"wasm\"".into()),
        };
        let entry = match (raw.entry, engine) {
            (Some(entry), _) => entry,
            (None, EngineKind::Lua) => "init.lua".into(),
            (None, EngineKind::Wasm) => {
                return Err("entry is required for engine = \"wasm\"".into());
            }
        };
        let entry = relative_entry(&entry)?;
        let mut deadlines = BTreeMap::new();
        for (key, value) in raw.deadlines.unwrap_or_default() {
            let Some(limit) = deadline_limit(&key) else {
                return Err(format!(
                    "deadlines.{key}: not a hook with a deadline (observers are never awaited)"
                ));
            };
            if !(1..=limit as i64).contains(&value) {
                return Err(format!("deadlines.{key} must be 1..={limit} ms"));
            }
            deadlines.insert(key, Duration::from_millis(value as u64));
        }
        let failure = raw.failure.unwrap_or(RawFailure {
            on_timeout: None,
            on_failure: None,
            restart: None,
        });
        let on_timeout = policy(failure.on_timeout, "on_timeout")?;
        let on_failure = policy(failure.on_failure, "on_failure")?.unwrap_or(Policy::Deny);
        let restart = match failure.restart.as_deref() {
            None | Some("on-failure") => Restart::OnFailure,
            Some("never") => Restart::Never,
            Some("always") => Restart::Always,
            Some(_) => {
                return Err(
                    "failure.restart must be \"never\", \"on-failure\" or \"always\"".into(),
                );
            }
        };
        let capabilities = match raw.capabilities {
            None => Capabilities::default(),
            Some(raw) => capabilities(raw)?,
        };
        let lua = match (raw.lua, engine) {
            (Some(_), EngineKind::Wasm) => {
                return Err("[lua] applies only to engine = \"lua\"".into());
            }
            (raw, EngineKind::Lua) => {
                let raw = raw.unwrap_or(RawLua {
                    ffi: None,
                    native_modules: None,
                    memory_limit_mb: None,
                });
                let memory_limit_mb = raw.memory_limit_mb.unwrap_or(0);
                if !(0..=65_536).contains(&memory_limit_mb) {
                    return Err("lua.memory_limit_mb must be 0..=65536 (0 is unlimited)".into());
                }
                Some(LuaOptions {
                    ffi: raw.ffi.unwrap_or(false),
                    native_modules: raw.native_modules.unwrap_or(false),
                    memory_limit_mb: memory_limit_mb as u32,
                })
            }
            (None, EngineKind::Wasm) => None,
        };
        let wasm = match (raw.wasm, engine) {
            (Some(_), EngineKind::Lua) => {
                return Err("[wasm] applies only to engine = \"wasm\"".into());
            }
            (_, EngineKind::Lua) => None,
            (raw, EngineKind::Wasm) => {
                let memory_limit_mb = raw.and_then(|raw| raw.memory_limit_mb).unwrap_or(0);
                if !(0..=4096).contains(&memory_limit_mb) {
                    return Err("wasm.memory_limit_mb must be 0..=4096 (0 is unlimited)".into());
                }
                Some(WasmOptions {
                    memory_limit_mb: memory_limit_mb as u32,
                })
            }
        };
        Ok(Self {
            name: raw.name,
            version: raw.version,
            api: raw.api,
            mode,
            engine: Some(engine),
            entry: Some(entry),
            hot_reload: raw.hot_reload.unwrap_or(false),
            deadlines,
            on_timeout,
            on_failure,
            restart,
            capabilities,
            lua,
            wasm,
        })
    }

    /// The deadline for one awaited hook; `None` for observers.
    pub fn deadline(&self, hook: Hook) -> Option<Duration> {
        if hook.is_observer() {
            return None;
        }
        if let Some(deadline) = self.deadlines.get(hook.name()) {
            return Some(*deadline);
        }
        Some(Duration::from_millis(match hook {
            Hook::Route | Hook::Suggest | Hook::Status => 50,
            Hook::Login | Hook::InitialServer | Hook::Command => 1000,
            Hook::BeforeTransfer => 300,
            Hook::Http => 5000,
            _ => unreachable!("observers have no deadline"),
        }))
    }

    pub fn init_deadline(&self) -> Duration {
        self.deadlines.get("init").copied().unwrap_or(INIT)
    }

    pub fn reload_deadline(&self) -> Duration {
        self.deadlines.get("reload").copied().unwrap_or(RELOAD)
    }

    pub fn shutdown_grace(&self) -> Duration {
        self.deadlines.get("shutdown").copied().unwrap_or(SHUTDOWN)
    }

    /// A missed deadline: the manifest's `on_timeout`, or the hook's default
    /// (continue for status, suggest and http, deny otherwise).
    pub fn on_timeout(&self, hook: Hook) -> Policy {
        self.on_timeout.unwrap_or(match hook {
            Hook::Status | Hook::Suggest | Hook::Http => Policy::Continue,
            _ => Policy::Deny,
        })
    }

    /// A failed plugin, a handler error or an invalid reply.
    pub fn on_failure(&self) -> Policy {
        self.on_failure
    }

    /// Effective grants, for `rift check` and the status API.
    pub fn describe_capabilities(&self) -> String {
        let filesystem: Vec<String> = self
            .capabilities
            .filesystem
            .iter()
            .map(|grant| match grant {
                Filesystem::Data => "data".to_owned(),
                Filesystem::Host => "host".to_owned(),
                Filesystem::Path(path) => path.display().to_string(),
            })
            .collect();
        let env = match &self.capabilities.env {
            None => "all".to_owned(),
            Some(names) if names.is_empty() => "none".to_owned(),
            Some(names) => names.join(","),
        };
        let mut text = format!(
            "filesystem={} network={} env={env}",
            if filesystem.is_empty() {
                "none".to_owned()
            } else {
                filesystem.join(",")
            },
            if self.capabilities.network {
                "yes"
            } else {
                "no"
            },
        );
        if let Some(lua) = &self.lua {
            text.push_str(&format!(
                " ffi={} native_modules={} memory_limit_mb={}",
                lua.ffi, lua.native_modules, lua.memory_limit_mb
            ));
        }
        if let Some(wasm) = self.wasm.as_ref().filter(|wasm| wasm.memory_limit_mb > 0) {
            text.push_str(&format!(" memory_limit_mb={}", wasm.memory_limit_mb));
        }
        text
    }
}

/// A path inside the plugin folder: relative, without `..`, `.` or empty parts.
fn relative_entry(entry: &str) -> Result<PathBuf, String> {
    let path = Path::new(entry);
    if entry.is_empty()
        || entry.len() > 256
        || entry.contains(['\0', '\\'])
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(
            "entry must be a relative path of at most 256 bytes inside the plugin folder".into(),
        );
    }
    Ok(path.to_owned())
}

fn capabilities(raw: RawCapabilities) -> Result<Capabilities, String> {
    let mut grants = BTreeSet::new();
    for value in raw.filesystem.unwrap_or_else(|| vec!["data".into()]) {
        let grant = match value.as_str() {
            "data" => Filesystem::Data,
            "host" => Filesystem::Host,
            path if Path::new(path).is_absolute() && path.len() <= 4096 && !path.contains('\0') => {
                Filesystem::Path(PathBuf::from(path))
            }
            _ => {
                return Err(
                    "capabilities.filesystem entries must be \"data\", \"host\" or absolute paths"
                        .into(),
                );
            }
        };
        if !grants.insert(grant) {
            return Err(format!("capabilities.filesystem lists {value:?} twice"));
        }
    }
    let env = match raw.env {
        None => None,
        Some(names) => {
            let mut seen = BTreeSet::new();
            for name in &names {
                let mut bytes = name.bytes();
                let valid = name.len() <= 256
                    && bytes
                        .next()
                        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                    && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_');
                if !valid {
                    return Err(format!(
                        "capabilities.env: {name:?} is not an environment variable name"
                    ));
                }
                if !seen.insert(name) {
                    return Err(format!("capabilities.env lists {name:?} twice"));
                }
            }
            Some(names)
        }
    };
    Ok(Capabilities {
        filesystem: grants.into_iter().collect(),
        network: raw.network.unwrap_or(true),
        env,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "name = \"relaxoban\"\nversion = \"0.3.1\"\napi = \"0.1\"\nengine = \"wasm\"\nentry = \"relaxoban.wasm\"\n";

    #[test]
    fn defaults_follow_the_contract() {
        let manifest = Manifest::parse(MINIMAL, "relaxoban").unwrap();
        assert_eq!(manifest.mode, Mode::Persistent);
        assert_eq!(manifest.engine, Some(EngineKind::Wasm));
        assert_eq!(manifest.init_deadline(), Duration::from_secs(30));
        assert_eq!(manifest.reload_deadline(), Duration::from_secs(5));
        assert_eq!(manifest.shutdown_grace(), Duration::from_secs(10));
        for (hook, ms) in [
            (Hook::Route, 50),
            (Hook::Login, 1000),
            (Hook::InitialServer, 1000),
            (Hook::BeforeTransfer, 300),
            (Hook::Command, 1000),
            (Hook::Suggest, 50),
            (Hook::Status, 50),
            (Hook::Http, 5000),
        ] {
            assert_eq!(manifest.deadline(hook), Some(Duration::from_millis(ms)));
        }
        assert_eq!(manifest.deadline(Hook::Join), None);
        assert_eq!(manifest.on_timeout(Hook::Login), Policy::Deny);
        assert_eq!(manifest.on_timeout(Hook::Status), Policy::Continue);
        assert_eq!(manifest.on_timeout(Hook::Http), Policy::Continue);
        assert_eq!(manifest.on_failure(), Policy::Deny);
        assert_eq!(manifest.restart, Restart::OnFailure);
        assert_eq!(manifest.capabilities, Capabilities::default());
        assert_eq!(manifest.lua, None);
        assert_eq!(manifest.wasm, Some(WasmOptions::default()));
        assert_eq!(
            manifest.describe_capabilities(),
            "filesystem=data network=yes env=all"
        );
        let lua = Manifest::parse(
            "name = \"a\"\nversion = \"1\"\napi = \"0.1\"\nengine = \"lua\"\n",
            "a",
        )
        .unwrap();
        assert_eq!(lua.entry, Some(PathBuf::from("init.lua")));
        assert_eq!(lua.lua, Some(LuaOptions::default()));
        assert_eq!(lua.wasm, None);
        let limited = Manifest::parse(
            &format!("{MINIMAL}[capabilities]\nfilesystem = []\nnetwork = false\nenv = []\n[wasm]\nmemory_limit_mb = 64\n"),
            "relaxoban",
        )
        .unwrap();
        assert_eq!(
            limited.wasm,
            Some(WasmOptions {
                memory_limit_mb: 64
            })
        );
        assert_eq!(
            limited.describe_capabilities(),
            "filesystem=none network=no env=none memory_limit_mb=64"
        );
    }

    #[test]
    fn full_manifest_parses() {
        let manifest = Manifest::parse(
            r#"
name = "relaxoban"
version = "0.3.1"
api = "0.1"
mode = "persistent"
engine = "lua"
entry = "src/main.lua"
hot_reload = true
[deadlines]
login = 2000
init = 60000
[failure]
on_timeout = "continue"
on_failure = "continue"
restart = "always"
[capabilities]
filesystem = ["data", "host", "/var/lib/relaxoban"]
network = false
env = ["RELAXOBAN_DSN"]
[lua]
ffi = true
native_modules = true
memory_limit_mb = 64
"#,
            "relaxoban",
        )
        .unwrap();
        assert!(manifest.hot_reload);
        assert_eq!(manifest.entry, Some(PathBuf::from("src/main.lua")));
        assert_eq!(manifest.deadline(Hook::Login), Some(Duration::from_secs(2)));
        assert_eq!(manifest.init_deadline(), Duration::from_secs(60));
        assert_eq!(manifest.on_timeout(Hook::Login), Policy::Continue);
        assert_eq!(manifest.on_failure(), Policy::Continue);
        assert_eq!(manifest.restart, Restart::Always);
        assert_eq!(
            manifest.describe_capabilities(),
            "filesystem=data,host,/var/lib/relaxoban network=no env=RELAXOBAN_DSN ffi=true native_modules=true memory_limit_mb=64"
        );
        let script = Manifest::parse(
            "name = \"a\"\nversion = \"1\"\napi = \"0.1\"\nmode = \"script\"\n",
            "a",
        )
        .unwrap();
        assert_eq!(script.mode, Mode::Script);
        assert_eq!(script.engine, None);
    }

    #[test]
    fn invalid_manifests_are_rejected() {
        let base = "name = \"a\"\nversion = \"1\"\napi = \"0.1\"\n";
        for (extra, folder) in [
            ("engine = \"lua\"\ntypo = 1\n", "a"),
            ("engine = \"lua\"\n", "b"),
            ("engine = \"python\"\n", "a"),
            ("", "a"),
            ("engine = \"wasm\"\n", "a"),
            ("engine = \"lua\"\nentry = \"../x.lua\"\n", "a"),
            ("engine = \"lua\"\nentry = \"/x.lua\"\n", "a"),
            ("engine = \"lua\"\nentry = \"./x.lua\"\n", "a"),
            ("engine = \"lua\"\nentry = \"\"\n", "a"),
            ("engine = \"lua\"\nmode = \"daemon\"\n", "a"),
            ("mode = \"script\"\nengine = \"lua\"\n", "a"),
            ("mode = \"script\"\n[deadlines]\nlogin = 5\n", "a"),
            ("engine = \"lua\"\n[deadlines]\nlogin = 0\n", "a"),
            ("engine = \"lua\"\n[deadlines]\nlogin = 10001\n", "a"),
            ("engine = \"lua\"\n[deadlines]\njoin = 10\n", "a"),
            ("engine = \"lua\"\n[deadlines]\nnope = 10\n", "a"),
            ("engine = \"lua\"\n[deadlines]\ninit = 300001\n", "a"),
            ("engine = \"lua\"\n[failure]\non_timeout = \"allow\"\n", "a"),
            (
                "engine = \"lua\"\n[failure]\nrestart = \"sometimes\"\n",
                "a",
            ),
            ("engine = \"lua\"\n[failure]\nextra = 1\n", "a"),
            (
                "engine = \"lua\"\n[capabilities]\nfilesystem = [\"tmp\"]\n",
                "a",
            ),
            (
                "engine = \"lua\"\n[capabilities]\nfilesystem = [\"data\", \"data\"]\n",
                "a",
            ),
            ("engine = \"lua\"\n[capabilities]\nenv = [\"1BAD\"]\n", "a"),
            (
                "engine = \"wasm\"\nentry = \"a.wasm\"\n[lua]\nffi = true\n",
                "a",
            ),
            ("engine = \"lua\"\n[lua]\nmemory_limit_mb = -1\n", "a"),
            ("engine = \"lua\"\n[lua]\njit = true\n", "a"),
            ("engine = \"lua\"\n[wasm]\nmemory_limit_mb = 1\n", "a"),
            (
                "engine = \"wasm\"\nentry = \"a.wasm\"\n[wasm]\nmemory_limit_mb = 4097\n",
                "a",
            ),
            (
                "engine = \"wasm\"\nentry = \"a.wasm\"\n[wasm]\nfuel = 1\n",
                "a",
            ),
            ("mode = \"script\"\n[wasm]\nmemory_limit_mb = 1\n", "a"),
        ] {
            let text = format!("{base}{extra}");
            assert!(Manifest::parse(&text, folder).is_err(), "{text}");
        }
        for text in [
            "version = \"1\"\napi = \"0.1\"\nengine = \"lua\"\n",
            "name = \"a\"\nversion = \"1 2\"\napi = \"0.1\"\nengine = \"lua\"\n",
            "name = \"a\"\nversion = \"1\"\napi = \"0.2\"\nengine = \"lua\"\n",
            "name = \"a/b\"\nversion = \"1\"\napi = \"0.1\"\nengine = \"lua\"\n",
            "name = [\n",
        ] {
            assert!(Manifest::parse(text, "a").is_err(), "{text}");
        }
        let error =
            Manifest::parse(&format!("{base}engine = \"lua\"\ntypo = 1\n"), "a").unwrap_err();
        assert!(error.contains("typo"), "{error}");
    }
}
