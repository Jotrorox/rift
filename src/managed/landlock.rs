//! Confining a managed server's filesystem with Landlock (Linux 5.13+), so
//! a plugin on an instance cannot read Rift's configuration, its database,
//! the messaging key, the instance-login key or other instances' files and
//! environment, without any privilege.
//!
//! The ruleset is built in the parent: every allowed path is opened there
//! and added as a rule, so nothing in the forked child allocates or resolves
//! names except one `open("/proc/self")`, which must happen in the child to
//! name the child's own `/proc` entry rather than Rift's. The child then
//! sets `no_new_privs` and restricts itself; the rules survive `execve`.
//!
//! What a confined server may use:
//!
//! - read and write: its own directory and `/tmp` (the JVM's perf data and
//!   native libraries such as Netty's are written there; nothing secret is),
//!   plus `/dev/null`;
//! - read and execute: `/usr`, `/lib*`, `/bin`, `/sbin`, the Java install the
//!   command's program resolves to, every existing absolute path in the
//!   command line (a shared `/absolute/paper.jar`), and the operator's
//!   `isolation.read` paths;
//! - read only: the system files under `/etc` that name resolution, TLS,
//!   time zones and user lookups need (never all of `/etc`, where a packaged
//!   Rift keeps `rift.lua`), the random devices, `/proc/self` and the
//!   system-wide `/proc` and `/sys` files the JVM sizes itself from;
//! - `isolation.write` paths read and write.
//!
//! On ABI 6 (Linux 6.12) the server also cannot signal processes outside its
//! sandbox, Rift included. Networking is not restricted: plugins listen on
//! their own ports (votes, maps, queries) and connect to databases.
use std::{
    fs::File,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::{Path, PathBuf},
};

// The syscall numbers are the same on every architecture Rift ships for.
const SYS_CREATE_RULESET: libc::c_long = 444;
const SYS_ADD_RULE: libc::c_long = 445;
const SYS_RESTRICT_SELF: libc::c_long = 446;
const CREATE_RULESET_VERSION: u32 = 1;
const RULE_PATH_BENEATH: libc::c_int = 1;
const SCOPE_SIGNAL: u64 = 1 << 1;

const EXECUTE: u64 = 1 << 0;
const WRITE_FILE: u64 = 1 << 1;
const READ_FILE: u64 = 1 << 2;
const READ_DIR: u64 = 1 << 3;
const REMOVE_DIR: u64 = 1 << 4;
const REMOVE_FILE: u64 = 1 << 5;
const MAKE_CHAR: u64 = 1 << 6;
const MAKE_DIR: u64 = 1 << 7;
const MAKE_REG: u64 = 1 << 8;
const MAKE_SOCK: u64 = 1 << 9;
const MAKE_FIFO: u64 = 1 << 10;
const MAKE_BLOCK: u64 = 1 << 11;
const MAKE_SYM: u64 = 1 << 12;
const REFER: u64 = 1 << 13;
const TRUNCATE: u64 = 1 << 14;
const IOCTL_DEV: u64 = 1 << 15;
/// The rights that apply to a file rather than a directory.
const FILE_RIGHTS: u64 = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE | IOCTL_DEV;

const READ: u64 = READ_FILE | READ_DIR;
const READ_EXECUTE: u64 = READ | EXECUTE;
const WRITE: u64 = READ_EXECUTE
    | WRITE_FILE
    | REMOVE_DIR
    | REMOVE_FILE
    | MAKE_DIR
    | MAKE_REG
    | MAKE_SOCK
    | MAKE_FIFO
    | MAKE_SYM
    | REFER
    | TRUNCATE
    | IOCTL_DEV;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The Landlock ABI this kernel offers, or why there is none.
pub fn abi() -> Result<u32, String> {
    // SAFETY: the version query takes no pointer.
    let version = unsafe {
        libc::syscall(
            SYS_CREATE_RULESET,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    if version > 0 {
        return Ok(version as u32);
    }
    Err(match io::Error::last_os_error().raw_os_error() {
        Some(libc::ENOSYS) => "the kernel predates Landlock (Linux 5.13)".into(),
        Some(libc::EOPNOTSUPP) => {
            "Landlock is not enabled (add landlock to the kernel's lsm= list)".into()
        }
        _ => format!("Landlock is unavailable: {}", io::Error::last_os_error()),
    })
}

/// What a sandbox on this ABI enforces, for logs and `rift check`.
pub fn describe(abi: u32) -> String {
    let mut parts = vec!["filesystem"];
    if abi >= 6 {
        parts.push("signals");
    }
    format!("landlock ABI {abi} ({})", parts.join(", "))
}

/// A ruleset ready for a child to enter.
pub(super) struct Sandbox {
    ruleset: OwnedFd,
}

impl Sandbox {
    /// The ruleset for a server in `directory` running `command`, with the
    /// operator's extra `read` and `write` paths.
    pub(super) fn new(
        abi: u32,
        directory: &Path,
        command: &[String],
        read: &[PathBuf],
        write: &[PathBuf],
    ) -> io::Result<Self> {
        let mut handled = MAKE_CHAR | MAKE_BLOCK | WRITE | READ_EXECUTE;
        if abi < 2 {
            handled &= !REFER;
        }
        if abi < 3 {
            handled &= !TRUNCATE;
        }
        if abi < 5 {
            handled &= !IOCTL_DEV;
        }
        let attr = RulesetAttr {
            handled_access_fs: handled,
            handled_access_net: 0,
            scoped: if abi >= 6 { SCOPE_SIGNAL } else { 0 },
        };
        // Older kernels reject the fields they do not know, unless zero and
        // left out of the size.
        let size = if abi >= 6 {
            size_of::<RulesetAttr>()
        } else {
            size_of::<u64>()
        };
        // SAFETY: `attr` outlives the call and `size` covers only its prefix.
        let fd =
            unsafe { libc::syscall(SYS_CREATE_RULESET, &attr as *const RulesetAttr, size, 0u32) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the kernel returned a fresh descriptor we now own.
        let sandbox = Self {
            ruleset: unsafe { OwnedFd::from_raw_fd(fd as i32) },
        };
        let mut rules: Vec<(PathBuf, u64)> =
            vec![(directory.to_owned(), WRITE), ("/tmp".into(), WRITE)];
        for system in ["/usr", "/lib", "/lib32", "/lib64", "/bin", "/sbin"] {
            rules.push((system.into(), READ_EXECUTE));
        }
        for file in [
            "/etc/resolv.conf",
            "/etc/hosts",
            "/etc/host.conf",
            "/etc/nsswitch.conf",
            "/etc/gai.conf",
            "/etc/passwd",
            "/etc/group",
            "/etc/localtime",
            "/etc/timezone",
            "/etc/ld.so.cache",
            "/etc/ld.so.conf",
            "/etc/ld.so.conf.d",
            "/etc/ssl",
            "/etc/pki",
            "/etc/ca-certificates",
            "/etc/alternatives",
            "/etc/java",
            "/etc/mime.types",
            "/dev/urandom",
            "/dev/random",
            "/dev/zero",
            "/proc/cpuinfo",
            "/proc/meminfo",
            "/proc/stat",
            "/proc/sys",
            "/sys/fs/cgroup",
            "/sys/devices/system/cpu",
        ] {
            rules.push((file.into(), READ));
        }
        rules.push(("/dev/null".into(), WRITE));
        if let Some(java) = command.first().and_then(|program| install_root(program)) {
            rules.push((java, READ_EXECUTE));
        }
        for argument in command.iter().skip(1) {
            let path = Path::new(argument);
            if path.is_absolute() && path.exists() {
                rules.push((path.to_owned(), READ_EXECUTE));
            }
        }
        rules.extend(read.iter().map(|path| (path.clone(), READ_EXECUTE)));
        rules.extend(write.iter().map(|path| (path.clone(), WRITE)));
        for (path, access) in rules {
            // Paths a host lacks (no /lib32, no /etc/pki) are skipped.
            let Ok(file) = File::open(&path) else {
                continue;
            };
            let directory = file.metadata()?.is_dir();
            sandbox.add(&file, access & handled, directory)?;
        }
        Ok(sandbox)
    }

    fn add(&self, file: &impl AsRawFd, access: u64, directory: bool) -> io::Result<()> {
        add_rule(
            self.ruleset.as_raw_fd(),
            file.as_raw_fd(),
            access,
            directory,
        )
    }

    /// What the child runs between `fork` and `execve`: allow its own
    /// `/proc/self`, set `no_new_privs`, and restrict itself.
    ///
    /// # Safety
    ///
    /// Call only in the forked child; it makes async-signal-safe syscalls.
    pub(super) unsafe fn enter(ruleset: i32) -> io::Result<()> {
        let path = c"/proc/self";
        // SAFETY: a plain open of a NUL-terminated literal.
        let proc_self = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if proc_self >= 0 {
            let _ = add_rule(ruleset, proc_self, READ, true);
            // SAFETY: closing the descriptor just opened.
            unsafe { libc::close(proc_self) };
        }
        // SAFETY: plain syscalls without pointers into our memory.
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::syscall(SYS_RESTRICT_SELF, ruleset, 0u32) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub(super) fn raw(&self) -> i32 {
        self.ruleset.as_raw_fd()
    }
}

fn add_rule(ruleset: i32, parent: i32, access: u64, directory: bool) -> io::Result<()> {
    let allowed = if directory {
        access
    } else {
        access & FILE_RIGHTS
    };
    let attr = PathBeneathAttr {
        allowed_access: allowed,
        parent_fd: parent,
    };
    // SAFETY: `attr` outlives the call.
    let result = unsafe {
        libc::syscall(
            SYS_ADD_RULE,
            ruleset,
            RULE_PATH_BENEATH,
            &attr as *const PathBeneathAttr,
            0u32,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The install root of a program found on `PATH`: `/usr/lib/jvm/java-21`
/// for a `java` that resolves to `/usr/lib/jvm/java-21/bin/java`, so a JDK
/// outside the system directories (and its `lib/`) is readable.
fn install_root(program: &str) -> Option<PathBuf> {
    let path = Path::new(program);
    let found = if path.components().count() > 1 {
        path.to_owned()
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|directory| directory.join(program))
            .find(|candidate| candidate.is_file())?
    };
    let resolved = std::fs::canonicalize(found).ok()?;
    let bin = resolved.parent()?;
    Some(if bin.file_name().is_some_and(|name| name == "bin") {
        bin.parent()?.to_owned()
    } else {
        bin.to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A confined child reads its own directory but not a sibling holding
    /// Rift's secrets, nor its parent's environment (readable without the
    /// sandbox: same user, dumpable), nor the rest of `/etc`. Skipped where
    /// Landlock is unavailable.
    #[tokio::test]
    async fn a_sandboxed_server_cannot_read_outside_its_grants() {
        let Ok(abi) = abi() else {
            eprintln!("skipped: {}", abi().unwrap_err());
            return;
        };
        let root = std::env::temp_dir().join(format!("rift-landlock-{}", std::process::id()));
        let instance = root.join("services/lobby-3");
        std::fs::create_dir_all(&instance).unwrap();
        std::fs::write(root.join("instance-key"), "secret").unwrap();
        std::fs::write(instance.join("own.txt"), "mine").unwrap();
        let script = format!(
            "cat own.txt > out.txt; \
             cat {key} >> out.txt 2>/dev/null && echo LEAKED >> out.txt; \
             cat /proc/$PPID/environ > /dev/null 2>&1 && echo PROC >> out.txt; \
             cat /etc/hostname > /dev/null 2>&1 && echo ETC >> out.txt; \
             echo done >> out.txt",
            key = root.join("instance-key").display()
        );
        let command = ["sh".to_owned(), "-c".to_owned(), script];
        let isolation = crate::config::Isolation {
            mode: crate::config::IsolationMode::Required,
            ..Default::default()
        };
        let mut child = super::super::backend::spawn(
            &command,
            &instance,
            &crate::config::ProcessEnvironment::default(),
            &isolation,
        )
        .unwrap();
        assert!(child.wait().await.unwrap().success());
        let out = std::fs::read_to_string(instance.join("out.txt")).unwrap();
        assert_eq!(out, "minedone\n", "ABI {abi}: {out}");
        let _ = std::fs::remove_dir_all(root);
    }
}
