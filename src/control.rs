//! Serialized configuration transactions shared by signals and HTTP clients.
use rift::config::Config;
use std::{
    fs::{self, OpenOptions},
    hash::{Hash, Hasher},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::sync::{mpsc, oneshot};

pub const MAX_SOURCE: usize = 256 * 1024;
pub type Reply = Result<String, Error>;
pub type Sender = mpsc::Sender<Command>;

#[derive(Debug)]
pub struct Error {
    pub status: u16,
    pub message: String,
}
impl Error {
    pub fn invalid(message: impl ToString) -> Self {
        Self {
            status: 400,
            message: message.to_string(),
        }
    }
    pub fn conflict(message: impl ToString) -> Self {
        Self {
            status: 409,
            message: message.to_string(),
        }
    }
}
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::invalid(error)
    }
}

pub enum Operation {
    Reload,
    Save { source: String, revision: String },
}
pub struct Command {
    pub operation: Operation,
    pub reply: oneshot::Sender<Reply>,
}

pub fn read_source(path: &Path) -> io::Result<String> {
    if !fs::metadata(path)?.is_file() {
        return Err(io::Error::other("configuration must be a regular file"));
    }
    let mut source = String::new();
    fs::File::open(path)?
        .take(MAX_SOURCE as u64 + 1)
        .read_to_string(&mut source)?;
    if source.len() > MAX_SOURCE {
        return Err(io::Error::other(
            "configuration exceeds 256 KiB source limit",
        ));
    }
    Ok(source)
}

pub fn revision(source: &str) -> String {
    // An opaque content revision, not an authentication credential.
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hash);
    format!("{:016x}", hash.finish())
}

pub struct Candidate {
    pub source: String,
    pub config: Config,
    pub previous_disk: String,
    pub save: bool,
}

pub fn prepare(path: &Path, active_source: &str, operation: Operation) -> Result<Candidate, Error> {
    let disk = read_source(path)?;
    let (source, save) = match operation {
        Operation::Reload => (disk.clone(), false),
        Operation::Save {
            source,
            revision: expected,
        } => {
            if expected != revision(active_source) {
                return Err(Error::conflict(
                    "configuration changed; fetch the latest revision before saving",
                ));
            }
            if disk != active_source {
                return Err(Error::conflict(
                    "configuration file changed on disk; reload it before saving",
                ));
            }
            (source, true)
        }
    };
    let config = Config::from_lua(&source, &path.display().to_string())?;
    Ok(Candidate {
        source,
        config,
        previous_disk: disk,
        save,
    })
}

/// Create a sibling file and atomically replace the selected configuration. A
/// failed write leaves the original untouched; preserve its access permissions.
pub fn persist(path: &Path, source: &str, expected: &str) -> Result<(), Error> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().unwrap_or(Path::new("."));
    let filename = path
        .file_name()
        .ok_or_else(|| Error::invalid("configuration has no filename"))?;
    let temp: PathBuf = parent.join(format!(
        ".{}.rift-{}-{}.tmp",
        filename.to_string_lossy(),
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let permissions = fs::metadata(path)?.permissions();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    let result = (|| {
        file.write_all(source.as_bytes())?;
        file.set_permissions(permissions)?;
        file.sync_all()?;
        if read_source(path)? != expected {
            return Err(Error::conflict(
                "configuration file changed during save; reload it before saving",
            ));
        }
        fs::rename(&temp, path)?;
        Ok(())
    })();
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
