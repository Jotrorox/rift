//! Limiting a managed server's memory with a cgroup v2 group of its own, so
//! one server that leaks or is misconfigured is killed by the kernel instead
//! of taking the host, the proxy and every other server with it.
//!
//! Rift needs no privilege for this, only a cgroup it may manage: the one it
//! runs in, delegated to its user (`Delegate=memory` in a systemd unit, or
//! `systemd-run --user --scope -p Delegate=yes`). cgroup v2 allows processes
//! only in leaves once a group distributes a controller, so the layout is:
//!
//! ```text
//! <delegated group>/            cgroup.subtree_control: memory
//!     rift/                     the proxy (or node agent) and unlimited servers
//!     rift-server-<name>/       one server: memory.max, no swap, memory.oom.group
//! ```
//!
//! Rift is either started in `rift/` (`DelegateSubgroup=rift`) or moves
//! itself there before the first limited server starts. It never moves a
//! process it did not start: if anything else shares its group, the memory
//! controller cannot be distributed and the limit is unavailable.
//!
//! Everything is prepared in the parent. The forked child only writes `0`
//! to its group's already open `cgroup.procs`, which moves the writer, so
//! the server never runs a single instruction outside its limit.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

const MOUNT: &str = "/sys/fs/cgroup";
/// The leaf Rift itself runs in.
const LEAF: &str = "rift";
const SERVER_PREFIX: &str = "rift-server-";

/// Serialises group setup with the starts that use it: an empty group of a
/// server about to start must not be swept away by another start.
static SETUP: Mutex<()> = Mutex::new(());

/// The delegated group, and whether Rift already runs in its [`LEAF`].
#[derive(Debug, PartialEq, Eq)]
struct Layout {
    root: PathBuf,
    in_leaf: bool,
}

/// Where this process is in the hierarchy mounted at `mount`, from the
/// contents of `/proc/self/cgroup`.
fn layout(mount: &Path, membership: &str) -> Result<Layout, String> {
    let path = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or("this host has no cgroup v2 hierarchy")?;
    let relative = path.trim_start_matches('/');
    if relative.split('/').any(|part| part == "..") {
        return Err(format!("unexpected cgroup path {path}"));
    }
    let own = mount.join(relative);
    // Only the hierarchy's real root lacks cgroup.events; the root of a
    // container's cgroup namespace has it and is an ordinary group.
    if relative.is_empty() && !mount.join("cgroup.events").exists() {
        return Err(
            "Rift runs in the root cgroup; start it in a delegated group of its own".into(),
        );
    }
    if !relative.is_empty() && own.file_name().is_some_and(|name| name == LEAF) {
        let root = own.parent().unwrap_or(mount).to_owned();
        return Ok(Layout {
            root,
            in_leaf: true,
        });
    }
    Ok(Layout {
        root: own,
        in_leaf: false,
    })
}

fn writable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a valid NUL-terminated string.
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

fn lists(path: &Path, controller: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|list| list.split_whitespace().any(|c| c == controller))
}

/// The delegated group, if Rift can limit memory below it. Changes nothing.
fn probe_at(mount: &Path, membership: &str) -> Result<Layout, String> {
    let layout = layout(mount, membership)?;
    let root = &layout.root;
    if !lists(&root.join("cgroup.controllers"), "memory") {
        return Err(format!(
            "the memory controller is not available in {}",
            root.display()
        ));
    }
    if !writable(root)
        || !writable(&root.join("cgroup.procs"))
        || !writable(&root.join("cgroup.subtree_control"))
    {
        return Err(format!(
            "{} is not delegated to Rift's user (in a systemd unit: Delegate=memory)",
            root.display()
        ));
    }
    Ok(layout)
}

fn probe() -> Result<Layout, String> {
    let membership = fs::read_to_string("/proc/self/cgroup")
        .map_err(|error| format!("/proc/self/cgroup: {error}"))?;
    probe_at(Path::new(MOUNT), &membership)
}

/// What a memory limit gets on this host, for `rift check`: the group
/// servers are limited in, or why they cannot be.
pub fn describe() -> Result<String, String> {
    probe().map(|layout| format!("cgroup v2 below {}", layout.root.display()))
}

fn write(path: &Path, value: &str) -> io::Result<()> {
    OpenOptions::new()
        .write(true)
        .open(path)?
        .write_all(value.as_bytes())
}

/// Move Rift into its leaf if needed and distribute the memory controller
/// to the server groups. Idempotent.
fn distribute(layout: &Layout) -> Result<(), String> {
    let root = &layout.root;
    let control = root.join("cgroup.subtree_control");
    if lists(&control, "memory") {
        return Ok(());
    }
    if !layout.in_leaf {
        let leaf = root.join(LEAF);
        match fs::create_dir(&leaf) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("{}: {error}", leaf.display())),
        }
        write(&leaf.join("cgroup.procs"), &std::process::id().to_string())
            .map_err(|error| format!("moving Rift into {}: {error}", leaf.display()))?;
    }
    write(&control, "+memory").map_err(|error| {
        if error.raw_os_error() == Some(libc::EBUSY) {
            format!(
                "other processes share Rift's cgroup {}; stop them or start Rift in a group of its own",
                root.display()
            )
        } else {
            format!("{}: {error}", control.display())
        }
    })
}

/// A group name for `server`: its name where that is a safe file name,
/// with every other byte escaped.
fn group_name(server: &str) -> String {
    let mut name = String::from(SERVER_PREFIX);
    for byte in server.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
            name.push(byte as char);
        } else {
            name.push_str(&format!(".{byte:02x}"));
        }
    }
    name
}

/// A server's group, ready for the child to enter. Hold it until the child
/// has exec'd.
pub(super) struct Group {
    procs: File,
    _setup: MutexGuard<'static, ()>,
}

impl Group {
    /// Create (or reuse) the group of `server` with `megabytes` of memory.
    pub(super) fn new(server: &str, megabytes: usize) -> Result<Self, String> {
        let setup = SETUP.lock().unwrap_or_else(|error| error.into_inner());
        let layout = probe()?;
        distribute(&layout)?;
        let name = group_name(server);
        // Groups of servers that have exited; a group in use refuses removal.
        if let Ok(entries) = fs::read_dir(&layout.root) {
            for entry in entries.flatten() {
                let other = entry.file_name();
                if other.to_string_lossy().starts_with(SERVER_PREFIX) && *other != *name {
                    let _ = fs::remove_dir(entry.path());
                }
            }
        }
        let directory = layout.root.join(&name);
        match fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("{}: {error}", directory.display())),
        }
        let bytes = (megabytes as u64).saturating_mul(1024 * 1024);
        let limit = directory.join("memory.max");
        write(&limit, &bytes.to_string())
            .map_err(|error| format!("{}: {error}", limit.display()))?;
        // The limit is on memory, not on memory before swapping starts: a
        // server past it is killed rather than left thrashing the host.
        let _ = write(&directory.join("memory.swap.max"), "0");
        // Kill the whole server, helper processes included, rather than
        // whichever of its processes the kernel picks. Linux 4.19+.
        let _ = write(&directory.join("memory.oom.group"), "1");
        let procs = directory.join("cgroup.procs");
        let procs = OpenOptions::new()
            .write(true)
            .open(&procs)
            .map_err(|error| format!("{}: {error}", procs.display()))?;
        Ok(Self {
            procs,
            _setup: setup,
        })
    }

    pub(super) fn raw(&self) -> i32 {
        self.procs.as_raw_fd()
    }

    /// Move the calling process into the group behind `procs`.
    ///
    /// # Safety
    /// For the forked child before exec: makes one async-signal-safe call
    /// and builds its error without allocating.
    pub(super) unsafe fn enter(procs: i32) -> io::Result<()> {
        // SAFETY: the buffer is one valid byte; `0` names the writer.
        if unsafe { libc::write(procs, b"0".as_ptr().cast(), 1) } != 1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_delegated_group_is_rifts_own_or_the_parent_of_its_leaf() {
        let mount = std::env::temp_dir().join(format!("rift-cgroup-layout-{}", std::process::id()));
        let _ = fs::remove_dir_all(&mount);
        fs::create_dir_all(mount.join("system.slice/rift.service/rift")).unwrap();
        let at = |membership: &str| layout(&mount, membership);
        assert_eq!(
            at("0::/system.slice/rift.service\n").unwrap(),
            Layout {
                root: mount.join("system.slice/rift.service"),
                in_leaf: false
            }
        );
        assert_eq!(
            at("0::/system.slice/rift.service/rift\n").unwrap(),
            Layout {
                root: mount.join("system.slice/rift.service"),
                in_leaf: true
            }
        );
        // cgroup v1 and hybrid hosts list only numbered hierarchies.
        assert!(at("12:memory:/user.slice\n1:name=systemd:/x\n").is_err());
        assert!(at("0::/../escape\n").is_err());
        // The real root is never managed; a container's root group is.
        assert!(at("0::/\n").unwrap_err().contains("root cgroup"));
        fs::write(mount.join("cgroup.events"), "populated 1\n").unwrap();
        assert_eq!(
            at("0::/\n").unwrap(),
            Layout {
                root: mount.clone(),
                in_leaf: false
            }
        );
        fs::remove_dir_all(&mount).unwrap();
    }

    #[test]
    fn an_undelegated_group_is_reported_without_being_changed() {
        let mount = std::env::temp_dir().join(format!("rift-cgroup-probe-{}", std::process::id()));
        let _ = fs::remove_dir_all(&mount);
        let group = mount.join("app");
        fs::create_dir_all(&group).unwrap();
        fs::write(group.join("cgroup.controllers"), "cpu pids\n").unwrap();
        let error = probe_at(&mount, "0::/app\n").unwrap_err();
        assert!(error.contains("memory controller"), "{error}");
        fs::write(group.join("cgroup.controllers"), "cpu memory pids\n").unwrap();
        // Without the control files there is nothing to write to.
        let error = probe_at(&mount, "0::/app\n").unwrap_err();
        assert!(error.contains("not delegated"), "{error}");
        fs::write(group.join("cgroup.procs"), "").unwrap();
        fs::write(group.join("cgroup.subtree_control"), "").unwrap();
        assert_eq!(probe_at(&mount, "0::/app\n").unwrap().root, group);
        fs::remove_dir_all(&mount).unwrap();
    }

    #[test]
    fn group_names_are_single_safe_path_components() {
        assert_eq!(group_name("lobby-1"), "rift-server-lobby-1");
        assert_eq!(group_name("../a b"), "rift-server-.2e.2e.2fa.20b");
    }
}
