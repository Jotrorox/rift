//! Downloadable server software for templates, resolved like CloudNet's
//! version catalog: a type and version name select an upstream build, and the
//! verified jar is cached once under `{cache}/{type}-{version}/server.jar`.
//!
//! Downloads never run during configuration loading or `rift check`.
use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// The jar's name inside each cache entry.
pub const JAR: &str = "server.jar";
const METADATA: &str = "rift-software.json";
const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;
const MAX_METADATA: usize = 8 * 1024 * 1024;
const MAX_VERSIONS: usize = 200;
const VERSION_CACHE: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Paper,
    Folia,
    Purpur,
    Vanilla,
    Fabric,
    Url,
}

impl Kind {
    pub const ALL: [Kind; 6] = [
        Kind::Paper,
        Kind::Folia,
        Kind::Purpur,
        Kind::Vanilla,
        Kind::Fabric,
        Kind::Url,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Paper => "paper",
            Self::Folia => "folia",
            Self::Purpur => "purpur",
            Self::Vanilla => "vanilla",
            Self::Fabric => "fabric",
            Self::Url => "url",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }

    /// Paper and its forks read Velocity forwarding from `paper-global.yml`.
    pub fn paper_family(self) -> bool {
        matches!(self, Self::Paper | Self::Folia | Self::Purpur)
    }
}

/// A requested build. `version` is a release name or `latest`; `url`
/// downloads a fixed artifact, optionally pinned by SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Software {
    pub kind: Kind,
    pub version: String,
    pub url: Option<String>,
    pub sha256: Option<String>,
}

impl Software {
    pub fn validate(&self) -> Result<(), String> {
        if self.kind == Kind::Url {
            let url = self.url.as_deref().ok_or("url: required for type 'url'")?;
            check_url(url).map_err(|error| format!("url: {error}"))?;
            if !self.version.is_empty() {
                return Err(
                    "version: not used for type 'url'; pin the artifact with sha256".into(),
                );
            }
        } else {
            if self.url.is_some() || self.sha256.is_some() {
                return Err("url and sha256 are only used with type 'url'".into());
            }
            if !valid_version(&self.version) {
                return Err(
                    "version: expected 'latest' or 1..=64 ASCII letters, digits, '.', '_', '+' or '-'"
                        .into(),
                );
            }
        }
        if let Some(sha256) = &self.sha256
            && (sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err("sha256: expected 64 hexadecimal digits".into());
        }
        Ok(())
    }

    /// CloudNet-style cache entry name, such as `paper-1.21.8` or `paper-latest`.
    pub fn key(&self) -> String {
        match &self.url {
            Some(url) if self.kind == Kind::Url => {
                let digest = hex(&Sha256::digest(url.as_bytes()));
                format!("url-{}", &digest[..16])
            }
            _ => format!("{}-{}", self.kind.as_str(), self.version),
        }
    }
}

fn valid_version(version: &str) -> bool {
    (1..=64).contains(&version.len())
        && !version.starts_with('.')
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
}

/// The jar a template copies into new instances.
pub fn jar_path(cache: &Path, software: &Software) -> PathBuf {
    cache.join(software.key()).join(JAR)
}

/// Cache state for listings; never contacts upstream.
pub fn status(cache: &Path, software: &Software) -> Value {
    let directory = cache.join(software.key());
    let mut value = json!({
        "type": software.kind.as_str(),
        "version": software.version,
        "cached": directory.join(JAR).is_file(),
    });
    if let Ok(bytes) = fs::read(directory.join(METADATA))
        && let Ok(Value::Object(metadata)) = serde_json::from_slice(&bytes)
    {
        for key in [
            "resolved_version",
            "build",
            "size",
            "sha256",
            "downloaded_at",
        ] {
            if let Some(field) = metadata.get(key) {
                value[key] = field.clone();
            }
        }
    }
    if software.kind == Kind::Url {
        value["url"] = json!(software.url);
    }
    value
}

/// Upstream services; tests point these at a local server.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub fill: String,
    pub purpur: String,
    pub mojang: String,
    pub fabric: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            fill: "https://fill.papermc.io".into(),
            purpur: "https://api.purpurmc.org".into(),
            mojang: "https://piston-meta.mojang.com".into(),
            fabric: "https://meta.fabricmc.net".into(),
        }
    }
}

/// A concrete artifact selected from a catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub url: String,
    pub version: String,
    pub build: Option<String>,
    pub sha256: Option<String>,
    pub sha1: Option<String>,
}

/// Download the jar unless it is already cached, and return its path.
pub async fn ensure(cache: &Path, software: &Software) -> io::Result<PathBuf> {
    download(cache, software, false, &Endpoints::default())
        .await
        .map(|_| jar_path(cache, software))
}

/// Download (or with `force`, refresh) a cache entry. Concurrent callers for
/// the same entry share one download; the jar is replaced atomically.
pub async fn download(
    cache: &Path,
    software: &Software,
    force: bool,
    endpoints: &Endpoints,
) -> io::Result<Value> {
    software.validate().map_err(invalid)?;
    let directory = cache.join(software.key());
    let lock = entry_lock(&directory);
    let _guard = lock.lock().await;
    if !force && directory.join(JAR).is_file() {
        return Ok(status(cache, software));
    }
    if *cancellation().borrow() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "software downloads are cancelled during shutdown",
        ));
    }
    let client = client()?;
    let resolved = resolve(&client, software, endpoints).await?;
    tokio::fs::create_dir_all(&directory).await?;
    // A killed process cannot clean up after itself; this entry's lock is held.
    let mut entries = tokio::fs::read_dir(&directory).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_name().to_string_lossy().contains(".part-") {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
    let partial = directory.join(format!(".{JAR}.part-{}", std::process::id()));
    let (size, sha256) = fetch(&client, &resolved, &partial).await?;
    let metadata = json!({
        "format": 1,
        "resolved_version": resolved.version,
        "build": resolved.build,
        "url": redact(&resolved.url),
        "size": size,
        "sha256": sha256,
        "downloaded_at": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
    });
    if let Err(error) = tokio::fs::rename(&partial, directory.join(JAR)).await {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(error);
    }
    let staged = directory.join(format!(".{METADATA}.part-{}", std::process::id()));
    tokio::fs::write(&staged, serde_json::to_vec_pretty(&metadata)?).await?;
    tokio::fs::rename(staged, directory.join(METADATA)).await?;
    Ok(status(cache, software))
}

/// Interrupt active and future downloads; called once Rift starts shutting down.
pub fn cancel_downloads() {
    cancellation().send_replace(true);
}

fn cancellation() -> &'static tokio::sync::watch::Sender<bool> {
    static CANCEL: OnceLock<tokio::sync::watch::Sender<bool>> = OnceLock::new();
    CANCEL.get_or_init(|| tokio::sync::watch::channel(false).0)
}

fn entry_lock(directory: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS.get_or_init(Default::default).lock().unwrap();
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(directory).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(directory.to_owned(), Arc::downgrade(&lock));
    lock
}

fn client() -> io::Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if check_url(attempt.url().as_str()).is_err() {
                attempt.error("redirect to an insecure URL")
            } else {
                attempt.follow()
            }
        }))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .user_agent(concat!(
            "Rift/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/Jotrorox/rift)"
        ))
        .build()
        .map_err(|_| io::Error::other("could not initialize the software download client"))
}

/// HTTPS everywhere; plain HTTP only for loopback mirrors and tests.
fn check_url(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "expected an absolute URL".to_owned())?;
    if url.len() > 2048 {
        return Err("URL exceeds 2048 bytes".into());
    }
    match parsed.scheme() {
        "https" => Ok(()),
        "http"
            if parsed.host_str().is_some_and(|host| {
                host.eq_ignore_ascii_case("localhost")
                    || host
                        .trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            }) =>
        {
            Ok(())
        }
        _ => Err("expected an https:// URL (http:// only for loopback hosts)".into()),
    }
}

async fn get_json(client: &reqwest::Client, url: &str) -> io::Result<Value> {
    check_url(url).map_err(invalid)?;
    let send = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(Duration::from_secs(30))
        .send();
    let mut response = cancellable(send)
        .await?
        .map_err(|error| upstream(url, error))?;
    if !response.status().is_success() {
        return Err(io::Error::other(format!(
            "{} returned HTTP {}",
            redact(url),
            response.status().as_u16()
        )));
    }
    let mut body = Vec::new();
    while let Some(chunk) = cancellable(response.chunk())
        .await?
        .map_err(|error| upstream(url, error))?
    {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_METADATA {
            return Err(io::Error::other(format!(
                "{} returned more than 8 MiB of metadata",
                redact(url)
            )));
        }
    }
    serde_json::from_slice(&body)
        .map_err(|_| io::Error::other(format!("{} returned invalid JSON", redact(url))))
}

async fn cancellable<T>(work: impl Future<Output = T>) -> io::Result<T> {
    let mut cancelled = cancellation().subscribe();
    tokio::select! {
        result = work => Ok(result),
        _ = cancelled.wait_for(|cancelled| *cancelled) => Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "software download cancelled during shutdown",
        )),
    }
}

/// Stream the artifact to `partial`, verifying size and published digests.
async fn fetch(
    client: &reqwest::Client,
    resolved: &Resolved,
    partial: &Path,
) -> io::Result<(u64, String)> {
    check_url(&resolved.url).map_err(invalid)?;
    let mut response = cancellable(client.get(&resolved.url).send())
        .await?
        .map_err(|error| upstream(&resolved.url, error))?;
    if !response.status().is_success() {
        return Err(io::Error::other(format!(
            "{} returned HTTP {}",
            redact(&resolved.url),
            response.status().as_u16()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DOWNLOAD)
    {
        return Err(io::Error::other("server jar exceeds 512 MiB"));
    }
    let mut file = tokio::fs::File::create(partial).await?;
    let mut sha256 = Sha256::new();
    let mut sha1 = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY);
    let mut size = 0u64;
    let result = async {
        while let Some(chunk) = cancellable(response.chunk())
            .await?
            .map_err(|error| upstream(&resolved.url, error))?
        {
            size += chunk.len() as u64;
            if size > MAX_DOWNLOAD {
                return Err(io::Error::other("server jar exceeds 512 MiB"));
            }
            sha256.update(&chunk);
            sha1.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.sync_all().await?;
        let sha256 = hex(&sha256.finalize());
        if let Some(expected) = &resolved.sha256
            && !expected.eq_ignore_ascii_case(&sha256)
        {
            return Err(io::Error::other(format!(
                "SHA-256 mismatch for {}: expected {expected}, received {sha256}",
                redact(&resolved.url)
            )));
        }
        let sha1 = hex(sha1.finish().as_ref());
        if let Some(expected) = &resolved.sha1
            && !expected.eq_ignore_ascii_case(&sha1)
        {
            return Err(io::Error::other(format!(
                "SHA-1 mismatch for {}: expected {expected}, received {sha1}",
                redact(&resolved.url)
            )));
        }
        if size < 4 {
            return Err(io::Error::other("downloaded server jar is empty"));
        }
        Ok((size, sha256))
    }
    .await;
    drop(file);
    if result.is_err() {
        let _ = tokio::fs::remove_file(partial).await;
    }
    result
}

/// Select a concrete build for `software` without downloading it.
pub async fn resolve(
    client: &reqwest::Client,
    software: &Software,
    endpoints: &Endpoints,
) -> io::Result<Resolved> {
    let version = if software.version == "latest" {
        latest(client, software.kind, endpoints).await?
    } else {
        software.version.clone()
    };
    match software.kind {
        Kind::Paper | Kind::Folia => {
            let project = software.kind.as_str();
            let url = format!(
                "{}/v3/projects/{project}/versions/{version}/builds/latest",
                endpoints.fill
            );
            let build = get_json(client, &url).await?;
            let download = build["downloads"]["server:default"]
                .as_object()
                .or_else(|| build["downloads"]["server:mojang"].as_object())
                .ok_or_else(|| {
                    io::Error::other(format!("no {project} {version} server download available"))
                })?;
            Ok(Resolved {
                url: text(download.get("url"), "download URL")?,
                version,
                build: build["id"].as_u64().map(|id| id.to_string()),
                sha256: download
                    .get("checksums")
                    .and_then(|checksums| checksums["sha256"].as_str())
                    .map(str::to_owned),
                sha1: None,
            })
        }
        Kind::Purpur => {
            let build = get_json(
                client,
                &format!("{}/v2/purpur/{version}/latest", endpoints.purpur),
            )
            .await?;
            let number = text(build.get("build"), "Purpur build")?;
            Ok(Resolved {
                url: format!("{}/v2/purpur/{version}/{number}/download", endpoints.purpur),
                version,
                build: Some(number),
                sha256: None,
                sha1: None,
            })
        }
        Kind::Vanilla => {
            let manifest = get_json(
                client,
                &format!("{}/mc/game/version_manifest_v2.json", endpoints.mojang),
            )
            .await?;
            let entry = manifest["versions"]
                .as_array()
                .and_then(|versions| {
                    versions
                        .iter()
                        .find(|entry| entry["id"] == version.as_str())
                })
                .ok_or_else(|| io::Error::other(format!("unknown Minecraft version {version}")))?;
            let details = get_json(client, &text(entry.get("url"), "version URL")?).await?;
            let server = &details["downloads"]["server"];
            Ok(Resolved {
                url: text(server.get("url"), "server download URL").map_err(|_| {
                    io::Error::other(format!("Minecraft {version} has no server download"))
                })?,
                version,
                build: None,
                sha256: None,
                sha1: server["sha1"].as_str().map(str::to_owned),
            })
        }
        Kind::Fabric => {
            let stable = |value: Value, what: &str| -> io::Result<String> {
                value
                    .as_array()
                    .and_then(|entries| entries.iter().find(|entry| entry["stable"] == true))
                    .and_then(|entry| entry["version"].as_str())
                    .map(str::to_owned)
                    .ok_or_else(|| io::Error::other(format!("no stable Fabric {what} available")))
            };
            let loader = stable(
                get_json(client, &format!("{}/v2/versions/loader", endpoints.fabric)).await?,
                "loader",
            )?;
            let installer = stable(
                get_json(
                    client,
                    &format!("{}/v2/versions/installer", endpoints.fabric),
                )
                .await?,
                "installer",
            )?;
            Ok(Resolved {
                url: format!(
                    "{}/v2/versions/loader/{version}/{loader}/{installer}/server/jar",
                    endpoints.fabric
                ),
                version,
                build: Some(format!("loader {loader}, installer {installer}")),
                sha256: None,
                sha1: None,
            })
        }
        Kind::Url => Ok(Resolved {
            url: software
                .url
                .clone()
                .ok_or_else(|| invalid("url is required"))?,
            version: String::new(),
            build: None,
            sha256: software.sha256.clone(),
            sha1: None,
        }),
    }
}

async fn latest(client: &reqwest::Client, kind: Kind, endpoints: &Endpoints) -> io::Result<String> {
    let releases = list(client, kind, endpoints).await?;
    releases
        .into_iter()
        .next()
        .ok_or_else(|| io::Error::other(format!("no {} releases available", kind.as_str())))
}

/// Release names, newest first, for version pickers.
pub async fn versions(kind: Kind) -> io::Result<Vec<String>> {
    type Cached = HashMap<Kind, (Instant, Vec<String>)>;
    static CACHE: OnceLock<Mutex<Cached>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some((fetched, versions)) = cache.lock().unwrap().get(&kind)
        && fetched.elapsed() < VERSION_CACHE
    {
        return Ok(versions.clone());
    }
    let versions = list(&client()?, kind, &Endpoints::default()).await?;
    cache
        .lock()
        .unwrap()
        .insert(kind, (Instant::now(), versions.clone()));
    Ok(versions)
}

async fn list(
    client: &reqwest::Client,
    kind: Kind,
    endpoints: &Endpoints,
) -> io::Result<Vec<String>> {
    let mut versions: Vec<String> = match kind {
        Kind::Paper | Kind::Folia => {
            let project = get_json(
                client,
                &format!("{}/v3/projects/{}", endpoints.fill, kind.as_str()),
            )
            .await?;
            let mut versions: Vec<String> = project["versions"]
                .as_object()
                .into_iter()
                .flat_map(|groups| groups.values())
                .filter_map(Value::as_array)
                .flatten()
                .filter_map(Value::as_str)
                .filter(|version| !version.contains('-'))
                .map(str::to_owned)
                .collect();
            versions.sort_by(|left, right| compare(right, left));
            versions
        }
        Kind::Purpur => {
            let project = get_json(client, &format!("{}/v2/purpur", endpoints.purpur)).await?;
            let mut versions: Vec<String> = project["versions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            versions.sort_by(|left, right| compare(right, left));
            versions
        }
        Kind::Vanilla => get_json(
            client,
            &format!("{}/mc/game/version_manifest_v2.json", endpoints.mojang),
        )
        .await?["versions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|entry| entry["type"] == "release")
            .filter_map(|entry| entry["id"].as_str().map(str::to_owned))
            .collect(),
        Kind::Fabric => get_json(client, &format!("{}/v2/versions/game", endpoints.fabric))
            .await?
            .as_array()
            .into_iter()
            .flatten()
            .filter(|entry| entry["stable"] == true)
            .filter_map(|entry| entry["version"].as_str().map(str::to_owned))
            .collect(),
        Kind::Url => Vec::new(),
    };
    versions.retain(|version| valid_version(version) && version != "latest");
    versions.dedup();
    versions.truncate(MAX_VERSIONS);
    Ok(versions)
}

/// Numeric dotted-version order; non-numeric parts compare as text.
fn compare(left: &str, right: &str) -> std::cmp::Ordering {
    let parts = |value: &str| -> Vec<(u64, String)> {
        value
            .split(['.', '-'])
            .map(|part| (part.parse().unwrap_or(0), part.to_owned()))
            .collect()
    };
    parts(left).cmp(&parts(right))
}

fn text(value: Option<&Value>, what: &str) -> io::Result<String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other(format!("upstream response is missing the {what}")))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Query strings can carry tokens for private mirrors; keep them out of errors.
fn redact(url: &str) -> String {
    url.split(['?', '#']).next().unwrap_or_default().to_owned()
}

fn upstream(url: &str, error: reqwest::Error) -> io::Error {
    let kind = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "connection failed"
    } else if error.is_redirect() {
        "redirect rejected"
    } else {
        "request failed"
    };
    io::Error::other(format!("{}: {kind}", redact(url)))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests;
