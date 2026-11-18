//! `backups`: snapshots of persistent instances on nodes, kept in a folder on
//! this host or in an S3 bucket. See docs/backups.md.
use super::*;

/// Below the configuration directory, next to the CloudNet-style `local/`.
pub const DEFAULT_BACKUP_DIRECTORY: &str = "local/backups";
const DEFAULT_INTERVAL_MS: usize = 6 * 60 * 60 * 1000;
const MIN_INTERVAL_MS: usize = 60 * 1000;
const MAX_INTERVAL_MS: usize = 30 * 24 * 60 * 60 * 1000;
const DEFAULT_KEEP: usize = 7;
const MAX_KEEP: usize = 1000;
pub const DEFAULT_ACCESS_KEY_ENV: &str = "RIFT_BACKUP_ACCESS_KEY";
pub const DEFAULT_SECRET_KEY_ENV: &str = "RIFT_BACKUP_SECRET_KEY";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backups {
    pub target: BackupTarget,
    /// How old an instance's newest snapshot may get before the next one is
    /// taken; `None` (`interval_ms = 0`) only backs up on request.
    pub interval: Option<Duration>,
    /// Snapshots kept per instance; older ones are deleted after each backup.
    pub keep: usize,
    /// The service groups whose instances are backed up; `None` is every
    /// persistent group with `nodes`.
    pub groups: Option<BTreeSet<String>>,
    /// Flush a running server's world with `save-off` and `save-all flush`
    /// before copying it, and `save-on` after.
    pub save_commands: bool,
}

impl Backups {
    /// Whether instances of `group` are backed up on schedule.
    pub fn covers(&self, group: &str) -> bool {
        self.groups
            .as_ref()
            .is_none_or(|groups| groups.contains(group))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupTarget {
    /// A folder on the controller host.
    Directory(PathBuf),
    S3(S3Target),
}

/// An S3 bucket, or a bucket of any S3-compatible service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Target {
    pub bucket: String,
    pub region: String,
    /// `scheme://host[:port]` without a path; AWS for the region when unset.
    pub endpoint: String,
    /// Prepended to every key: empty or ending in `/`.
    pub prefix: String,
    /// `endpoint/bucket/key` instead of `bucket.endpoint/key`.
    pub path_style: bool,
    pub access_key_env: String,
    pub secret_key_env: String,
}

pub(super) fn parse(
    root: &Table,
    base: &Path,
    groups: &BTreeMap<String, ServiceGroup>,
) -> Result<Option<Backups>, String> {
    let Some(values) = options(
        root,
        "backups",
        &[
            "directory",
            "s3",
            "interval_ms",
            "keep",
            "groups",
            "save_commands",
        ],
    )?
    else {
        return Ok(None);
    };
    let directory = values
        .raw_get::<Value>("directory")
        .map_err(|e| e.to_string())?;
    let s3 = values.raw_get::<Value>("s3").map_err(|e| e.to_string())?;
    let target = match (directory, s3) {
        (Value::Nil, Value::Nil) => BackupTarget::Directory(base.join(DEFAULT_BACKUP_DIRECTORY)),
        (directory, Value::Nil) => {
            let value = string(directory, "backups.directory")?;
            if value.trim().is_empty()
                || value.len() > 4096
                || value.contains('\0')
                || value.contains(['{', '}'])
            {
                return Err(
                    "backups.directory: expected a nonblank directory path without NUL or placeholders"
                        .into(),
                );
            }
            BackupTarget::Directory(base.join(value))
        }
        (Value::Nil, s3) => BackupTarget::S3(parse_s3(table(s3, "backups.s3")?)?),
        _ => return Err("backups: set directory or s3, not both".into()),
    };
    let interval_error = || {
        format!(
            "backups.interval_ms: expected 0 (only on request) or an integer in {MIN_INTERVAL_MS}..={MAX_INTERVAL_MS}"
        )
    };
    let interval = match values
        .raw_get::<Value>("interval_ms")
        .map_err(|e| e.to_string())?
    {
        Value::Integer(0) => None,
        Value::Number(0.0) => None,
        _ => {
            let milliseconds = integer(
                &values,
                "backups",
                "interval_ms",
                DEFAULT_INTERVAL_MS,
                MAX_INTERVAL_MS,
            )
            .map_err(|_| interval_error())?;
            if milliseconds < MIN_INTERVAL_MS {
                return Err(interval_error());
            }
            Some(Duration::from_millis(milliseconds as u64))
        }
    };
    let keep = integer(&values, "backups", "keep", DEFAULT_KEEP, MAX_KEEP)?;
    let selected = match values
        .raw_get::<Value>("groups")
        .map_err(|e| e.to_string())?
    {
        Value::Nil => None,
        value => {
            let selected = string_set(value, "backups.groups")?;
            for name in &selected {
                let Some(group) = groups.get(name) else {
                    return Err(format!("backups.groups: unknown service group {name:?}"));
                };
                if group.storage != InstanceStorage::Persistent || group.nodes.is_none() {
                    return Err(format!(
                        "backups.groups: service group {name} has no persistent instances on nodes; only those are backed up"
                    ));
                }
            }
            Some(selected)
        }
    };
    Ok(Some(Backups {
        target,
        interval,
        keep,
        groups: selected,
        save_commands: boolean(&values, "backups", "save_commands", true)?,
    }))
}

fn parse_s3(values: Table) -> Result<S3Target, String> {
    const PATH: &str = "backups.s3";
    fields(
        &values,
        &[
            "bucket",
            "region",
            "endpoint",
            "prefix",
            "path_style",
            "access_key_env",
            "secret_key_env",
        ],
        PATH,
    )?;
    let optional = |key: &str| -> Result<Option<String>, String> {
        match values.raw_get::<Value>(key).map_err(|e| e.to_string())? {
            Value::Nil => Ok(None),
            value => string(value, &format!("{PATH}.{key}")).map(Some),
        }
    };
    let bucket = optional("bucket")?.ok_or_else(|| format!("{PATH}.bucket: required"))?;
    if !(3..=63).contains(&bucket.len())
        || !bucket
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(format!(
            "{PATH}.bucket: expected 3..=63 letters, digits, dots, hyphens or underscores"
        ));
    }
    let region = optional("region")?.unwrap_or_else(|| "us-east-1".into());
    if region.is_empty()
        || region.len() > 64
        || !region
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(format!(
            "{PATH}.region: expected letters, digits, hyphens or underscores"
        ));
    }
    let configured = optional("endpoint")?;
    let path_style = boolean(&values, PATH, "path_style", configured.is_some())?;
    let endpoint = match configured {
        Some(endpoint) => {
            endpoint_origin(&endpoint).map_err(|error| format!("{PATH}.endpoint: {error}"))?
        }
        None => format!("https://s3.{region}.amazonaws.com"),
    };
    let mut prefix = optional("prefix")?.unwrap_or_default();
    if prefix.len() > 256
        || prefix.starts_with('/')
        || prefix
            .split('/')
            .any(|segment| segment == "." || segment == "..")
        || prefix.contains("//")
        || !prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'/'))
    {
        return Err(format!(
            "{PATH}.prefix: expected up to 256 letters, digits, '.', '-', '_' and '/' without a leading '/'"
        ));
    }
    if !prefix.is_empty() && !prefix.ends_with('/') {
        prefix.push('/');
    }
    let variable = |key: &str, default: &str| -> Result<String, String> {
        let name = optional(key)?.unwrap_or_else(|| default.to_owned());
        if !environment_name(&name) {
            return Err(format!(
                "{PATH}.{key}: expected an environment variable name"
            ));
        }
        Ok(name)
    };
    Ok(S3Target {
        bucket,
        region,
        endpoint,
        prefix,
        path_style,
        access_key_env: variable("access_key_env", DEFAULT_ACCESS_KEY_ENV)?,
        secret_key_env: variable("secret_key_env", DEFAULT_SECRET_KEY_ENV)?,
    })
}

/// `scheme://host[:port]` of an S3 endpoint. Plain HTTP would send the
/// signed requests and every world in the clear, so it is only accepted on
/// loopback, for a local MinIO.
fn endpoint_origin(endpoint: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(endpoint).map_err(|error| error.to_string())?;
    let host = url.host_str().ok_or("expected a URL with a host")?;
    let loopback = host == "localhost"
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => return Err("expected an https:// URL (http:// only on loopback)".into()),
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("expected scheme://host[:port] without credentials, path or query".into());
    }
    Ok(match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "rift.config.listeners.public='127.0.0.1:0'
rift.config.routes.public='game'
rift.config.messaging={listen='127.0.0.1:0',certificate='c.pem',private_key='k.pem'}
rift.config.nodes.node1={address='10.0.0.1'}
rift.config.templates.t={server_jar='paper.jar'}
rift.config.service_groups.game={template='t',storage='disposable',port_range={30000,30099},nodes={'node1'}}
rift.config.service_groups.world={template='t',storage='persistent',port_range={31000,31099},nodes={'node1'}}
";

    fn parse(backups: &str) -> Result<Config, String> {
        Config::from_lua(
            &format!("{BASE}rift.config.backups={backups}"),
            "backups.lua",
        )
        .map_err(|error| error.to_string())
    }

    #[test]
    fn backups_default_to_a_folder_every_six_hours_and_check_their_groups() {
        assert!(
            Config::from_lua(BASE, "backups.lua")
                .unwrap()
                .backups
                .is_none()
        );
        let backups = parse("{}").unwrap().backups.unwrap();
        match &backups.target {
            BackupTarget::Directory(path) => assert!(path.ends_with(DEFAULT_BACKUP_DIRECTORY)),
            other => panic!("{other:?}"),
        }
        assert_eq!(backups.interval, Some(Duration::from_secs(6 * 60 * 60)));
        assert_eq!(backups.keep, 7);
        assert!(backups.save_commands && backups.groups.is_none());
        assert!(backups.covers("world"));
        let manual = parse("{interval_ms=0, keep=30, groups={'world'}, save_commands=false}")
            .unwrap()
            .backups
            .unwrap();
        assert_eq!(manual.interval, None);
        assert_eq!(manual.keep, 30);
        assert!(!manual.save_commands);
        assert!(manual.covers("world") && !manual.covers("game"));
        for bad in [
            "{interval_ms=1000}",
            "{interval_ms=-1}",
            "{keep=0}",
            "{groups={'game'}}",
            "{groups={'ghost'}}",
            "{directory=''}",
            "{directory='a', s3={bucket='rift-backups'}}",
            "{s3={}}",
            "{frequency=1}",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn s3_targets_default_to_aws_and_read_credentials_from_the_environment() {
        let config =
            parse("{s3={bucket='rift-backups', region='eu-central-1', prefix='prod'}}").unwrap();
        let Some(Backups {
            target: BackupTarget::S3(s3),
            ..
        }) = &config.backups
        else {
            panic!("{:?}", config.backups);
        };
        assert_eq!(s3.endpoint, "https://s3.eu-central-1.amazonaws.com");
        assert!(!s3.path_style);
        assert_eq!(s3.prefix, "prod/");
        let variables = config.secret_variables();
        for (field, variable) in [
            ("backups.s3.access_key_env", DEFAULT_ACCESS_KEY_ENV),
            ("backups.s3.secret_key_env", DEFAULT_SECRET_KEY_ENV),
        ] {
            assert!(
                variables.contains(&(field.into(), variable.into())),
                "{variables:?}"
            );
        }
        let config = parse(
            "{s3={bucket='rift', endpoint='http://127.0.0.1:9000', access_key_env='MINIO_USER', secret_key_env='MINIO_PASSWORD'}}",
        )
        .unwrap();
        let Some(Backups {
            target: BackupTarget::S3(minio),
            ..
        }) = &config.backups
        else {
            panic!("{:?}", config.backups);
        };
        assert!(minio.path_style);
        assert_eq!(minio.endpoint, "http://127.0.0.1:9000");
        assert_eq!(minio.access_key_env, "MINIO_USER");
        assert_eq!(minio.region, "us-east-1");
        for bad in [
            "{s3={bucket='x'}}",
            "{s3={bucket='rift', prefix='/abs'}}",
            "{s3={bucket='rift', prefix='a/../b'}}",
            "{s3={bucket='rift', endpoint='http://s3.example.com'}}",
            "{s3={bucket='rift', access_key_env='not a name'}}",
            "{s3={bucket='rift', secret='inline'}}",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn endpoints_keep_only_their_origin_and_need_tls_off_loopback() {
        assert_eq!(
            endpoint_origin("https://fsn1.your-objectstorage.com/").unwrap(),
            "https://fsn1.your-objectstorage.com"
        );
        assert_eq!(
            endpoint_origin("http://127.0.0.1:9000").unwrap(),
            "http://127.0.0.1:9000"
        );
        assert_eq!(
            endpoint_origin("http://localhost:9000").unwrap(),
            "http://localhost:9000"
        );
        // The default port is not part of the origin, as reqwest sends it.
        assert_eq!(
            endpoint_origin("https://s3.example.com:443").unwrap(),
            "https://s3.example.com"
        );
        for bad in [
            "http://s3.example.com",
            "https://s3.example.com/bucket",
            "https://key:secret@s3.example.com",
            "https://s3.example.com?x=1",
            "ftp://s3.example.com",
            "s3.example.com",
        ] {
            assert!(endpoint_origin(bad).is_err(), "{bad}");
        }
    }
}
