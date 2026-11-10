//! The plugin's configuration file: the client [`Config`] as TOML or JSON in
//! the plugin's data folder, with relative paths resolved against that folder.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use rift_permissions_client::{Config, OfflinePolicy};

/// File names looked up in the data folder, in this order.
pub const CONFIG_FILES: [&str; 2] = ["config.toml", "config.json"];

/// Written to `config.toml` when the data folder has no configuration yet.
pub const DEFAULT_CONFIG: &str = r#"# Rift permissions for Pumpkin. See docs/permissions-pumpkin.md.

# On a server Rift manages with a messaging block, leave out address,
# server_name, ca, token_env and server: Rift sets RIFT_BUS_ADDRESS,
# RIFT_BUS_SERVER_NAME, RIFT_BUS_CERT, RIFT_BUS_TOKEN, RIFT_SERVER_NAME and
# RIFT_SERVER_GROUP for each instance, and unset fields fall back to them.

# host:port of the proxy's messaging listener.
address = "127.0.0.1:7443"
# Name the proxy's certificate is verified against.
server_name = "localhost"
# PEM file with the proxy's CA or certificate; relative to this folder.
ca = "ca.pem"
# Environment variable that holds this backend's bus token.
token_env = "RIFT_PERMISSIONS_TOKEN"
# This backend's name on the proxy, and the `server` context.
server = "pumpkin"
# This backend's service group, when it is an instance of one (lobby for
# lobby-2): the `group` context.
# group = "lobby"
# How long a login waits for the player's snapshot.
join_timeout_ms = 2000
# How long /perms, writes and group requests wait for the proxy.
request_timeout_ms = 5000

# What players get while the proxy is unreachable: "deny" (Pumpkin's own
# defaults and op levels), "last_snapshot" or "fallback_file" (both with a
# `path`, relative to this folder).
[offline_policy]
mode = "last_snapshot"
path = "replica.json"
"#;

/// Why the configuration could not be loaded.
#[derive(Debug)]
pub enum ConfigError {
    /// No configuration file existed; a template was written to this path.
    Created(PathBuf),
    /// The file could not be read or written.
    Io(PathBuf, io::Error),
    /// The file is not a valid configuration.
    Invalid(PathBuf, String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created(path) => write!(
                f,
                "no configuration found: wrote a template to {}; set the proxy address, CA, token variable and server name there and restart",
                path.display()
            ),
            Self::Io(path, error) => write!(f, "{}: {error}", path.display()),
            Self::Invalid(path, error) => write!(f, "{}: {error}", path.display()),
        }
    }
}

impl std::error::Error for ConfigError {}

/// The configuration file format, chosen by extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Toml,
    Json,
}

impl Format {
    /// The format for a file name: `.json` is JSON, anything else TOML.
    pub fn of(path: &Path) -> Self {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("json") => Self::Json,
            _ => Self::Toml,
        }
    }
}

/// Parse a configuration file's contents. Unknown keys are errors, so a typo
/// does not silently fall back to a default.
pub fn parse(text: &str, format: Format) -> Result<Config, String> {
    match format {
        Format::Toml => toml::from_str(text).map_err(|error| error.to_string()),
        Format::Json => serde_json::from_str(text).map_err(|error| error.to_string()),
    }
}

/// Resolve the CA and offline-policy paths against `base` (the data folder)
/// when they are relative.
pub fn resolve(mut config: Config, base: &Path) -> Config {
    let join = |path: PathBuf| {
        if path.is_relative() {
            base.join(path)
        } else {
            path
        }
    };
    // An unset CA stays unset, for `RIFT_BUS_CERT` to fill.
    if !config.ca.as_os_str().is_empty() {
        config.ca = join(config.ca);
    }
    config.offline_policy = match config.offline_policy {
        OfflinePolicy::Deny => OfflinePolicy::Deny,
        OfflinePolicy::LastSnapshot(path) => OfflinePolicy::LastSnapshot(join(path)),
        OfflinePolicy::FallbackFile(path) => OfflinePolicy::FallbackFile(join(path)),
    };
    config
}

/// Load the configuration from `folder`, writing [`DEFAULT_CONFIG`] to
/// `config.toml` when no configuration file exists. Unset connection fields
/// come from the `RIFT_*` variables of a server Rift manages; with those set
/// and no file at all, the plugin starts on them alone, so a template needs
/// no configuration for this plugin.
pub fn load(folder: &Path) -> Result<Config, ConfigError> {
    load_with(folder, |name| {
        std::env::var(name).ok().filter(|value| !value.is_empty())
    })
}

/// [`load`] over any variable lookup, for tests.
pub fn load_with(
    folder: &Path,
    variable: impl Fn(&str) -> Option<String>,
) -> Result<Config, ConfigError> {
    if CONFIG_FILES.iter().all(|name| !folder.join(name).exists())
        && variable("RIFT_BUS_TOKEN").is_some()
        && variable("RIFT_BUS_ADDRESS").is_some()
    {
        return Ok(Config::new("", "", "", "", "").with_variables(variable));
    }
    load_file(folder).map(|config| config.with_variables(variable))
}

fn load_file(folder: &Path) -> Result<Config, ConfigError> {
    for name in CONFIG_FILES {
        let path = folder.join(name);
        match fs::read_to_string(&path) {
            Ok(text) => {
                let config = parse(&text, Format::of(&path))
                    .map_err(|error| ConfigError::Invalid(path.clone(), error))?;
                return Ok(resolve(config, folder));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(ConfigError::Io(path, error)),
        }
    }
    let path = folder.join(CONFIG_FILES[0]);
    fs::create_dir_all(folder)
        .and_then(|()| fs::write(&path, DEFAULT_CONFIG))
        .map_err(|error| ConfigError::Io(path.clone(), error))?;
    Err(ConfigError::Created(path))
}

/// A short description of the offline policy for log lines.
pub fn describe_policy(policy: &OfflinePolicy) -> String {
    match policy {
        OfflinePolicy::Deny => "deny".to_owned(),
        OfflinePolicy::LastSnapshot(path) => format!("last_snapshot ({})", path.display()),
        OfflinePolicy::FallbackFile(path) => format!("fallback_file ({})", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_template_parses() {
        let config = parse(DEFAULT_CONFIG, Format::Toml).unwrap();
        assert_eq!(config.server, "pumpkin");
        assert_eq!(config.join_timeout, Duration::from_secs(2));
        assert_eq!(
            config.offline_policy,
            OfflinePolicy::LastSnapshot("replica.json".into())
        );
    }

    #[test]
    fn toml_and_json_agree() {
        let toml = parse(
            r#"address = "proxy:7443"
               server_name = "proxy"
               ca = "/etc/rift/ca.pem"
               token_env = "TOKEN"
               server = "survival-1"
               join_timeout_ms = 500
               [offline_policy]
               mode = "fallback_file"
               path = "fallback.json""#,
            Format::Toml,
        )
        .unwrap();
        let json = parse(
            r#"{"address": "proxy:7443", "server_name": "proxy", "ca": "/etc/rift/ca.pem",
                "token_env": "TOKEN", "server": "survival-1", "join_timeout_ms": 500,
                "offline_policy": {"mode": "fallback_file", "path": "fallback.json"}}"#,
            Format::Json,
        )
        .unwrap();
        assert_eq!(toml, json);
        assert_eq!(toml.join_timeout, Duration::from_millis(500));
        assert_eq!(toml.request_timeout, Duration::from_secs(5));
    }

    #[test]
    fn defaults_and_errors() {
        let minimal = parse(
            "address = \"a:1\"\nserver_name = \"a\"\nca = \"ca.pem\"\ntoken_env = \"T\"\nserver = \"s\"\n",
            Format::Toml,
        )
        .unwrap();
        assert_eq!(minimal.offline_policy, OfflinePolicy::Deny);
        let deny = parse(
            "address = \"a:1\"\nserver_name = \"a\"\nca = \"ca.pem\"\ntoken_env = \"T\"\nserver = \"s\"\noffline_policy = { mode = \"deny\" }\n",
            Format::Toml,
        )
        .unwrap();
        assert_eq!(deny.offline_policy, OfflinePolicy::Deny);
        // Unknown keys are rejected. Missing connection keys parse: they
        // fall back to Rift's variables, and the client refuses to start
        // when neither sets them.
        assert!(parse("address = \"a:1\"\nadress = \"typo\"\n", Format::Toml).is_err());
        assert!(parse("server = \"s\"\n", Format::Toml).unwrap().address.is_empty());
        assert!(parse("{", Format::Json).is_err());
    }

    #[test]
    fn format_by_extension() {
        assert_eq!(Format::of(Path::new("config.json")), Format::Json);
        assert_eq!(Format::of(Path::new("CONFIG.JSON")), Format::Json);
        assert_eq!(Format::of(Path::new("config.toml")), Format::Toml);
        assert_eq!(Format::of(Path::new("config")), Format::Toml);
    }

    #[test]
    fn relative_paths_resolve_against_the_data_folder() {
        let base = Path::new("plugins/data/rift-permissions-pumpkin");
        let mut config = Config::new("a:1", "a", "ca.pem", "T", "s");
        config.offline_policy = OfflinePolicy::LastSnapshot("replica.json".into());
        let config = resolve(config, base);
        assert_eq!(config.ca, base.join("ca.pem"));
        assert_eq!(
            config.offline_policy,
            OfflinePolicy::LastSnapshot(base.join("replica.json"))
        );
        let absolute = resolve(Config::new("a:1", "a", "/abs/ca.pem", "T", "s"), base);
        assert_eq!(absolute.ca, Path::new("/abs/ca.pem"));
        assert_eq!(absolute.offline_policy, OfflinePolicy::Deny);
    }

    #[test]
    fn a_managed_instance_needs_no_file() {
        let folder = std::env::temp_dir().join(format!(
            "rift-permissions-pumpkin-env-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&folder);
        let variables = std::collections::HashMap::from([
            ("RIFT_BUS_ADDRESS", "10.0.0.2:7443"),
            ("RIFT_BUS_SERVER_NAME", "rift.internal"),
            ("RIFT_BUS_CERT", "/srv/lobby-3/rift-bus.pem"),
            ("RIFT_BUS_TOKEN", "ri1.lobby-3.1.x"),
            ("RIFT_SERVER_NAME", "lobby-3"),
            ("RIFT_SERVER_GROUP", "lobby"),
        ]);
        let lookup = |name: &str| variables.get(name).map(|value| (*value).to_owned());
        let config = load_with(&folder, lookup).unwrap();
        assert!(
            !folder.join("config.toml").exists(),
            "no template is written"
        );
        assert_eq!(config.server, "lobby-3");
        assert_eq!(config.group.as_deref(), Some("lobby"));
        assert_eq!(config.server_name, "rift.internal");
        assert_eq!(config.token_env, "RIFT_BUS_TOKEN");
        // A file that leaves the connection out gets it from Rift too.
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("config.toml"), "join_timeout_ms = 3000\n").unwrap();
        let config = load_with(&folder, lookup).unwrap();
        assert_eq!(config.address, "10.0.0.2:7443");
        assert_eq!(config.ca, PathBuf::from("/srv/lobby-3/rift-bus.pem"));
        let _ = fs::remove_dir_all(&folder);
    }

    #[test]
    fn load_writes_a_template_then_reads_it() {
        let folder = std::env::temp_dir().join(format!(
            "rift-permissions-pumpkin-config-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&folder);
        match load(&folder) {
            Err(ConfigError::Created(path)) => assert_eq!(path, folder.join("config.toml")),
            other => panic!("expected a template, got {other:?}"),
        }
        let config = load(&folder).unwrap();
        assert_eq!(config.ca, folder.join("ca.pem"));
        fs::write(folder.join("config.toml"), "server = 1").unwrap();
        assert!(matches!(load(&folder), Err(ConfigError::Invalid(..))));
        fs::remove_dir_all(&folder).unwrap();
    }
}
