use super::*;
use std::path::{Component, PathBuf};

const MAX_SERVERS: usize = 128;
const MAX_ARGUMENTS: usize = 128;
const MAX_ARGUMENT_BYTES: usize = 8192;
const MAX_COMMAND_BYTES: usize = 65536;
const MAX_TIMEOUT_MS: usize = 86_400_000;

/// A persistent local Minecraft server directory and its supervised process.
/// Commands execute directly as argv, without shell expansion. The process
/// receives `stop\n` on stdin during an orderly shutdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedServer {
    pub command: Vec<String>,
    pub directory: PathBuf,
    pub autostart: bool,
    pub start_on_connect: bool,
    pub idle_timeout: Option<Duration>,
    pub start_timeout: Duration,
    pub stop_timeout: Duration,
    pub restart_delay: Duration,
    /// Maximum recovery attempts after a failed start or unexpected process exit.
    /// Zero disables automatic recovery.
    pub restart_retries: usize,
}

pub(super) fn parse(root: &Table, base: &Path) -> Result<BTreeMap<String, ManagedServer>, String> {
    let mut servers = BTreeMap::new();
    let Some(values) = options_map(root, "managed_servers")? else {
        return Ok(servers);
    };
    for pair in values.pairs::<Value, Value>() {
        let (name, value) = pair.map_err(|error| error.to_string())?;
        let name = string(name, "managed_servers key")?;
        let path = format!("managed_servers.{name}");
        let values = table(value, &path)?;
        fields(
            &values,
            &[
                "command",
                "directory",
                "autostart",
                "start_on_connect",
                "idle_timeout_ms",
                "start_timeout_ms",
                "stop_timeout_ms",
                "restart_delay_ms",
                "restart_retries",
            ],
            &path,
        )?;
        let command = string_list(
            values
                .raw_get("command")
                .map_err(|error| error.to_string())?,
            &format!("{path}.command"),
            MAX_ARGUMENTS,
        )?;
        let directory = string(
            values
                .raw_get("directory")
                .map_err(|error| error.to_string())?,
            &format!("{path}.directory"),
        )?;
        validate_directory(Path::new(&directory), &path)?;
        let directory = resolve_directory(&base.join(directory))
            .map_err(|error| format!("{path}.directory: {error}"))?;
        let idle_timeout = match values
            .raw_get::<Value>("idle_timeout_ms")
            .map_err(|error| error.to_string())?
        {
            Value::Nil | Value::Integer(0) | Value::Number(0.0) => None,
            _ => Some(Duration::from_millis(integer(
                &values,
                &path,
                "idle_timeout_ms",
                1,
                MAX_TIMEOUT_MS,
            )? as u64)),
        };
        servers.insert(
            name,
            ManagedServer {
                command,
                directory,
                autostart: boolean(&values, &path, "autostart", false)?,
                start_on_connect: boolean(&values, &path, "start_on_connect", true)?,
                idle_timeout,
                start_timeout: Duration::from_millis(integer(
                    &values,
                    &path,
                    "start_timeout_ms",
                    120_000,
                    MAX_TIMEOUT_MS,
                )? as u64),
                stop_timeout: Duration::from_millis(integer(
                    &values,
                    &path,
                    "stop_timeout_ms",
                    30_000,
                    MAX_TIMEOUT_MS,
                )? as u64),
                restart_delay: Duration::from_millis(integer(
                    &values,
                    &path,
                    "restart_delay_ms",
                    5000,
                    MAX_TIMEOUT_MS,
                )? as u64),
                restart_retries: bounded_count(&values, &path, "restart_retries", 3, 100)?,
            },
        );
    }
    Ok(servers)
}

pub(super) fn validate(
    servers: &BTreeMap<String, ManagedServer>,
    backends: &BTreeMap<String, Backend>,
) -> Result<(), String> {
    if servers.len() > MAX_SERVERS {
        return Err(format!("managed_servers: at most {MAX_SERVERS} servers"));
    }
    let mut directories = BTreeMap::new();
    for (name, server) in servers {
        let path = format!("managed_servers.{name}");
        if server.restart_retries > 100 {
            return Err(format!(
                "{path}.restart_retries: expected an integer in 0..=100"
            ));
        }
        if name.trim().is_empty() {
            return Err("managed_servers: names must not be empty".into());
        }
        let backend = backends
            .get(name)
            .ok_or_else(|| format!("{path}: unknown backend"))?;
        let address = backend
            .address()
            .parse::<SocketAddr>()
            .ok()
            .filter(|address| address.ip().to_canonical().is_loopback() && address.port() != 0)
            .ok_or_else(|| {
                format!("{path}: backend must be a literal loopback IP address with a nonzero port")
            })?;
        for (other_name, other) in backends {
            if other_name != name && endpoint_alias(address, other) {
                return Err(format!(
                    "{path}: backend address conflicts with backends.{other_name}; managed endpoints must have a unique backend name"
                ));
            }
        }
        if server.command.is_empty() || server.command.len() > MAX_ARGUMENTS {
            return Err(format!(
                "{path}.command: expected 1..={MAX_ARGUMENTS} arguments"
            ));
        }
        if server.command[0].trim().is_empty() {
            return Err(format!("{path}.command: executable must not be empty"));
        }
        if server
            .command
            .iter()
            .any(|argument| argument.len() > MAX_ARGUMENT_BYTES || argument.contains('\0'))
            || server.command.iter().map(String::len).sum::<usize>() > MAX_COMMAND_BYTES
        {
            return Err(format!(
                "{path}.command: arguments must not contain NUL and are limited to {MAX_ARGUMENT_BYTES} bytes each, {MAX_COMMAND_BYTES} bytes total"
            ));
        }
        validate_directory(&server.directory, &path)?;
        let directory = resolve_directory(&server.directory)
            .map_err(|error| format!("{path}.directory: {error}"))?;
        if let Some(other_name) = directories.insert(directory, name) {
            return Err(format!(
                "{path}.directory: directory is already used by managed_servers.{other_name}"
            ));
        }
        for (field, value) in [
            ("idle_timeout_ms", server.idle_timeout),
            ("start_timeout_ms", Some(server.start_timeout)),
            ("stop_timeout_ms", Some(server.stop_timeout)),
            ("restart_delay_ms", Some(server.restart_delay)),
        ] {
            if value.is_some_and(|value| {
                value < Duration::from_millis(1)
                    || value > Duration::from_millis(MAX_TIMEOUT_MS as u64)
            }) {
                return Err(format!(
                    "{path}.{field}: expected 1..={MAX_TIMEOUT_MS} milliseconds"
                ));
            }
        }
    }
    Ok(())
}

fn endpoint_alias(address: SocketAddr, other: &Backend) -> bool {
    if let Ok(other) = other.address().parse::<SocketAddr>() {
        return address.port() == other.port()
            && address.ip().to_canonical() == other.ip().to_canonical();
    }
    let Some((host, port)) = other.address().rsplit_once(':') else {
        return false;
    };
    host.trim_end_matches('.').eq_ignore_ascii_case("localhost")
        && port.parse::<u16>().ok() == Some(address.port())
}

/// Counts may be zero; ordinary configuration integers require a positive value.
pub(super) fn bounded_count(
    table: &Table,
    path: &str,
    key: &str,
    default: usize,
    max: usize,
) -> Result<usize, String> {
    let value: Value = table.raw_get(key).map_err(|e| e.to_string())?;
    match value {
        Value::Nil => Ok(default),
        Value::Integer(value) if value >= 0 && value as u64 <= max as u64 => Ok(value as usize),
        Value::Number(value)
            if value.is_finite() && value.fract() == 0.0 && value >= 0.0 && value <= max as f64 =>
        {
            Ok(value as usize)
        }
        _ => Err(format!("{path}.{key}: expected an integer in 0..={max}")),
    }
}

fn validate_directory(directory: &Path, path: &str) -> Result<(), String> {
    let directory = directory.as_os_str().as_encoded_bytes();
    if directory.is_empty() || directory.len() > 4096 || directory.contains(&0) {
        return Err(format!(
            "{path}.directory: expected 1..=4096 bytes without NUL"
        ));
    }
    Ok(())
}

/// Canonicalize existing ancestors to catch symlink aliases, while accepting
/// directories that the operator has yet to provision. Never create anything.
fn resolve_directory(path: &Path) -> io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut result = PathBuf::new();
    for component in absolute.components() {
        match component {
            // A Windows prefix (C: or \\?\C:) is not a rooted path yet.
            // Inspect it only after the following root component is appended.
            Component::Prefix(_) => {
                result.push(component.as_os_str());
                continue;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            component => result.push(component.as_os_str()),
        }
        match fs::canonicalize(&result) {
            Ok(canonical) => result = canonical,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    if result.exists() && !result.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a directory",
        ));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(policy: &str) -> io::Result<Config> {
        Config::from_lua(
            &format!(
                "return {{listeners={{public='127.0.0.1:25565'}},backends={{lobby='127.0.0.1:25566'}},routes={{public='lobby'}},managed_servers={{lobby={{directory='servers/lobby',command={{'java'}},{policy}}}}}}}"
            ),
            "retries.lua",
        )
    }

    #[test]
    fn recovery_retry_defaults_and_boundaries() {
        assert_eq!(
            config("").unwrap().managed_servers["lobby"].restart_retries,
            3
        );
        for retries in ["0", "0.0", "100", "100.0"] {
            let config = config(&format!("restart_retries={retries}")).unwrap();
            assert_eq!(
                config.managed_servers["lobby"].restart_retries,
                retries.parse::<f64>().unwrap() as usize
            );
        }
    }

    #[test]
    fn recovery_retry_values_and_mutations_are_validated() {
        for retries in ["-1", "101", "0.5", "true", "'3'", "math.huge", "0/0"] {
            let error = config(&format!("restart_retries={retries}"))
                .unwrap_err()
                .to_string();
            assert!(error.contains("restart_retries"), "{retries}: {error}");
        }
        let mut config = config("").unwrap();
        config
            .managed_servers
            .get_mut("lobby")
            .unwrap()
            .restart_retries = 101;
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("restart_retries")
        );
    }
}
