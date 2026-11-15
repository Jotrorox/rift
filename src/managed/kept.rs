//! Servers with `keep_running`: processes that outlive the proxy and are
//! taken over again by the next one.
//!
//! Such a server runs in its own session, reads its console from a FIFO in
//! its directory and is recorded there with its pid and start time. A later
//! proxy adopts the recorded process only if that pid still has that start
//! time, leads its own session and runs in the server directory. The server
//! can write its own directory, so those checks also keep a forged record
//! from naming a process outside it.

/// The record of a kept server's process, in its working directory.
pub(crate) const STATE_FILE: &str = "rift-process.json";
/// The FIFO a kept server reads its console from, in its working directory.
pub(crate) const CONSOLE_FILE: &str = "rift-console";
/// Whether this platform can identify a process again after a restart.
pub(crate) const SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "macos"));

pub(crate) use imp::*;

#[cfg(unix)]
mod imp {
    use std::{
        fs::{self, OpenOptions},
        io::{self, Read, Write},
        os::unix::fs::{FileTypeExt, OpenOptionsExt},
        path::{Path, PathBuf},
        time::{Duration, Instant},
    };

    use super::{CONSOLE_FILE, STATE_FILE};

    /// How long a process may take to disappear after SIGKILL.
    const KILL_WAIT: Duration = Duration::from_secs(5);

    /// A process started with `keep_running`, identified by more than its pid.
    #[derive(Debug)]
    pub(crate) struct Kept {
        pid: libc::pid_t,
        started: String,
    }

    impl Kept {
        pub(crate) fn pid(&self) -> u32 {
            self.pid as u32
        }

        /// Whether the process still runs; a reused pid has another start time.
        pub(crate) fn alive(&self) -> bool {
            identity(self.pid).as_deref() == Some(self.started.as_str())
        }

        /// `stop` on the console, then kill the process group after `grace`.
        /// The process is no child of this one, so its end is polled.
        pub(crate) fn stop(&self, directory: &Path, grace: Duration) -> io::Result<()> {
            let _ = write_console(directory, "stop");
            let deadline = Instant::now() + grace;
            while self.alive() {
                if Instant::now() >= deadline {
                    return self.kill();
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(())
        }

        /// Kill the process together with its process group.
        pub(crate) fn kill(&self) -> io::Result<()> {
            let deadline = Instant::now() + KILL_WAIT;
            while self.alive() {
                // The process leads its session and so its group (see
                // `recorded`); a pid above 1 never addresses other groups.
                // SAFETY: a plain syscall without memory arguments.
                unsafe {
                    libc::kill(-self.pid, libc::SIGKILL);
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::other(format!(
                        "kept server process {} survived SIGKILL",
                        self.pid
                    )));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(())
        }
    }

    /// The process recorded in `directory`, if Rift started it and it still
    /// runs. A stale record, a reused pid, a process outside the directory
    /// and one that does not lead its own session are all refused.
    pub(crate) fn recorded(directory: &Path) -> Option<Kept> {
        let mut options = OpenOptions::new();
        // The server can write its directory: never follow a planted link,
        // and never wait on a planted FIFO.
        options
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let file = options.open(directory.join(STATE_FILE)).ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        let mut text = String::new();
        file.take(4096).read_to_string(&mut text).ok()?;
        let record: serde_json::Value = serde_json::from_str(&text).ok()?;
        let kept = Kept {
            pid: libc::pid_t::try_from(record["pid"].as_u64()?)
                .ok()
                .filter(|pid| *pid > 1)?,
            started: record["started"].as_str()?.to_owned(),
        };
        // SAFETY: a plain syscall without memory arguments.
        let leads = unsafe { libc::getsid(kept.pid) } == kept.pid;
        (kept.alive() && leads && cwd(kept.pid)? == fs::canonicalize(directory).ok()?)
            .then_some(kept)
    }

    /// Record the process just spawned in `directory` for a later proxy.
    pub(crate) fn record(directory: &Path, pid: u32) -> io::Result<()> {
        // Already gone: there is nothing to adopt, and its exit is reported.
        let Some(started) = libc::pid_t::try_from(pid).ok().and_then(identity) else {
            return Ok(());
        };
        let temporary = directory.join("rift-process.json.tmp");
        match fs::remove_file(&temporary) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(
            serde_json::json!({"pid": pid, "started": started})
                .to_string()
                .as_bytes(),
        )?;
        fs::rename(temporary, directory.join(STATE_FILE))
    }

    /// Remove the record and console of a process that ended.
    pub(crate) fn forget(directory: &Path) {
        let _ = fs::remove_file(directory.join(STATE_FILE));
        let _ = fs::remove_file(directory.join(CONSOLE_FILE));
    }

    /// Create the console FIFO and open it as the server's stdin. Opened for
    /// reading and writing, it never reports end of input, whether or not a
    /// proxy currently holds the other end.
    pub(crate) fn console(directory: &Path) -> io::Result<fs::File> {
        let path = directory.join(CONSOLE_FILE);
        match fs::remove_file(&path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        // SAFETY: `name` is a valid C string.
        if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        if !file.metadata()?.file_type().is_fifo() {
            return Err(unavailable());
        }
        Ok(file)
    }

    /// Write one console line. Without a running server nobody reads the
    /// FIFO, and the write is refused instead of waiting for one.
    pub(crate) fn write_console(directory: &Path, command: &str) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(directory.join(CONSOLE_FILE))
            .map_err(|error| match error.raw_os_error() {
                Some(libc::ENXIO | libc::ENOENT | libc::ELOOP) => unavailable(),
                _ => error,
            })?;
        // The server may have put anything under the console's name.
        if !file.metadata()?.file_type().is_fifo() {
            return Err(unavailable());
        }
        let line = format!("{command}\n");
        let mut rest = line.as_bytes();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !rest.is_empty() {
            match file.write(rest) {
                Ok(written) => rest = &rest[written..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                // A server that does not read its console fills the FIFO.
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "server console write timed out",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn unavailable() -> io::Error {
        io::Error::new(
            io::ErrorKind::NotConnected,
            "managed server console is unavailable",
        )
    }

    /// What tells a process from a later one with the same pid: its start
    /// time, and on Linux the boot it counts from. `None` for no process, a
    /// zombie and a platform without this check.
    #[cfg(target_os = "linux")]
    fn identity(pid: libc::pid_t) -> Option<String> {
        static BOOT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        let boot = BOOT
            .get_or_init(|| {
                fs::read_to_string("/proc/sys/kernel/random/boot_id")
                    .ok()
                    .map(|boot| boot.trim().to_owned())
            })
            .as_deref()?;
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name may itself contain spaces and parentheses; the
        // state (field 3) and start time (field 22) follow its last one.
        let mut fields = stat.rsplit_once(')')?.1.split_ascii_whitespace();
        if fields.next()? == "Z" {
            return None;
        }
        Some(format!("{boot}:{}", fields.nth(18)?))
    }

    #[cfg(target_os = "macos")]
    fn identity(pid: libc::pid_t) -> Option<String> {
        /// `SZOMB` in `sys/proc.h`.
        const ZOMBIE: u32 = 5;
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: the kernel writes at most `size` bytes into `info`, and
        // every bit pattern is a valid `proc_bsdinfo`.
        let info = unsafe {
            if libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            ) != size
            {
                return None;
            }
            info.assume_init()
        };
        (info.pbi_status != ZOMBIE)
            .then(|| format!("{}.{:06}", info.pbi_start_tvsec, info.pbi_start_tvusec))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn identity(_pid: libc::pid_t) -> Option<String> {
        None
    }

    /// The working directory of `pid`.
    #[cfg(target_os = "linux")]
    fn cwd(pid: libc::pid_t) -> Option<PathBuf> {
        fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    #[cfg(target_os = "macos")]
    fn cwd(pid: libc::pid_t) -> Option<PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
        let size = size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        // SAFETY: the kernel writes at most `size` bytes into `info`, every
        // bit pattern is a valid `proc_vnodepathinfo`, and the zeroed buffer
        // keeps the path NUL-terminated.
        let path = unsafe {
            if libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            ) != size
            {
                return None;
            }
            let info = info.assume_init();
            std::ffi::CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast())
                .to_bytes()
                .to_vec()
        };
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(&path)))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn cwd(_pid: libc::pid_t) -> Option<PathBuf> {
        None
    }
}

/// Configuration validation rejects `keep_running` here; nothing is kept.
#[cfg(not(unix))]
mod imp {
    use std::{fs::File, io, path::Path, time::Duration};

    #[derive(Debug)]
    pub(crate) struct Kept(());

    impl Kept {
        pub(crate) fn pid(&self) -> u32 {
            0
        }

        pub(crate) fn alive(&self) -> bool {
            false
        }

        pub(crate) fn stop(&self, _directory: &Path, _grace: Duration) -> io::Result<()> {
            Ok(())
        }

        pub(crate) fn kill(&self) -> io::Result<()> {
            Ok(())
        }
    }

    pub(crate) fn recorded(_directory: &Path) -> Option<Kept> {
        None
    }

    pub(crate) fn record(_directory: &Path, _pid: u32) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub(crate) fn forget(_directory: &Path) {}

    pub(crate) fn console(_directory: &Path) -> io::Result<File> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub(crate) fn write_console(_directory: &Path, _command: &str) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}
