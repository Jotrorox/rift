//! Template folders kept in a git repository.
//!
//! A source names a repository and a branch, tag or pinned commit. Syncing
//! it exports that commit's files to `template_storage/<name>`, where
//! templates use them like any other folder: as `configs` layers, `plugins`,
//! a `map` or a `server_jar`, on this host and on nodes alike.
//!
//! The checkout contains the files only. The repository itself lives in the
//! software cache as `git-<name>`, so no `.git` directory is ever copied
//! into an instance or served to a node. A sync builds the new tree next to
//! the old one and swaps them, so a failed fetch leaves the previous
//! checkout in place.
//!
//! Rift runs the host's `git`, with the operator's own configuration and
//! credentials (SSH keys, credential helpers) but without Rift's secrets in
//! its environment. Symbolic links in the repository are written as plain
//! files, since provisioning rejects links, and submodules are not fetched.
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const RECORD: &str = "rift-source.json";
/// A fetch or checkout that takes longer than this is stuck.
const STEP_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// `{ git = "https://…", ref = "main" }` or `{ git = …, rev = "<commit>" }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateSource {
    pub url: String,
    /// A branch or tag; `None` with no `revision` follows the remote's default branch.
    pub reference: Option<String>,
    /// A full commit id the checkout must be.
    pub revision: Option<String>,
}

impl TemplateSource {
    /// Checks everything that reaches a `git` command line. Errors start
    /// with the offending field.
    pub fn validate(&self) -> Result<(), String> {
        let url = &self.url;
        let scp_like = || {
            // user@host:path, the usual SSH form.
            url.split_once(':').is_some_and(|(host, path)| {
                !path.is_empty()
                    && !path.starts_with('/')
                    && host.split_once('@').is_some_and(|(user, host)| {
                        !user.is_empty() && !host.is_empty() && !host.contains('/')
                    })
            })
        };
        if url.is_empty()
            || url.len() > 2048
            || url.starts_with('-')
            || url.chars().any(|c| c.is_control() || c.is_whitespace())
            || !(url.starts_with("https://")
                || url.starts_with("ssh://")
                || url.starts_with("file://")
                || scp_like())
        {
            return Err(
                "git: expected an https://, ssh://, user@host:path or file:// repository URL"
                    .into(),
            );
        }
        if let Some((_, rest)) = url.split_once("://")
            && let Some((userinfo, _)) = rest.split('/').next().unwrap_or_default().rsplit_once('@')
            && userinfo.contains(':')
        {
            return Err(
                "git: do not put a password or token in the URL; use an SSH key or a git credential helper"
                    .into(),
            );
        }
        if self.reference.is_some() && self.revision.is_some() {
            return Err("ref: set either ref or rev, not both".into());
        }
        if let Some(reference) = &self.reference
            && (reference.is_empty()
                || reference.len() > 255
                || reference.starts_with(['-', '/'])
                || reference.ends_with(['/', '.'])
                || reference.contains("..")
                || reference.contains("//")
                || !reference
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._/-+".contains(&b)))
        {
            return Err("ref: expected a branch or tag name".into());
        }
        if let Some(revision) = &self.revision
            && (!matches!(revision.len(), 40 | 64)
                || !revision
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        {
            return Err("rev: expected a full lowercase commit id (40 or 64 hex digits)".into());
        }
        Ok(())
    }

    /// What a record must match for its checkout to still be the configured one.
    fn spec(&self) -> Value {
        json!({"url": self.url, "ref": self.reference, "rev": self.revision})
    }
}

/// The folder templates refer to.
pub fn checkout_path(storage: &Path, name: &str) -> PathBuf {
    storage.join(name)
}

fn repository_path(cache: &Path, name: &str) -> PathBuf {
    cache.join(format!("git-{name}"))
}

/// A URL without the user name, for listings and errors.
fn redact(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
            let host = authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host);
            format!("{scheme}://{host}/{path}")
        }
        None => url.to_owned(),
    }
}

fn record(cache: &Path, name: &str) -> Option<Value> {
    let bytes = std::fs::read(repository_path(cache, name).join(RECORD)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Whether the checkout exists and was made from this definition.
fn current(storage: &Path, cache: &Path, name: &str, source: &TemplateSource) -> bool {
    checkout_path(storage, name).is_dir()
        && record(cache, name).is_some_and(|record| record["spec"] == source.spec())
}

/// Checkout state for listings; never contacts the repository.
pub fn status(storage: &Path, cache: &Path, name: &str, source: &TemplateSource) -> Value {
    let mut value = json!({
        "name": name,
        "git": redact(&source.url),
        "ref": source.reference,
        "rev": source.revision,
        "synced": current(storage, cache, name, source),
    });
    if let Some(record) = record(cache, name) {
        for key in ["commit", "synced_at"] {
            if let Some(field) = record.get(key) {
                value[key] = field.clone();
            }
        }
    }
    value
}

fn entry_lock(directory: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(directory).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(directory.to_owned(), Arc::downgrade(&lock));
    lock
}

/// Run `git` in `repository` and return its trimmed standard output.
async fn git(repository: &Path, arguments: &[&str]) -> io::Result<String> {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("--git-dir")
        .arg(repository)
        // Byte-exact files on every platform, links as plain files, and
        // never the one transport that runs an arbitrary command.
        .args([
            "-c",
            "core.symlinks=false",
            "-c",
            "core.autocrlf=false",
            "-c",
            "protocol.ext.allow=never",
            "-c",
            "gc.auto=0",
        ])
        .args(arguments)
        // A missing credential fails instead of waiting for a terminal.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for name in crate::environment::protected() {
        command.env_remove(name);
    }
    let output = tokio::time::timeout(STEP_TIMEOUT, command.output())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "git timed out"))?
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "git is not installed (or not on Rift's PATH)",
                )
            } else {
                error
            }
        })?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        // The end of git's output is where it says what went wrong.
        let lines: Vec<_> = message
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        let tail = lines[lines.len().saturating_sub(3)..].join("; ");
        let tail: String = tail.chars().take(400).collect();
        return Err(io::Error::other(format!(
            "git {} failed: {tail}",
            arguments.first().copied().unwrap_or_default()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Fetch the configured commit and replace the checkout with its files.
/// Returns the new [`status`]. In a cluster the leader runs the sync (it
/// is forwarded there) and publishes the folder's manifest, from which every
/// peer's `template_storage` is written (docs/clustering.md, "Templates and
/// other files").
pub async fn sync(
    storage: &Path,
    cache: &Path,
    name: &str,
    source: &TemplateSource,
) -> io::Result<Value> {
    match crate::cluster::files::active() {
        Some(files) => files.sync_template_source(name, source, false).await,
        None => sync_local(storage, cache, name, source).await,
    }
}

/// [`sync`] on this host's own folders, whatever the cluster.
pub(crate) async fn sync_local(
    storage: &Path,
    cache: &Path,
    name: &str,
    source: &TemplateSource,
) -> io::Result<Value> {
    source
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let repository = repository_path(cache, name);
    let lock = entry_lock(&repository);
    let _guard = lock.lock().await;
    tokio::fs::create_dir_all(&repository).await?;
    if !repository.join("HEAD").is_file() {
        git(&repository, &["init", "--quiet", "--bare"]).await?;
    }
    let wanted = source
        .revision
        .as_deref()
        .or(source.reference.as_deref())
        .unwrap_or("HEAD");
    git(
        &repository,
        &[
            "fetch",
            "--quiet",
            "--force",
            "--no-tags",
            "--depth",
            "1",
            "--",
            &source.url,
            wanted,
        ],
    )
    .await
    .map_err(|error| io::Error::other(format!("{}: {error}", redact(&source.url))))?;
    let commit = git(
        &repository,
        &["rev-parse", "--verify", "--quiet", "FETCH_HEAD^{commit}"],
    )
    .await?;
    if let Some(revision) = &source.revision
        && *revision != commit
    {
        return Err(io::Error::other(format!(
            "{} sent commit {commit} instead of the pinned {revision}",
            redact(&source.url)
        )));
    }
    tokio::fs::create_dir_all(storage).await?;
    let checkout = checkout_path(storage, name);
    let sibling = |tag: &str| storage.join(format!(".{name}.{tag}-{}", std::process::id()));
    let (staging, previous) = (sibling("sync"), sibling("old"));
    for leftover in [&staging, &previous] {
        match tokio::fs::remove_dir_all(leftover).await {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    tokio::fs::create_dir(&staging).await?;
    let exported = async {
        let work_tree = staging
            .to_str()
            .ok_or_else(|| io::Error::other("template_storage must be a UTF-8 path"))?;
        git(
            &repository,
            &[
                "--work-tree",
                work_tree,
                "reset",
                "--quiet",
                "--hard",
                &commit,
            ],
        )
        .await?;
        let had_checkout = match tokio::fs::rename(&checkout, &previous).await {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        if let Err(error) = tokio::fs::rename(&staging, &checkout).await {
            if had_checkout {
                let _ = tokio::fs::rename(&previous, &checkout).await;
            }
            return Err(error);
        }
        Ok(())
    }
    .await;
    if let Err(error) = exported {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return Err(error);
    }
    let _ = tokio::fs::remove_dir_all(&previous).await;
    let record = json!({
        "spec": source.spec(),
        "commit": commit,
        "synced_at": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
    });
    tokio::fs::write(repository.join(RECORD), record.to_string()).await?;
    Ok(status(storage, cache, name, source))
}

/// Sync a source that has no checkout yet, or whose definition changed since
/// its checkout was made. An existing checkout of a branch is left alone:
/// instances never change because someone pushed.
/// In a cluster the leader decides and publishes, as for [`sync`].
pub async fn ensure(
    storage: &Path,
    cache: &Path,
    name: &str,
    source: &TemplateSource,
) -> io::Result<()> {
    match crate::cluster::files::active() {
        Some(files) => files
            .sync_template_source(name, source, true)
            .await
            .map(drop),
        None => ensure_local(storage, cache, name, source).await.map(drop),
    }
}

/// [`ensure`] on this host's own folders; returns whether it synced.
pub(crate) async fn ensure_local(
    storage: &Path,
    cache: &Path,
    name: &str,
    source: &TemplateSource,
) -> io::Result<bool> {
    if current(storage, cache, name, source) {
        return Ok(false);
    }
    sync_local(storage, cache, name, source).await.map(|_| true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(url: &str, reference: Option<&str>, revision: Option<&str>) -> TemplateSource {
        TemplateSource {
            url: url.into(),
            reference: reference.map(Into::into),
            revision: revision.map(Into::into),
        }
    }

    #[test]
    fn only_safe_urls_and_names_reach_the_command_line() {
        for url in [
            "https://github.com/example/templates.git",
            "ssh://git@example.com/templates.git",
            "git@example.com:example/templates.git",
            "https://deploy@example.com/templates.git",
            "file:///srv/git/templates.git",
        ] {
            assert_eq!(source(url, Some("main"), None).validate(), Ok(()), "{url}");
        }
        for url in [
            "",
            "http://example.com/templates.git",
            "git://example.com/templates.git",
            "ext::sh -c id",
            "--upload-pack=id",
            "/srv/git/templates.git",
            "https://example.com/a b.git",
            "https://user:token@example.com/templates.git",
        ] {
            let error = source(url, None, None).validate().unwrap_err();
            assert!(error.starts_with("git:"), "{url}: {error}");
        }
        for reference in ["", "-x", "a..b", "/main", "main/", "a b", "x.", "a//b"] {
            let error = source("https://example.com/t.git", Some(reference), None)
                .validate()
                .unwrap_err();
            assert!(error.starts_with("ref:"), "{reference}: {error}");
        }
        let commit = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            source("https://example.com/t.git", None, Some(commit)).validate(),
            Ok(())
        );
        for revision in ["main", "0123456", &commit.to_uppercase()] {
            assert!(
                source("https://example.com/t.git", None, Some(revision))
                    .validate()
                    .unwrap_err()
                    .starts_with("rev:"),
                "{revision}"
            );
        }
        assert!(
            source("https://example.com/t.git", Some("main"), Some(commit))
                .validate()
                .is_err()
        );
        assert_eq!(
            redact("https://deploy@example.com/org/t.git"),
            "https://example.com/org/t.git"
        );
    }

    fn run(directory: &Path, arguments: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(directory)
            .args([
                "-c",
                "user.name=Rift",
                "-c",
                "user.email=rift@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[tokio::test]
    async fn a_sync_exports_the_files_and_later_syncs_replace_them() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: git is not installed");
            return;
        }
        let root =
            std::env::temp_dir().join(format!("rift-template-source-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let origin = root.join("origin");
        std::fs::create_dir_all(origin.join("Lobby/default")).unwrap();
        run(&origin, &["init", "--quiet"]);
        // Hosting services serve any reachable commit; plain git must be told to.
        run(
            &origin,
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
        std::fs::write(origin.join("Lobby/default/server.properties"), "motd=one\n").unwrap();
        std::fs::write(origin.join("removed.txt"), "x").unwrap();
        run(&origin, &["add", "."]);
        run(&origin, &["commit", "--quiet", "-m", "one"]);
        let first = run(&origin, &["rev-parse", "HEAD"]);
        // On Windows the path has a drive and backslashes.
        let url = format!(
            "file://{}{}",
            if cfg!(windows) { "/" } else { "" },
            origin.to_string_lossy().replace('\\', "/")
        );
        let (storage, cache) = (root.join("templates"), root.join("cache"));
        let branch = source(&url, Some("main"), None);

        let status = sync(&storage, &cache, "network", &branch).await.unwrap();
        assert_eq!(status["commit"], first.as_str());
        assert_eq!(status["synced"], true);
        let checkout = checkout_path(&storage, "network");
        let properties = checkout.join("Lobby/default/server.properties");
        assert_eq!(std::fs::read_to_string(&properties).unwrap(), "motd=one\n");
        // Only the files: nothing of git's ends up in instances.
        assert!(!checkout.join(".git").exists());

        std::fs::write(origin.join("Lobby/default/server.properties"), "motd=two\n").unwrap();
        std::fs::remove_file(origin.join("removed.txt")).unwrap();
        run(&origin, &["commit", "--quiet", "-am", "two"]);
        // A branch is not followed until someone asks.
        ensure(&storage, &cache, "network", &branch).await.unwrap();
        assert_eq!(std::fs::read_to_string(&properties).unwrap(), "motd=one\n");
        sync(&storage, &cache, "network", &branch).await.unwrap();
        assert_eq!(std::fs::read_to_string(&properties).unwrap(), "motd=two\n");
        assert!(!checkout.join("removed.txt").exists());

        // Changing the definition is asking: the pin applies on next use.
        let pinned = source(&url, None, Some(&first));
        assert_eq!(status_of(&storage, &cache, &pinned)["synced"], false);
        ensure(&storage, &cache, "network", &pinned).await.unwrap();
        assert_eq!(std::fs::read_to_string(&properties).unwrap(), "motd=one\n");
        assert!(checkout.join("removed.txt").exists());

        // A failed sync keeps the checkout that was there.
        let missing = source(&url, Some("no-such-branch"), None);
        let error = sync(&storage, &cache, "network", &missing)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("git fetch failed"), "{error}");
        assert_eq!(std::fs::read_to_string(&properties).unwrap(), "motd=one\n");
        let leftovers: Vec<_> = std::fs::read_dir(&storage)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, ["network"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn status_of(storage: &Path, cache: &Path, source: &TemplateSource) -> Value {
        status(storage, cache, "network", source)
    }
}
