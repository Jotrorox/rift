//! `rift admin move`: copy a stopped server's directory from one node agent
//! to another, through the controller.
//!
//! The controller lists the source directory page by page
//! ([`Command::Tree`]), reads each file in chunks of [`FILE_PART`] bytes
//! ([`Command::Read`]) and writes them into a staging folder on the target
//! ([`Command::Receive`]). Agents never talk to each other, so a move needs no
//! listener on the agents and no credentials beyond their bus sessions. The
//! target checks every file against the SHA-256 of the bytes read on the
//! source. The staging folder becomes the server directory only when the
//! controller commits the move, and the source is only deleted after that.
//!
//! An interrupted copy is resumed: with [`feature::MOVE_RESUME`] on the
//! target, a failed copy keeps its staging folder, and the next move of the
//! same server into that node starts with [`ReceiveStep::Resume`]. The agent
//! sets aside what the earlier attempt left as a [`resumable`] folder, and
//! [`ReceiveStep::Have`] takes over every file that arrived whole, which the
//! agent recognises by its size and the modification time of the source
//! file. Everything else is sent again; what the earlier attempt left that
//! the source no longer has is deleted on commit.
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, UNIX_EPOCH},
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    Nodes,
    protocol::{
        self, Command, FILE_PART, FileEnd, MOVE_MARKER, Receive, ReceiveStep, Tree, TreeEntry,
        TreeTotal, feature,
    },
};

/// The longest one move command may take; a chunk is at most 512 KiB.
const WAIT: Duration = Duration::from_secs(60);
/// Tree pages stay well below the 1 MiB message limit.
const PAGE_BYTES: usize = 256 * 1024;
const PAGE_ENTRIES: usize = 2048;

/// What [`copy`] transferred, also while it runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Copied {
    /// Files sent, and their bytes; `bytes` also counts the file being sent.
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
    /// Files that an interrupted earlier attempt already put on the target.
    pub reused_files: u64,
    pub reused_bytes: u64,
    /// The whole folder, when the source agent reports it
    /// ([`feature::MOVE_RESUME`]).
    pub total: Option<TreeTotal>,
}

impl Copied {
    /// Whether anything reached the target that a later attempt can reuse.
    pub fn landed(&self) -> bool {
        self.files > 0 || self.reused_files > 0
    }
}

/// [`copy`] updates this while it runs, for progress reports.
pub type Progress = Mutex<Copied>;

fn update(progress: &Progress, change: impl FnOnce(&mut Copied)) -> Copied {
    let mut copied = progress.lock().unwrap_or_else(|error| error.into_inner());
    change(&mut copied);
    *copied
}

/// A failed [`copy`].
#[derive(Debug)]
pub struct CopyError {
    pub error: io::Error,
    /// The staging folder stays on the target, so moving the same server
    /// into that node again resumes the copy.
    pub kept: bool,
}

impl std::fmt::Display for CopyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

/// Whether a copy into `node` can be resumed after an interruption.
pub fn resumes(nodes: &Nodes, node: &str) -> bool {
    nodes.supports(node, feature::MOVE_RESUME)
}

/// The staging folder of a move into `directory`, next to it below the
/// agent's root.
pub fn staging(server: &str, directory: &Path) -> PathBuf {
    directory
        .parent()
        .unwrap_or(Path::new(""))
        .join(format!(".rift-move-{server}"))
}

/// Copy `directory` of the stopped `server` from node `from` into a staging
/// folder on node `to`, resuming what an interrupted earlier copy left there
/// when `to` [`resumes`]. [`commit`] puts it in place. On failure the
/// staging folder is kept for the next attempt if `to` resumes and anything
/// arrived, and deleted otherwise. `progress` follows the copy as it runs.
pub async fn copy(
    nodes: &Nodes,
    server: &str,
    from: &str,
    to: &str,
    directory: &Path,
    progress: &Progress,
) -> Result<Copied, CopyError> {
    let resume = resumes(nodes, to);
    match copy_tree(nodes, server, from, to, directory, resume, progress).await {
        Ok(copied) => Ok(copied),
        Err(error) => {
            let kept = resume && update(progress, |_| {}).landed();
            if !kept {
                abort(nodes, server, to, directory).await;
            }
            Err(CopyError { error, kept })
        }
    }
}

async fn copy_tree(
    nodes: &Nodes,
    server: &str,
    from: &str,
    to: &str,
    directory: &Path,
    resume: bool,
    progress: &Progress,
) -> io::Result<Copied> {
    let start = if resume {
        ReceiveStep::Resume
    } else {
        ReceiveStep::Begin
    };
    receive(nodes, to, server, directory, start).await?;
    let mut offset = 0;
    loop {
        let reply = nodes
            .request(
                from,
                &Command::Tree {
                    server: server.into(),
                    directory: directory.into(),
                    offset,
                },
                WAIT,
            )
            .await?;
        let page: Tree = serde_json::from_value(reply)
            .map_err(|error| io::Error::other(format!("invalid tree from node {from}: {error}")))?;
        if let Some(total) = page.total {
            update(progress, |copied| copied.total = Some(total));
        }
        // The target creates the page's folders and keeps the files that
        // an earlier attempt already received.
        let missing = if resume {
            let entries = page.entries.clone();
            let reply =
                receive(nodes, to, server, directory, ReceiveStep::Have { entries }).await?;
            let missing = reply["missing"]
                .as_array()
                .ok_or_else(|| io::Error::other(format!("node {to} sent an invalid have reply")))?
                .iter()
                .map(|index| index.as_u64().map(|index| index as usize))
                .collect::<Option<BTreeSet<_>>>()
                .ok_or_else(|| io::Error::other(format!("node {to} sent an invalid have reply")))?;
            Some(missing)
        } else {
            None
        };
        for (index, entry) in page.entries.into_iter().enumerate() {
            if entry.folder {
                if missing.is_none() {
                    let path = entry.path;
                    receive(nodes, to, server, directory, ReceiveStep::Folder { path }).await?;
                }
                update(progress, |copied| copied.folders += 1);
            } else if missing
                .as_ref()
                .is_some_and(|missing| !missing.contains(&index))
            {
                update(progress, |copied| {
                    copied.reused_files += 1;
                    copied.reused_bytes += entry.size;
                });
            } else {
                copy_file(nodes, server, (from, to), directory, &entry, progress).await?;
                update(progress, |copied| copied.files += 1);
            }
        }
        match page.next {
            None => return Ok(update(progress, |_| {})),
            Some(next) if next > offset => offset = next,
            Some(_) => {
                return Err(io::Error::other(format!(
                    "node {from} returned an invalid tree page"
                )));
            }
        }
    }
}

/// Relay one file chunk by chunk, reading the next chunk while the previous
/// one is written.
async fn copy_file(
    nodes: &Nodes,
    server: &str,
    (from, to): (&str, &str),
    directory: &Path,
    entry: &TreeEntry,
    progress: &Progress,
) -> io::Result<()> {
    let changed = || {
        io::Error::other(format!(
            "{} changed on node {from} during the move",
            entry.path
        ))
    };
    let mut digest = Sha256::new();
    let mut offset = 0;
    let mut chunk = match entry.size {
        0 => None,
        _ => Some(read(nodes, from, server, directory, &entry.path, 0).await?),
    };
    loop {
        let (content, length) = match chunk.take() {
            None => (String::new(), 0),
            Some((content, size)) => {
                let bytes = protocol::decode_base64(&content).ok_or_else(|| {
                    io::Error::other(format!("node {from} sent invalid file contents"))
                })?;
                if size != entry.size || bytes.is_empty() {
                    return Err(changed());
                }
                digest.update(&bytes);
                (content, bytes.len() as u64)
            }
        };
        let next = offset + length;
        if next > entry.size {
            return Err(changed());
        }
        let end = (next == entry.size).then(|| FileEnd {
            sha256: hex(&digest.clone().finalize()),
            mode: entry.mode,
            modified: entry.modified,
        });
        let last = end.is_some();
        let step = ReceiveStep::Chunk {
            path: entry.path.clone(),
            offset,
            content,
            end,
        };
        let written = receive(nodes, to, server, directory, step);
        if last {
            written.await?;
            update(progress, |copied| copied.bytes += length);
            return Ok(());
        }
        let (written, following) = tokio::join!(
            written,
            read(nodes, from, server, directory, &entry.path, next)
        );
        written?;
        update(progress, |copied| copied.bytes += length);
        chunk = Some(following?);
        offset = next;
    }
}

async fn read(
    nodes: &Nodes,
    node: &str,
    server: &str,
    directory: &Path,
    path: &str,
    offset: u64,
) -> io::Result<(String, u64)> {
    let command = Command::Read {
        server: server.into(),
        directory: directory.into(),
        path: path.into(),
        offset,
        snapshot: false,
    };
    let mut reply = nodes.request(node, &command, WAIT).await?;
    match (reply["content"].take(), reply["size"].as_u64()) {
        (Value::String(content), Some(size)) => Ok((content, size)),
        _ => Err(io::Error::other(format!(
            "node {node} sent an invalid read reply"
        ))),
    }
}

async fn receive(
    nodes: &Nodes,
    node: &str,
    server: &str,
    directory: &Path,
    step: ReceiveStep,
) -> io::Result<Value> {
    send_step(nodes, node, server, directory, step, false).await
}

/// One [`Command::Receive`] step on `node`. `replace` is for a restore in
/// place: the existing directory is swapped out on commit.
pub async fn send_step(
    nodes: &Nodes,
    node: &str,
    server: &str,
    directory: &Path,
    step: ReceiveStep,
    replace: bool,
) -> io::Result<Value> {
    let command = Command::Receive(Receive {
        server: server.into(),
        directory: directory.into(),
        step,
        replace,
    });
    nodes.request(node, &command, WAIT).await
}

/// Rename the staging folder on `to` to the server directory. Repeating a
/// commit that already happened succeeds.
pub async fn commit(nodes: &Nodes, server: &str, to: &str, directory: &Path) -> io::Result<()> {
    receive(nodes, to, server, directory, ReceiveStep::Commit)
        .await
        .map(|_| ())
}

/// The move is recorded: the directory on `to` is no longer an orphan that a
/// later move may replace.
pub async fn finish(nodes: &Nodes, server: &str, to: &str, directory: &Path) -> io::Result<()> {
    receive(nodes, to, server, directory, ReceiveStep::Finish)
        .await
        .map(|_| ())
}

/// Delete a folder that [`commit`] put in place on `to` but the controller
/// could not record; best effort, like [`abort`].
pub async fn discard_orphan(nodes: &Nodes, server: &str, to: &str, directory: &Path) {
    // Beginning anew replaces the unrecorded folder with an empty staging
    // folder, which aborting then deletes.
    if receive(nodes, to, server, directory, ReceiveStep::Begin)
        .await
        .is_ok()
    {
        abort(nodes, server, to, directory).await;
    }
}

/// Take back a folder that [`commit`] put in place on `to` but the
/// controller could not record: kept to resume from when `to` [`resumes`],
/// deleted otherwise. Best effort, like [`abort`].
pub async fn set_aside(nodes: &Nodes, server: &str, to: &str, directory: &Path) {
    if resumes(nodes, to)
        && receive(nodes, to, server, directory, ReceiveStep::Resume)
            .await
            .is_ok()
    {
        return;
    }
    discard_orphan(nodes, server, to, directory).await;
}

/// Delete the staging folder on `to`, and what an interrupted copy left
/// there to resume, if any; best effort, since the next move into that node
/// starts over anyway.
pub async fn abort(nodes: &Nodes, server: &str, to: &str, directory: &Path) {
    let _ = receive(nodes, to, server, directory, ReceiveStep::Abort).await;
}

/// Agent side of [`Command::Tree`]. The first page also walks the rest of the
/// tree, so a symlink or special file fails the move before any copying.
pub(crate) fn tree(base: &Path, offset: u64) -> io::Result<Tree> {
    if !fs::symlink_metadata(base)?.is_dir() {
        return Err(invalid("the server directory is not a folder"));
    }
    let mut page = Tree::default();
    let mut total = TreeTotal::default();
    let mut index = 0u64;
    let mut budget = PAGE_BYTES;
    walk(base, "", &mut |entry| {
        if offset == 0 {
            if entry.folder {
                total.folders += 1;
            } else {
                total.files += 1;
                total.bytes += entry.size;
            }
        }
        if index >= offset && page.next.is_none() {
            let cost = entry.path.len() + 64;
            if page.entries.len() < PAGE_ENTRIES && cost <= budget {
                budget -= cost;
                page.entries.push(entry);
            } else {
                page.next = Some(index);
            }
        }
        index += 1;
        // Later pages stop at their end; the first one validates everything.
        page.next.is_none() || offset == 0
    })?;
    if offset == 0 {
        page.total = Some(total);
    }
    Ok(page)
}

/// Visit every entry below `directory` in a stable order, folders before
/// their contents, until `visit` returns `false`. Returns whether it ran to
/// the end.
fn walk(
    directory: &Path,
    prefix: &str,
    visit: &mut dyn FnMut(TreeEntry) -> bool,
) -> io::Result<bool> {
    let mut names = fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    names.sort();
    for name in names {
        let full = directory.join(&name);
        let text = name.to_str().ok_or_else(|| {
            invalid(format!(
                "{} is not a UTF-8 file name; rename it before moving",
                full.display()
            ))
        })?;
        if prefix.is_empty() && text == MOVE_MARKER {
            continue;
        }
        let path = if prefix.is_empty() {
            text.to_owned()
        } else {
            format!("{prefix}/{text}")
        };
        let metadata = fs::symlink_metadata(&full)?;
        if metadata.is_dir() {
            let entry = TreeEntry {
                path: path.clone(),
                folder: true,
                ..TreeEntry::default()
            };
            if !visit(entry) || !walk(&full, &path, visit)? {
                return Ok(false);
            }
        } else if metadata.is_file() {
            let entry = TreeEntry {
                path,
                folder: false,
                size: metadata.len(),
                mode: mode(&metadata),
                modified: modified(&metadata),
            };
            if !visit(entry) {
                return Ok(false);
            }
        } else {
            return Err(invalid(format!(
                "{path} is a symlink or special file; a move copies only regular files and folders"
            )));
        }
    }
    Ok(true)
}

/// Agent side of [`Command::Read`].
pub(crate) fn read_chunk(base: &Path, path: &str, offset: u64) -> io::Result<Value> {
    let target = resolve(base, path)?;
    if !fs::symlink_metadata(&target)?.is_file() {
        return Err(invalid(format!("{path} is not a regular file")));
    }
    let mut file = fs::File::open(&target)?;
    let size = file.metadata()?.len();
    if offset > size {
        return Err(invalid(format!("{path} shrank during the move")));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = Vec::with_capacity((size - offset).min(FILE_PART as u64) as usize);
    file.take(FILE_PART as u64).read_to_end(&mut buffer)?;
    Ok(json!({"content": protocol::encode_base64(&buffer), "size": size}))
}

/// [`apply_step`] without [`Receive::replace`].
#[cfg(test)]
pub(crate) fn receive_step(
    server: &str,
    staging: &Path,
    target: &Path,
    step: &ReceiveStep,
) -> io::Result<Value> {
    apply_step(server, staging, target, step, false)
}

/// The folder an in-place restore moves the old directory to while it puts
/// the restored one in place; deleted right after.
fn replaced(server: &str, target: &Path) -> PathBuf {
    target
        .parent()
        .unwrap_or(Path::new(""))
        .join(format!(".rift-replaced-{server}"))
}

/// Where [`ReceiveStep::Resume`] sets aside what an interrupted move of
/// `server` left behind, for [`ReceiveStep::Have`] to take files from.
/// Deleted on commit, abort and begin.
pub fn resumable(server: &str, target: &Path) -> PathBuf {
    target
        .parent()
        .unwrap_or(Path::new(""))
        .join(format!(".rift-resume-{server}"))
}

/// Agent side of [`Command::Receive`], with `staging` and `target` already
/// resolved below the agent's root.
pub(crate) fn apply_step(
    server: &str,
    staging: &Path,
    target: &Path,
    step: &ReceiveStep,
    replace: bool,
) -> io::Result<Value> {
    let staged = || -> io::Result<()> {
        if exists(staging)? {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no move into this node is in progress for the server",
            ))
        }
    };
    let previous = resumable(server, target);
    match step {
        ReceiveStep::Begin => {
            let mut replaced = false;
            // A restore in place keeps the existing directory until commit.
            if !replace && exists(target)? {
                if moved_server(target).as_deref() != Some(server) {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!(
                            "{} already exists on this node; delete it, or move with --no-copy to use it as the server's folder",
                            target.display()
                        ),
                    ));
                }
                // Left behind by an interrupted move of this server, which
                // the controller never recorded.
                fs::remove_dir_all(target)?;
                replaced = true;
            }
            if exists(staging)? {
                fs::remove_dir_all(staging)?;
            }
            if exists(&previous)? {
                fs::remove_dir_all(&previous)?;
            }
            create_staging(server, staging)?;
            Ok(json!({"replaced": replaced}))
        }
        ReceiveStep::Resume => {
            let mut replaced = false;
            if exists(target)? {
                if moved_server(target).as_deref() != Some(server) {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!(
                            "{} already exists on this node; delete it, or move with --no-copy to use it as the server's folder",
                            target.display()
                        ),
                    ));
                }
                // Put in place by a move the controller never recorded: a
                // whole copy, which the files of this attempt come from.
                absorb(target, &previous)?;
                replaced = true;
            }
            if exists(staging)? {
                if moved_server(staging).as_deref() == Some(server) {
                    absorb(staging, &previous)?;
                } else {
                    fs::remove_dir_all(staging)?;
                }
            }
            create_staging(server, staging)?;
            Ok(json!({"replaced": replaced, "resumed": exists(&previous)?}))
        }
        ReceiveStep::Have { entries } => {
            staged()?;
            let earlier = exists(&previous)?;
            let mut missing = Vec::new();
            for (index, entry) in entries.iter().enumerate() {
                let path = resolve_received(staging, &entry.path)?;
                if entry.folder {
                    fs::create_dir_all(&path)?;
                    continue;
                }
                if received(&path, entry)? {
                    continue;
                }
                // A path that no longer fits the earlier tree, such as a
                // file below what was a file then, is simply sent again.
                if earlier
                    && let Ok(old) = resolve_received(&previous, &entry.path)
                    && received(&old, entry)?
                {
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::rename(&old, &path)?;
                    continue;
                }
                missing.push(index);
            }
            Ok(json!({"missing": missing}))
        }
        ReceiveStep::Folder { path } => {
            staged()?;
            fs::create_dir_all(resolve_received(staging, path)?)?;
            Ok(json!({}))
        }
        ReceiveStep::Chunk {
            path,
            offset,
            content,
            end,
        } => {
            staged()?;
            let bytes = protocol::decode_base64(content)
                .ok_or_else(|| invalid("file contents must be base64"))?;
            if bytes.len() > FILE_PART {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    "a move chunk holds at most 512 KiB",
                ));
            }
            let file_path = resolve_received(staging, path)?;
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = if *offset == 0 {
                fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&file_path)?
            } else {
                let file = fs::OpenOptions::new().write(true).open(&file_path)?;
                if file.metadata()?.len() != *offset {
                    return Err(invalid(format!("chunks of {path} arrived out of order")));
                }
                file
            };
            file.seek(SeekFrom::Start(*offset))?;
            file.write_all(&bytes)?;
            if let Some(end) = end {
                file.sync_all()?;
                drop(file);
                if hash(&file_path)? != end.sha256 {
                    return Err(io::Error::other(format!(
                        "{path} arrived damaged: its SHA-256 differs from the source"
                    )));
                }
                // Only a whole, verified file gets the source's time, which
                // is what a resumed move recognises it by.
                if let Some(modified) = end.modified {
                    fs::OpenOptions::new()
                        .write(true)
                        .open(&file_path)?
                        .set_modified(UNIX_EPOCH + Duration::from_nanos(modified))?;
                }
                set_mode(&file_path, end.mode)?;
            }
            Ok(json!({}))
        }
        ReceiveStep::Commit => {
            if !exists(staging)? && moved_server(target).as_deref() == Some(server) {
                if exists(&previous)? {
                    fs::remove_dir_all(&previous)?;
                }
                return Ok(json!({"committed": true}));
            }
            staged()?;
            let old = replaced(server, target);
            if replace {
                // Left behind by a commit that stopped half way.
                if exists(&old)? {
                    fs::remove_dir_all(&old)?;
                }
                if exists(target)? {
                    fs::rename(target, &old)?;
                }
            } else if exists(target)? {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} appeared during the move", target.display()),
                ));
            }
            let parent = target
                .parent()
                .ok_or_else(|| invalid("the server directory has no parent"))?;
            fs::create_dir_all(parent)?;
            sync_directory(staging)?;
            fs::rename(staging, target)?;
            sync_directory(parent)?;
            if replace && exists(&old)? {
                fs::remove_dir_all(&old)?;
            }
            // What an earlier attempt left that the source no longer has.
            if exists(&previous)? {
                fs::remove_dir_all(&previous)?;
            }
            Ok(json!({"committed": true}))
        }
        ReceiveStep::Finish => match fs::remove_file(target.join(MOVE_MARKER)) {
            Ok(()) => Ok(json!({"finished": true})),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(json!({"finished": true})),
            Err(error) => Err(error),
        },
        ReceiveStep::Abort => {
            if exists(staging)? {
                fs::remove_dir_all(staging)?;
            }
            if exists(&previous)? {
                fs::remove_dir_all(&previous)?;
            }
            Ok(json!({"aborted": true}))
        }
    }
}

fn create_staging(server: &str, staging: &Path) -> io::Result<()> {
    fs::create_dir_all(staging)?;
    fs::write(
        staging.join(MOVE_MARKER),
        json!({"server": server}).to_string(),
    )
}

/// Move everything below `from` into `into`, replacing what is there, and
/// delete `from`.
fn absorb(from: &Path, into: &Path) -> io::Result<()> {
    if !exists(into)? {
        return fs::rename(from, into);
    }
    if !fs::symlink_metadata(into)?.is_dir() {
        fs::remove_file(into)?;
        return fs::rename(from, into);
    }
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let (source, target) = (entry.path(), into.join(entry.file_name()));
        if fs::symlink_metadata(&source)?.is_dir() {
            absorb(&source, &target)?;
        } else {
            if fs::symlink_metadata(&target).is_ok_and(|metadata| metadata.is_dir()) {
                fs::remove_dir_all(&target)?;
            }
            fs::rename(&source, &target)?;
        }
    }
    fs::remove_dir_all(from)
}

/// Whether `path` is a file that an earlier attempt received whole from
/// `entry`: verified files get the source's modification time, so a file
/// with the same size and time is the same file, unless the source was
/// changed and its time put back on purpose. Fixes its permission bits,
/// which a change does not touch the time for.
fn received(path: &Path, entry: &TreeEntry) -> io::Result<bool> {
    let Some(time) = entry.modified else {
        return Ok(false);
    };
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(false);
    };
    if !metadata.is_file() || metadata.len() != entry.size || modified(&metadata) != Some(time) {
        return Ok(false);
    }
    set_mode(path, entry.mode)?;
    Ok(true)
}

/// The server a not yet recorded move put into `directory`, if any.
fn moved_server(directory: &Path) -> Option<String> {
    let marker = directory.join(MOVE_MARKER);
    if !fs::symlink_metadata(&marker).ok()?.is_file() {
        return None;
    }
    let value: Value = serde_json::from_slice(&fs::read(marker).ok()?).ok()?;
    value["server"].as_str().map(str::to_owned)
}

/// `path` below `base`, `/`-separated, refusing `.`, `..` and symlinks.
/// More permissive than the web console: a world may hold any file name its
/// filesystem allows.
fn resolve(base: &Path, path: &str) -> io::Result<PathBuf> {
    if path.is_empty() || path.len() > 4096 {
        return Err(invalid("invalid path"));
    }
    let segments: Vec<String> = path.split('/').map(str::to_owned).collect();
    if segments.len() > 128
        || segments.iter().any(|segment| {
            segment.is_empty()
                || segment == "."
                || segment == ".."
                || segment.contains('\0')
                || (cfg!(windows) && segment.contains(['\\', ':']))
        })
    {
        return Err(invalid(format!("invalid path {path:?}")));
    }
    crate::files::resolve(base, &segments)
}

/// Like [`resolve`], but never the marker of the move itself.
fn resolve_received(staging: &Path, path: &str) -> io::Result<PathBuf> {
    if path == MOVE_MARKER {
        return Err(invalid("a move cannot overwrite its own marker"));
    }
    resolve(staging, path)
}

fn hash(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(hex(&digest.finalize()));
        }
        digest.update(&buffer[..read]);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The modification time in nanoseconds since the Unix epoch, if the
/// filesystem has one.
fn modified(metadata: &fs::Metadata) -> Option<u64> {
    let since = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(since.as_nanos()).ok()
}

#[cfg(unix)]
fn mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn mode(_metadata: &fs::Metadata) -> u32 {
    0
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if mode == 0 {
        return Ok(());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "rift-transfer-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        // The temporary directory may itself sit behind a symlink.
        fs::canonicalize(&path).unwrap()
    }

    /// The agent-side steps of a copy, without the bus, starting with
    /// `start`; returns the paths of the files sent.
    fn send(
        source: &Path,
        staging: &Path,
        target: &Path,
        start: ReceiveStep,
    ) -> io::Result<Vec<String>> {
        let resume = start == ReceiveStep::Resume;
        receive_step("lobby-1", staging, target, &start)?;
        let mut sent = Vec::new();
        let mut offset = 0;
        loop {
            let page = tree(source, offset)?;
            let missing: Option<Vec<u64>> = if resume {
                let step = ReceiveStep::Have {
                    entries: page.entries.clone(),
                };
                let reply = receive_step("lobby-1", staging, target, &step)?;
                let missing = reply["missing"].as_array().unwrap();
                Some(
                    missing
                        .iter()
                        .map(|index| index.as_u64().unwrap())
                        .collect(),
                )
            } else {
                None
            };
            for (index, entry) in page.entries.into_iter().enumerate() {
                if entry.folder {
                    if !resume {
                        let step = ReceiveStep::Folder { path: entry.path };
                        receive_step("lobby-1", staging, target, &step)?;
                    }
                    continue;
                }
                if missing
                    .as_ref()
                    .is_some_and(|missing| !missing.contains(&(index as u64)))
                {
                    continue;
                }
                let mut digest = Sha256::new();
                let mut position = 0;
                loop {
                    let reply = read_chunk(source, &entry.path, position)?;
                    let content = reply["content"].as_str().unwrap().to_owned();
                    let bytes = protocol::decode_base64(&content).unwrap();
                    digest.update(&bytes);
                    let next = position + bytes.len() as u64;
                    let end = (next == entry.size).then(|| FileEnd {
                        sha256: hex(&digest.clone().finalize()),
                        mode: entry.mode,
                        modified: entry.modified,
                    });
                    let last = end.is_some();
                    let step = ReceiveStep::Chunk {
                        path: entry.path.clone(),
                        offset: position,
                        content,
                        end,
                    };
                    receive_step("lobby-1", staging, target, &step)?;
                    if last {
                        break;
                    }
                    position = next;
                }
                sent.push(entry.path);
            }
            match page.next {
                Some(next) => offset = next,
                None => break,
            }
        }
        Ok(sent)
    }

    /// A whole copy from scratch, committed.
    fn relay(source: &Path, staging: &Path, target: &Path) -> io::Result<()> {
        send(source, staging, target, ReceiveStep::Begin)?;
        receive_step("lobby-1", staging, target, &ReceiveStep::Commit)?;
        Ok(())
    }

    fn set_time(path: &Path, seconds: u64) {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(seconds))
            .unwrap();
    }

    #[test]
    fn a_resumed_copy_reuses_whole_files_and_drops_what_the_source_lost() {
        let root = scratch("resume");
        let source = root.join("a/lobby-1");
        fs::create_dir_all(source.join("world/region")).unwrap();
        let big: Vec<u8> = (0..FILE_PART * 2 + 5).map(|i| (i % 249) as u8).collect();
        fs::write(source.join("world/region/big.mca"), &big).unwrap();
        fs::write(source.join("world/level.dat"), b"level").unwrap();
        fs::write(source.join("world/changed.dat"), b"old!").unwrap();
        fs::write(source.join("gone.txt"), b"gone").unwrap();
        set_time(&source.join("world/changed.dat"), 1_000);
        let target = root.join("b/lobby-1");
        let staging = root.join("b/.rift-move-lobby-1");
        let previous = resumable("lobby-1", &target);
        // The first attempt copies everything and is cut off before the
        // commit, half way through rewriting the big file.
        send(&source, &staging, &target, ReceiveStep::Begin).unwrap();
        let interrupted = fs::OpenOptions::new()
            .write(true)
            .open(staging.join("world/region/big.mca"))
            .unwrap();
        interrupted.set_len(FILE_PART as u64).unwrap();
        drop(interrupted);
        // Meanwhile the server ran: one file changed without changing size,
        // one was deleted and one added.
        fs::write(source.join("world/changed.dat"), b"new!").unwrap();
        set_time(&source.join("world/changed.dat"), 2_000);
        fs::remove_file(source.join("gone.txt")).unwrap();
        fs::write(source.join("added.txt"), b"added").unwrap();
        let page = tree(&source, 0).unwrap();
        assert_eq!(
            page.total,
            Some(TreeTotal {
                files: 4,
                folders: 2,
                bytes: big.len() as u64 + 5 + 4 + 5,
            })
        );
        assert!(tree(&source, 1).unwrap().total.is_none());
        let resumed = send(&source, &staging, &target, ReceiveStep::Resume).unwrap();
        assert_eq!(
            resumed,
            ["added.txt", "world/changed.dat", "world/region/big.mca"]
        );
        assert!(previous.exists());
        receive_step("lobby-1", &staging, &target, &ReceiveStep::Commit).unwrap();
        assert!(!previous.exists() && !staging.exists());
        assert_eq!(fs::read(target.join("world/region/big.mca")).unwrap(), big);
        assert_eq!(fs::read(target.join("world/changed.dat")).unwrap(), b"new!");
        assert_eq!(fs::read(target.join("world/level.dat")).unwrap(), b"level");
        assert_eq!(fs::read(target.join("added.txt")).unwrap(), b"added");
        assert!(!target.join("gone.txt").exists());
        // Received files carry the source's time.
        let time = |path: &Path| modified(&fs::metadata(path).unwrap());
        assert_eq!(
            time(&target.join("world/changed.dat")),
            time(&source.join("world/changed.dat"))
        );
        // A copy that was put in place but never recorded is whole: resuming
        // takes it back and sends nothing.
        let begin = receive_step("lobby-1", &staging, &target, &ReceiveStep::Resume).unwrap();
        assert_eq!(begin["replaced"], true);
        assert_eq!(begin["resumed"], true);
        assert!(!target.exists());
        assert!(
            send(&source, &staging, &target, ReceiveStep::Resume)
                .unwrap()
                .is_empty()
        );
        receive_step("lobby-1", &staging, &target, &ReceiveStep::Commit).unwrap();
        assert_eq!(fs::read(target.join("world/region/big.mca")).unwrap(), big);
        // Starting over and aborting delete what an attempt left.
        receive_step("lobby-1", &staging, &target, &ReceiveStep::Finish).unwrap();
        fs::create_dir_all(&previous).unwrap();
        receive_step("lobby-1", &staging, &target, &ReceiveStep::Abort).unwrap();
        assert!(!previous.exists());
        // Without a recorded time nothing is reused, and a folder that is not
        // a move of this server is never taken over.
        let error = receive_step("lobby-1", &staging, &target, &ReceiveStep::Resume).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        let entry = TreeEntry {
            path: "world/level.dat".into(),
            size: 5,
            ..TreeEntry::default()
        };
        assert!(!received(&target.join("world/level.dat"), &entry).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn trees_page_folders_before_their_files_and_skip_the_move_marker() {
        let root = scratch("tree");
        fs::create_dir_all(root.join("world/region")).unwrap();
        fs::write(root.join("world/region/r.0.0.mca"), b"region").unwrap();
        fs::write(root.join("server.properties"), b"x").unwrap();
        fs::write(root.join(MOVE_MARKER), b"{}").unwrap();
        let page = tree(&root, 0).unwrap();
        let paths: Vec<_> = page.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "server.properties",
                "world",
                "world/region",
                "world/region/r.0.0.mca"
            ]
        );
        assert_eq!(page.next, None);
        assert_eq!(page.entries[3].size, 6);
        // Later pages start where the previous one ended.
        let rest = tree(&root, 2).unwrap();
        assert_eq!(rest.entries[0].path, "world/region");
        // Many entries split into pages that cover the tree exactly once.
        fs::create_dir_all(root.join("many")).unwrap();
        for index in 0..PAGE_ENTRIES + 10 {
            fs::write(root.join(format!("many/{index:05}")), b"").unwrap();
        }
        let first = tree(&root, 0).unwrap();
        assert_eq!(first.entries.len(), PAGE_ENTRIES);
        let second = tree(&root, first.next.unwrap()).unwrap();
        assert_eq!(second.next, None);
        assert_eq!(
            first.entries.len() + second.entries.len(),
            PAGE_ENTRIES + 15
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn trees_refuse_symlinks_anywhere_on_the_first_page() {
        let root = scratch("symlink");
        fs::create_dir_all(root.join("z/deep")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("z/deep/link")).unwrap();
        let error = tree(&root, 0).unwrap_err();
        assert!(error.to_string().contains("z/deep/link"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_relayed_directory_arrives_whole_and_verified() {
        let root = scratch("relay");
        let source = root.join("a/local/services/lobby-1");
        fs::create_dir_all(source.join("world/empty")).unwrap();
        let big: Vec<u8> = (0..FILE_PART * 2 + 17).map(|i| (i % 251) as u8).collect();
        fs::write(source.join("world/region.mca"), &big).unwrap();
        fs::write(source.join("blank"), b"").unwrap();
        fs::write(source.join("start.sh"), b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(source.join("start.sh"), fs::Permissions::from_mode(0o750))
                .unwrap();
        }
        let target = root.join("b/local/services/lobby-1");
        let staging = root.join("b/local/services/.rift-move-lobby-1");
        relay(&source, &staging, &target).unwrap();
        assert_eq!(fs::read(target.join("world/region.mca")).unwrap(), big);
        assert!(target.join("world/empty").is_dir());
        assert_eq!(fs::read(target.join("blank")).unwrap(), b"");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(target.join("start.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o750);
        }
        assert!(!staging.exists());
        // Unrecorded, the copy is an orphan that the next attempt replaces.
        assert_eq!(moved_server(&target).as_deref(), Some("lobby-1"));
        let begin = receive_step("lobby-1", &staging, &target, &ReceiveStep::Begin).unwrap();
        assert_eq!(begin["replaced"], true);
        assert!(!target.exists());
        relay(&source, &staging, &target).unwrap();
        // Committing twice is harmless; finishing makes the copy permanent.
        receive_step("lobby-1", &staging, &target, &ReceiveStep::Commit).unwrap();
        receive_step("lobby-1", &staging, &target, &ReceiveStep::Finish).unwrap();
        assert!(!target.join(MOVE_MARKER).exists());
        let error = receive_step("lobby-1", &staging, &target, &ReceiveStep::Begin).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(target.join("world/region.mca").exists());
        // Another server's orphan is not ours to replace either.
        let error = receive_step("lobby-2", &staging, &target, &ReceiveStep::Begin).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn received_files_are_checked_and_confined_to_the_staging_folder() {
        let root = scratch("checks");
        let target = root.join("lobby-1");
        let staging = root.join(".rift-move-lobby-1");
        let chunk =
            |path: &str, offset: u64, content: &[u8], sha256: Option<&str>| ReceiveStep::Chunk {
                path: path.into(),
                offset,
                content: protocol::encode_base64(content),
                end: sha256.map(|sha256| FileEnd {
                    sha256: sha256.into(),
                    mode: 0,
                    modified: None,
                }),
            };
        // Nothing is written before a move begins.
        let error =
            receive_step("lobby-1", &staging, &target, &chunk("a", 0, b"x", None)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        receive_step("lobby-1", &staging, &target, &ReceiveStep::Begin).unwrap();
        for path in ["../escape", "a/../../b", "/abs", "a//b", ".", MOVE_MARKER] {
            assert!(
                receive_step("lobby-1", &staging, &target, &chunk(path, 0, b"x", None)).is_err(),
                "{path}"
            );
        }
        assert!(!root.join("escape").exists());
        // A damaged file fails its final chunk.
        let error = receive_step(
            "lobby-1",
            &staging,
            &target,
            &chunk("a", 0, b"data", Some(&"0".repeat(64))),
        )
        .unwrap_err();
        assert!(error.to_string().contains("SHA-256"), "{error}");
        // Chunks must arrive in order.
        receive_step("lobby-1", &staging, &target, &chunk("b", 0, b"ab", None)).unwrap();
        assert!(receive_step("lobby-1", &staging, &target, &chunk("b", 5, b"c", None)).is_err());
        // Abort removes only the staging folder.
        fs::create_dir_all(&target).unwrap();
        receive_step("lobby-1", &staging, &target, &ReceiveStep::Abort).unwrap();
        assert!(!staging.exists() && target.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
