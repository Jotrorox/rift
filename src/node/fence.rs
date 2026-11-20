//! The fence of a node agent (docs/clustering.md, "Leader exclusivity"):
//! the highest cluster term a leader fenced the agent with, kept in a file
//! below `--root` so it survives restarts of the agent. Control commands of
//! a lower term come from a deposed leader and are refused; inspection
//! commands are never fenced.
use std::{
    io,
    path::{Path, PathBuf},
    sync::Mutex,
};

/// The file below `--root` that holds the term. It starts with a dot, like
/// every file the agent keeps for itself, so no server directory can be
/// named like it.
pub const FILE: &str = ".rift-fence";

#[derive(Debug)]
pub struct Fence {
    path: PathBuf,
    /// 0 until a cluster leader fenced this agent.
    term: Mutex<u64>,
}

impl Fence {
    /// The fence kept below `root`; term 0 when there is no file yet.
    pub fn open(root: &Path) -> io::Result<Self> {
        let path = root.join(FILE);
        let term = match std::fs::read_to_string(&path) {
            Ok(text) => text.trim().parse::<u64>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: not a cluster term: {error}", path.display()),
                )
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("{}: {error}", path.display()),
                ));
            }
        };
        Ok(Self {
            path,
            term: Mutex::new(term),
        })
    }

    /// The highest term this agent was fenced with; 0 if never.
    pub fn term(&self) -> u64 {
        *self.term.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A leader of `term` fences this agent. A lower term than the stored
    /// one is refused; an equal one is accepted again (a leader retrying).
    /// The term is on disk before this returns.
    pub fn raise(&self, term: u64) -> io::Result<u64> {
        let mut current = self.term.lock().unwrap_or_else(|e| e.into_inner());
        if term < *current {
            return Err(stale(term, *current));
        }
        if term > *current {
            let staging = self.path.with_extension("tmp");
            std::fs::write(&staging, format!("{term}\n"))?;
            if let Ok(file) = std::fs::File::open(&staging) {
                let _ = file.sync_all();
            }
            std::fs::rename(&staging, &self.path)?;
            *current = term;
        }
        Ok(term)
    }

    /// Whether a control command carrying `term` may run: always while the
    /// agent was never fenced (a single Rift sends no term); afterwards only
    /// with a term at least the stored one.
    pub fn check(&self, term: Option<u64>) -> io::Result<()> {
        let current = self.term();
        if current == 0 {
            return Ok(());
        }
        match term {
            Some(term) if term >= current => Ok(()),
            Some(term) => Err(stale(term, current)),
            None => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "this node is fenced by cluster term {current}; a control command without a term is refused"
                ),
            )),
        }
    }
}

fn stale(term: u64, current: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "this node is fenced by cluster term {current}; a command of term {term} comes from a deposed leader and is refused"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("rift-fence-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn an_unfenced_agent_takes_commands_without_a_term() {
        let root = root("unfenced");
        let fence = Fence::open(&root).unwrap();
        assert_eq!(fence.term(), 0);
        fence.check(None).unwrap();
        fence.check(Some(1)).unwrap();
        assert!(!root.join(FILE).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_term_survives_a_restart_and_lower_terms_are_refused() {
        let root = root("persisted");
        let fence = Fence::open(&root).unwrap();
        assert_eq!(fence.raise(3).unwrap(), 3);
        // A retry of the same leader is fine.
        assert_eq!(fence.raise(3).unwrap(), 3);
        drop(fence);
        let fence = Fence::open(&root).unwrap();
        assert_eq!(fence.term(), 3);
        let refused = fence.raise(2).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fence.term(), 3);
        fence.check(Some(3)).unwrap();
        fence.check(Some(4)).unwrap();
        assert_eq!(
            fence.check(Some(2)).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            fence.check(None).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(fence.raise(5).unwrap(), 5);
        assert_eq!(Fence::open(&root).unwrap().term(), 5);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_damaged_fence_file_is_an_error_not_term_zero() {
        let root = root("damaged");
        std::fs::write(root.join(FILE), "not a number").unwrap();
        assert_eq!(
            Fence::open(&root).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
