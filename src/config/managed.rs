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
