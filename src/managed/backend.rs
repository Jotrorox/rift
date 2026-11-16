//! Process handling behind the supervision worker: a child of this process
//! ([`Local`]) or a process run by a `rift node` agent ([`Remote`]). The worker
//! keeps phases, retries, readiness deadlines and idle stops for both. A local
//! server with `keep_running` outlives this process; see [`kept`].
use std::{fs::OpenOptions, io, process::Stdio, sync::atomic::Ordering, time::Duration};

use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    process::{Child, Command},
};

use super::{LOG_FILE, Server, kept, lock, loopback_address};
use crate::config::ProcessEnvironment;
use crate::node::{Nodes, protocol};

/// What a status check found.
#[derive(Debug)]
pub(super) enum Status {
    /// No process is owned.
    Idle,
    Running,
    /// The process ended; the text describes its exit status. `success`
    /// means exit status 0, which a node agent older than this field never
    /// reports.
    Exited {
        status: String,
        success: bool,
    },
    /// The process could not be inspected and was reaped.
    Broken(String),
    /// The process may still run, but its node is unreachable.
    Unknown,
}

pub(super) trait ProcessBackend: Send + 'static {
    fn owns(&self) -> bool;
    fn pid(&self) -> Option<u32>;
    /// Refuse a start without recording a failure, e.g. while a node is unreachable.
    fn available(&self) -> io::Result<()>;
    /// Spawn the server process; readiness is probed separately.
    fn start(&mut self, server: &Server) -> impl Future<Output = io::Result<()>> + Send;
    /// One readiness probe: whether the server accepts TCP connections.
    fn ready(&mut self, server: &Server, wait: Duration) -> impl Future<Output = bool> + Send;
    fn status(&mut self, server: &Server) -> impl Future<Output = Status> + Send;
    /// Write `stop` to the console, then kill after `grace`.
    fn stop(
        &mut self,
        server: &Server,
        grace: Duration,
    ) -> impl Future<Output = io::Result<()>> + Send;
    /// Terminate a process that failed to become ready.
    fn kill(&mut self, server: &Server) -> impl Future<Output = ()> + Send;
    fn send_command(
        &mut self,
        server: &Server,
        command: &str,
    ) -> impl Future<Output = io::Result<()>> + Send;
    /// A running process this worker did not start, which it may take over:
    /// remote servers and local ones with `keep_running` keep running while
    /// the controller restarts.
    fn adoptable(&mut self, server: &Server) -> Option<Option<u32>>;
    fn adopt(&mut self, server: &Server, pid: Option<u32>);
    /// On controller shutdown, local processes stop unless they have
    /// `keep_running`; remote ones keep running. Both are adopted again by
    /// the next controller.
    fn survives_controller(&self, server: &Server) -> bool;
}

#[derive(Debug, Default)]
pub(super) struct Local {
    child: Option<Child>,
    /// A `keep_running` process an earlier proxy started. It is no child of
    /// this one: its status is polled and its exit status is never known.
    adopted: Option<kept::Kept>,
    /// The recorded process `adoptable` found, until `adopt` takes it.
    candidate: Option<kept::Kept>,
    /// Whether the server directory was searched for such a process. Every
    /// process started later is this worker's own.
    searched: bool,
    logins: super::LoginSlot,
}

impl Local {
    pub(super) fn new(logins: super::LoginSlot) -> Self {
        Self {
            logins,
            ..Self::default()
        }
    }
}

/// Run blocking work on a kept process (see [`kept`]) off the runtime.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(io::Error::other)?
}

/// The server's environment plus what Rift sets for it: `RIFT_SERVER_NAME`,
/// and with a bus login `RIFT_SERVER_GROUP` and the `RIFT_BUS_*` variables
/// (see [`crate::instance_login`]). `directory` is where the process runs on
/// this host, for `RIFT_BUS_CERT`; `None` leaves that to the node agent.
/// Rift's variables are set last, so `env` cannot override them.
fn with_login(
    logins: &super::LoginSlot,
    server: &Server,
    directory: Option<&std::path::Path>,
) -> io::Result<(ProcessEnvironment, Option<String>)> {
    let mut environment = server.config.environment.clone();
    let logins = logins
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let Some(logins) = logins else {
        return Ok((environment, None));
    };
    if let Some(directory) = directory {
        logins.write_certificate(directory)?;
    }
    environment
        .set
        .extend(logins.environment(&server.name, directory));
    let certificate = logins
        .is_registered(&server.name)
        .then(|| logins.bus().map(|bus| bus.certificate))
        .flatten();
    Ok((environment, certificate))
}

impl ProcessBackend for Local {
    fn owns(&self) -> bool {
        self.child.is_some() || self.adopted.is_some()
    }

    fn pid(&self) -> Option<u32> {
        match &self.adopted {
            Some(process) => Some(process.pid()),
            None => self.child.as_ref().and_then(Child::id),
        }
    }

    fn available(&self) -> io::Result<()> {
        Ok(())
    }

    async fn start(&mut self, server: &Server) -> io::Result<()> {
        let address = loopback_address(server.address)?;
        // Probe local port ownership directly. Closed-port TCP refusal can take
        // longer than a short preflight deadline on Windows. The probe holds
        // the spawn lock so no child can inherit its listener (see `SPAWN`).
        let probe = {
            let _spawning = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
            std::net::TcpListener::bind(address).map(drop)
        };
        match probe {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "managed backend port is in use; Rift will not adopt an unowned process",
                ));
            }
            Err(error) => return Err(error),
        }
        let directory = &server.config.directory;
        let (environment, _) = with_login(&self.logins, server, Some(directory))?;
        self.searched = true;
        let console = if server.config.keep_running {
            if !kept::SUPPORTED {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "keep_running requires Linux or macOS",
                ));
            }
            Some(kept::console(directory)?)
        } else {
            None
        };
        let kept = console.is_some();
        let mut child = spawn_with(
            &server.config.command,
            directory,
            &environment,
            &server.config.isolation,
            server
                .config
                .memory_limit
                .map(|limit| (server.name.as_str(), limit)),
            console,
        )?;
        // Without its record the next proxy could not adopt the process.
        if kept
            && let Some(pid) = child.id()
            && let Err(error) = kept::record(directory, pid)
        {
            let _ = kill(&mut child).await;
            kept::forget(directory);
            return Err(error);
        }
        self.child = Some(child);
        Ok(())
    }

    async fn ready(&mut self, server: &Server, wait: Duration) -> bool {
        let Ok(address) = loopback_address(server.address) else {
            return false;
        };
        accepted(tokio::time::timeout(wait, TcpStream::connect(address)).await)
    }

    async fn status(&mut self, server: &Server) -> Status {
        if let Some(process) = &self.adopted {
            if process.alive() {
                return Status::Running;
            }
            self.adopted = None;
            kept::forget(&server.config.directory);
            return Status::Exited {
                status: "no longer running; an adopted process leaves no exit status".into(),
                success: false,
            };
        }
        let Some(process) = self.child.as_mut() else {
            return Status::Idle;
        };
        let status = process.try_wait();
        if server.config.keep_running && !matches!(status, Ok(None)) {
            kept::forget(&server.config.directory);
        }
        match status {
            Ok(Some(status)) => {
                self.child = None;
                Status::Exited {
                    status: status.to_string(),
                    success: status.success(),
                }
            }
            Ok(None) => Status::Running,
            Err(error) => {
                // Reap an uninspectable child before replacement so two processes
                // can never be owned for the same managed backend.
                if let Some(mut process) = self.child.take() {
                    let _ = kill(&mut process).await;
                    let _ = process.wait().await;
                }
                Status::Broken(error.to_string())
            }
        }
    }

    async fn stop(&mut self, server: &Server, grace: Duration) -> io::Result<()> {
        let directory = server.config.directory.clone();
        let result = if let Some(process) = self.adopted.take() {
            let directory = directory.clone();
            blocking(move || process.stop(&directory, grace)).await
        } else if let Some(process) = self.child.take() {
            if server.config.keep_running {
                // A kept server reads its console from the FIFO, not a pipe.
                let directory = directory.clone();
                let _ = blocking(move || kept::write_console(&directory, "stop")).await;
            }
            stop_child(process, grace).await
        } else {
            return Ok(());
        };
        if server.config.keep_running && result.is_ok() {
            kept::forget(&directory);
        }
        result
    }

    async fn kill(&mut self, server: &Server) {
        if let Some(process) = self.adopted.take() {
            let _ = blocking(move || process.kill()).await;
        } else if let Some(mut process) = self.child.take() {
            let _ = kill(&mut process).await;
            let _ = process.wait().await;
        } else {
            return;
        }
        if server.config.keep_running {
            kept::forget(&server.config.directory);
        }
    }

    async fn send_command(&mut self, server: &Server, command: &str) -> io::Result<()> {
        if server.config.keep_running && self.owns() {
            let (directory, command) = (server.config.directory.clone(), command.to_owned());
            return blocking(move || kept::write_console(&directory, &command)).await;
        }
        write_console(&mut self.child, command).await
    }

    fn adoptable(&mut self, server: &Server) -> Option<Option<u32>> {
        if self.searched || self.owns() || !server.config.keep_running {
            return None;
        }
        self.candidate = kept::recorded(&server.config.directory);
        // A record that names no running process of this server is stale.
        self.searched = self.candidate.is_none();
        self.candidate.as_ref().map(|process| Some(process.pid()))
    }

    fn adopt(&mut self, _server: &Server, _pid: Option<u32>) {
        self.adopted = self.candidate.take();
        self.searched = true;
    }

    fn survives_controller(&self, server: &Server) -> bool {
        server.config.keep_running && !server.stop_kept.load(Ordering::Acquire)
    }
}

/// Whether a readiness probe reached a listening server.
///
/// A connect can report success for a connection the server refused: on a
/// busy macOS host the refusal may already have closed the socket, with no
/// error left to report, by the time the runtime checks it. Only a stream
/// that still has a peer counts, or a server would be marked running before
/// it listens.
fn accepted(connect: Result<io::Result<TcpStream>, tokio::time::error::Elapsed>) -> bool {
    matches!(connect, Ok(Ok(stream)) if stream.peer_addr().is_ok())
}

/// Held while a child is spawned and while a port is probed with a listener.
///
/// On macOS a new socket is marked close-on-exec only after it is created, so
/// a child spawned by another thread in between inherits it. A probe listener
/// inherited that way stays open, listening on the probed port, for the
/// child's whole life: the port's own server then cannot bind it, and its
/// readiness probes reach the wrong process.
static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Spawn `command` in `directory`, appending stdout and stderr to its log.
/// Shared with the node agent, which runs the same launch locally.
///
/// The child's environment follows `environment` (see [`apply_environment`]),
/// and never contains Rift's own secrets.
///
/// The child leads its own process group, so a kill also reaches helper
/// processes a start script left behind. `kill_on_drop` covers normal exits
/// only: after SIGKILL or the OOM killer an orphaned server would keep its
/// port, and the next start would refuse to take the port over. On Linux the
/// kernel therefore kills the child when Rift dies (`PR_SET_PDEATHSIG`).
pub(crate) fn spawn(
    command: &[String],
    directory: &std::path::Path,
    environment: &ProcessEnvironment,
    isolation: &crate::config::Isolation,
    memory_limit: Option<(&str, crate::config::MemoryLimit)>,
) -> io::Result<Child> {
    spawn_with(
        command,
        directory,
        environment,
        isolation,
        memory_limit,
        None,
    )
}

/// [`spawn`], or with `console` a server that keeps running without Rift
/// (`keep_running`): it reads `console` (see [`kept::console`]) instead of a
/// pipe, leads its own session instead of only its own group, and neither a
/// dropped handle nor Rift's death kills it. Its log, isolation and memory
/// limit (`memory_limit`, with the server's name) are the same.
fn spawn_with(
    command: &[String],
    directory: &std::path::Path,
    environment: &ProcessEnvironment,
    isolation: &crate::config::Isolation,
    memory_limit: Option<(&str, crate::config::MemoryLimit)>,
    console: Option<std::fs::File>,
) -> io::Result<Child> {
    let (program, arguments) = command.split_first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed server command is empty",
        )
    })?;
    super::rotate_log_reporting(directory);
    let log = open_log(directory)?;
    let kept = console.is_some();
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(directory)
        .stdin(console.map_or_else(Stdio::piped, Stdio::from))
        .stderr(Stdio::from(log.try_clone()?))
        .stdout(Stdio::from(log))
        .kill_on_drop(!kept);
    apply_environment(command.as_std_mut(), environment);
    #[cfg(unix)]
    if !kept {
        command.process_group(0);
    }
    // Built before fork; it lives until this function returns, after the
    // child has exec'd.
    let _sandbox = sandbox(isolation, directory, &arguments_with(program, arguments))?;
    let _group = memory_group(memory_limit, directory)?;
    #[cfg(target_os = "linux")]
    {
        let parent = std::process::id() as libc::pid_t;
        let ruleset = _sandbox.as_ref().map(super::landlock::Sandbox::raw);
        let group = _group.as_ref().map(super::cgroup::Group::raw);
        // SAFETY: runs in the forked child before exec and only makes
        // async-signal-safe calls, building errors without allocating.
        unsafe {
            command.pre_exec(move || {
                if kept {
                    // A new session is also a new process group.
                    if libc::setsid() < 0 {
                        return Err(io::Error::last_os_error());
                    }
                } else {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    // Rift may have died before the signal was armed.
                    if libc::getppid() != parent {
                        return Err(io::Error::from_raw_os_error(libc::ESRCH));
                    }
                }
                // Before anything else the server could allocate, and before
                // the sandbox, which the open group file is not subject to.
                if let Some(group) = group {
                    super::cgroup::Group::enter(group)?;
                }
                if let Some(ruleset) = ruleset {
                    super::landlock::Sandbox::enter(ruleset)?;
                }
                Ok(())
            });
        }
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    if kept {
        // SAFETY: runs in the forked child before exec and only makes an
        // async-signal-safe call.
        unsafe {
            command.pre_exec(|| {
                // A new session is also a new process group.
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    spawner::spawn(command)
}

/// Open a server's log for appending, refusing anything but a regular file.
///
/// The server owns its directory and may be confined to it, while this
/// process is not. Following a symbolic link it left under the log's name
/// would append its output to a file of its choosing outside the sandbox.
fn open_log(directory: &std::path::Path) -> io::Result<std::fs::File> {
    let path = directory.join(LOG_FILE);
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Non-blocking so a FIFO fails the check below instead of waiting
        // for a reader; the flag does nothing on a regular file.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let refused = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} must be a regular file, not a symbolic link or special file; remove it",
                path.display()
            ),
        )
    };
    let log = options.open(&path).map_err(|error| {
        #[cfg(unix)]
        if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENXIO)) {
            return refused();
        }
        error
    })?;
    if !log.metadata()?.is_file() {
        return Err(refused());
    }
    Ok(log)
}

fn arguments_with(program: &str, arguments: &[String]) -> Vec<String> {
    std::iter::once(program.to_owned())
        .chain(arguments.iter().cloned())
        .collect()
}

#[cfg(target_os = "linux")]
type Sandbox = super::landlock::Sandbox;
/// Nothing to hold where there is no Landlock.
#[cfg(not(target_os = "linux"))]
struct Sandbox;

/// The confinement `isolation` asks for, where the kernel offers it.
/// `"landlock"` without Landlock runs the server unconfined and says why;
/// `"required"` refuses to start it.
fn sandbox(
    isolation: &crate::config::Isolation,
    directory: &std::path::Path,
    command: &[String],
) -> io::Result<Option<Sandbox>> {
    use crate::config::IsolationMode;
    if isolation.mode == IsolationMode::Off {
        return Ok(None);
    }
    #[cfg(target_os = "linux")]
    let available = super::landlock::abi();
    #[cfg(not(target_os = "linux"))]
    let available: Result<u32, String> = {
        let _ = command;
        Err("Landlock exists only on Linux".to_owned())
    };
    match available {
        #[cfg(target_os = "linux")]
        Ok(abi) => {
            let directory = std::path::absolute(directory)?;
            super::landlock::Sandbox::new(
                abi,
                &directory,
                command,
                &isolation.read,
                &isolation.write,
            )
            .map(Some)
        }
        #[cfg(not(target_os = "linux"))]
        Ok(_) => Ok(None),
        Err(reason) if isolation.mode == IsolationMode::Required => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("isolation = \"required\", but {reason}"),
        )),
        Err(reason) => {
            log_warn!(
                "starting {} unconfined: isolation = \"landlock\", but {reason}",
                directory.display()
            );
            Ok(None)
        }
    }
}

#[cfg(target_os = "linux")]
type Group = super::cgroup::Group;
/// Nothing to hold where there are no cgroups.
#[cfg(not(target_os = "linux"))]
struct Group;

/// The cgroup `memory_limit` asks for, where Rift can manage one. Without
/// one the server starts unlimited and Rift says why; a `required` limit
/// refuses to start it.
fn memory_group(
    memory_limit: Option<(&str, crate::config::MemoryLimit)>,
    directory: &std::path::Path,
) -> io::Result<Option<Group>> {
    let Some((server, limit)) = memory_limit else {
        return Ok(None);
    };
    #[cfg(target_os = "linux")]
    let group = super::cgroup::Group::new(server, limit.megabytes);
    #[cfg(not(target_os = "linux"))]
    let group: Result<Group, String> = {
        let _ = server;
        Err("cgroups exist only on Linux".to_owned())
    };
    match group {
        Ok(group) => Ok(Some(group)),
        Err(reason) if limit.required => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("memory_limit is required, but {reason}"),
        )),
        Err(reason) => {
            log_warn!(
                "starting {} without its memory limit of {} MiB: {reason}",
                directory.display(),
                limit.megabytes
            );
            Ok(None)
        }
    }
}

/// Give a child Rift's environment minus Rift's secrets, or only the
/// variables `inherit_env` lists, then the `env` entries on top.
///
/// The default edits the inherited environment instead of rebuilding it, so
/// `PATH` lookup of the program and everything else behaves exactly as it
/// did before secrets were removed. A restricted environment is built from
/// scratch; without `PATH` in it the program is looked up in the system's
/// default path, so a restricted server should keep `PATH` or name an
/// absolute executable.
pub(crate) fn apply_environment(
    command: &mut std::process::Command,
    environment: &ProcessEnvironment,
) {
    match &environment.inherit {
        None => {
            for name in crate::environment::protected() {
                command.env_remove(name);
            }
        }
        Some(names) => {
            command.env_clear();
            for name in names {
                if crate::environment::is_protected(name) {
                    continue;
                }
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
        }
    }
    command.envs(&environment.set);
}

/// Spawns every child from one thread that lives as long as the process.
///
/// `PR_SET_PDEATHSIG` fires when the spawning *thread* exits, not the
/// process. Tokio retires idle blocking threads, and `block_in_place` can turn
/// a worker into one, so a child spawned from a runtime thread could be
/// killed long before Rift exits.
mod spawner {
    use std::{
        io,
        sync::{OnceLock, mpsc},
    };
    use tokio::process::{Child, Command};

    type Job = Box<dyn FnOnce() + Send>;

    pub(super) fn spawn(mut command: Command) -> io::Result<Child> {
        static JOBS: OnceLock<io::Result<mpsc::Sender<Job>>> = OnceLock::new();
        let jobs = JOBS
            .get_or_init(|| {
                let (sender, receiver) = mpsc::channel::<Job>();
                std::thread::Builder::new()
                    .name("rift-spawner".into())
                    .spawn(move || {
                        for job in receiver {
                            job();
                        }
                    })
                    .map(|_| sender)
            })
            .as_ref()
            .map_err(|error| io::Error::new(error.kind(), error.to_string()))?;
        // The child registers with the caller's runtime, which reaps it.
        let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        let (result, received) = mpsc::sync_channel(1);
        let stopped = || io::Error::other("the process spawner thread stopped");
        jobs.send(Box::new(move || {
            let _runtime = runtime.enter();
            let _spawning = super::SPAWN.lock().unwrap_or_else(|e| e.into_inner());
            let _ = result.send(command.spawn());
        }))
        .map_err(|_| stopped())?;
        received.recv().map_err(|_| stopped())?
    }
}

/// Kill `process` together with its process group, then reap it.
pub(crate) async fn kill(process: &mut Child) -> io::Result<()> {
    #[cfg(unix)]
    if let Some(pid) = process.id() {
        // The group is the child's own (see `spawn`). Its id stays reserved
        // until the leader is reaped below.
        // SAFETY: a plain syscall without memory arguments.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }
    process.kill().await
}

/// Graceful `stop` on stdin, then kill and reap after `grace`.
pub(crate) async fn stop_child(mut process: Child, grace: Duration) -> io::Result<()> {
    let graceful = async {
        if let Some(mut stdin) = process.stdin.take() {
            let _ = stdin.write_all(b"stop\n").await;
            let _ = stdin.shutdown().await;
        }
        process.wait().await
    };
    match tokio::time::timeout(grace, graceful).await {
        Ok(result) => result.map(|_| ()),
        Err(_) => {
            let result = kill(&mut process).await;
            // `kill` normally waits too; explicitly reap on error as well.
            let waited = process.wait().await;
            result.and(waited.map(|_| ()))
        }
    }
}

pub(crate) async fn write_console(child: &mut Option<Child>, command: &str) -> io::Result<()> {
    let stdin = child
        .as_mut()
        .and_then(|process| process.stdin.as_mut())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "managed server console is unavailable",
            )
        })?;
    let write = async {
        stdin.write_all(command.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await
    };
    tokio::time::timeout(Duration::from_secs(2), write)
        .await
        .unwrap_or_else(|_| {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "server console write timed out",
            ))
        })
}

/// Deadline for quick agent commands; stops add the server's stop timeout.
const COMMAND_WAIT: Duration = Duration::from_secs(15);

/// A server process run by a node agent.
#[derive(Debug)]
pub(super) struct Remote {
    nodes: Nodes,
    node: String,
    /// The process this worker started or adopted, if any.
    running: Option<Option<u32>>,
    /// The agent that runs `running`; a different agent means it restarted.
    agent: Option<String>,
    /// Snapshots older than the latest command reply are stale.
    revision: u64,
    logins: super::LoginSlot,
}

impl Remote {
    pub(super) fn new(nodes: Nodes, node: String, logins: super::LoginSlot) -> Self {
        Self {
            nodes,
            node,
            running: None,
            agent: None,
            revision: 0,
            logins,
        }
    }

    async fn request(
        &mut self,
        command: protocol::Command,
        wait: Duration,
    ) -> io::Result<serde_json::Value> {
        let result = self.nodes.request(&self.node, &command, wait).await?;
        // Replies carry the agent identity and process revision after the change.
        if let Some(agent) = result["agent"].as_str() {
            if self.agent.as_deref() != Some(agent) {
                self.revision = 0;
            }
            self.agent = Some(agent.to_owned());
        }
        if let Some(revision) = result["revision"].as_u64() {
            self.revision = self.revision.max(revision);
        }
        Ok(result)
    }

    /// The node's latest report, unless it predates our last command.
    fn current(&self, server: &Server) -> Option<crate::node::Observation> {
        let observation = self.nodes.observe(&self.node, &server.name)?;
        let same_agent = self.agent.as_deref() == Some(observation.agent.as_str());
        (!same_agent || observation.revision >= self.revision).then_some(observation)
    }
}

impl ProcessBackend for Remote {
    fn owns(&self) -> bool {
        self.running.is_some()
    }

    fn pid(&self) -> Option<u32> {
        self.running.flatten()
    }

    fn available(&self) -> io::Result<()> {
        if self.nodes.online(&self.node) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotConnected,
                format!("node {} is unreachable", self.node),
            ))
        }
    }

    async fn start(&mut self, server: &Server) -> io::Result<()> {
        let endpoint = server.endpoint.as_deref().unwrap_or_default();
        let port = endpoint
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "remote server has no port")
            })?;
        // The token crosses the bus in the start command, the one secret
        // that does: it is scoped to this instance, the node never gets the
        // key, and every (re)start carries it, so a recovered server has it.
        let (environment, bus_certificate) = with_login(&self.logins, server, None)?;
        let start = protocol::Start {
            server: server.name.clone(),
            command: server.config.command.clone(),
            directory: server.config.directory.clone(),
            port,
            environment: (&environment).into(),
            bus_certificate,
            isolation: (&server.config.isolation).into(),
            memory_limit: server.config.memory_limit.map(Into::into),
        };
        // An agent drops fields it does not know. A process it already runs
        // is adopted as it is, so only a new one is checked.
        let running = self
            .nodes
            .observe(&self.node, &server.name)
            .is_some_and(|observation| observation.running.is_some());
        if !running {
            self.nodes.check_start(&self.node, &start)?;
        }
        let result = self
            .request(protocol::Command::Start(start), COMMAND_WAIT)
            .await?;
        self.running = Some(result["pid"].as_u64().map(|pid| pid as u32));
        Ok(())
    }

    async fn ready(&mut self, server: &Server, wait: Duration) -> bool {
        let Some(endpoint) = server.endpoint.as_deref() else {
            return false;
        };
        // The proxy itself must reach the server, so probe from here.
        accepted(tokio::time::timeout(wait, TcpStream::connect(endpoint)).await)
    }

    async fn status(&mut self, server: &Server) -> Status {
        if self.running.is_none() {
            return Status::Idle;
        }
        let Some(observation) = self.nodes.observe(&self.node, &server.name) else {
            return Status::Unknown;
        };
        if self
            .agent
            .as_deref()
            .is_some_and(|agent| agent != observation.agent)
        {
            self.running = None;
            return Status::Exited {
                status: format!(
                    "the agent on node {} restarted and its servers stopped with it",
                    self.node
                ),
                success: false,
            };
        }
        if observation.revision < self.revision || observation.running.is_some() {
            return Status::Running;
        }
        self.running = None;
        match observation.exit {
            Some((exit, success)) => Status::Exited {
                status: format!("{exit} on node {}", self.node),
                success,
            },
            None => Status::Exited {
                status: format!("no longer running on node {}", self.node),
                success: false,
            },
        }
    }

    async fn stop(&mut self, server: &Server, grace: Duration) -> io::Result<()> {
        // Forget the process either way: a later snapshot that still lists
        // it is reconciled through adoption against the administrator intent.
        self.running = None;
        self.request(
            protocol::Command::Stop {
                server: server.name.clone(),
                timeout_ms: grace.as_millis().min(u64::MAX as u128) as u64,
            },
            grace.saturating_add(COMMAND_WAIT),
        )
        .await
        .map(|_| ())
    }

    async fn kill(&mut self, server: &Server) {
        let _ = self.stop(server, Duration::ZERO).await;
    }

    async fn send_command(&mut self, server: &Server, command: &str) -> io::Result<()> {
        self.request(
            protocol::Command::Console {
                server: server.name.clone(),
                command: command.to_owned(),
            },
            COMMAND_WAIT,
        )
        .await
        .map(|_| ())
    }

    fn adoptable(&mut self, server: &Server) -> Option<Option<u32>> {
        if self.running.is_some() {
            return None;
        }
        let observation = self.current(server)?;
        let pid = observation.running?;
        Some(pid)
    }

    fn adopt(&mut self, server: &Server, pid: Option<u32>) {
        self.running = Some(pid);
        if let Some(observation) = self.nodes.observe(&self.node, &server.name) {
            self.agent = Some(observation.agent);
            self.revision = self.revision.max(observation.revision);
        }
    }

    fn survives_controller(&self, _server: &Server) -> bool {
        true
    }
}

/// Keep `server`'s phase consistent with an unreachable node.
pub(super) fn mark_unknown(server: &Server) {
    let mut state = lock(&server.state);
    if state.phase == super::Phase::Running {
        state.phase = super::Phase::Unknown;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn the_log_is_never_opened_through_a_link_or_special_file() {
        let directory = std::env::temp_dir().join(format!("rift-log-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let victim = directory.join("victim");
        std::fs::write(&victim, "untouched").unwrap();
        let log = directory.join(LOG_FILE);
        std::os::unix::fs::symlink(&victim, &log).unwrap();
        let command = ["sh", "-c", "echo escaped"].map(String::from);
        let error = spawn(
            &command,
            &directory,
            &ProcessEnvironment::default(),
            &Default::default(),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("must be a regular file"));
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
        // A link to nothing is not created through either.
        std::fs::remove_file(&log).unwrap();
        std::os::unix::fs::symlink(directory.join("absent"), &log).unwrap();
        assert!(open_log(&directory).is_err());
        assert!(!directory.join("absent").exists());
        // A FIFO is refused without waiting for a reader.
        std::fs::remove_file(&log).unwrap();
        let name = std::ffi::CString::new(log.to_str().unwrap()).unwrap();
        // SAFETY: `name` is a valid C string.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(open_log(&directory).is_err());
        std::fs::remove_file(&log).unwrap();
        assert!(open_log(&directory).unwrap().metadata().unwrap().is_file());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    /// Whether `pid` has exited (gone, or a zombie awaiting its new parent).
    fn exited(pid: &str) -> bool {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&output.stdout);
        state.trim().is_empty() || state.trim_start().starts_with('Z')
    }

    #[test]
    fn full_logs_rotate_in_place_for_an_appending_writer() {
        use std::io::Write as _;
        let directory =
            std::env::temp_dir().join(format!("rift-log-rotation-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let (log, rotated) = (
            directory.join(LOG_FILE),
            directory.join(crate::managed::ROTATED_LOG_FILE),
        );
        assert!(!crate::managed::rotate_log(&directory, 8).unwrap());
        // The child keeps appending through the descriptor it was given.
        let mut child = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        child.write_all(b"short").unwrap();
        assert!(!crate::managed::rotate_log(&directory, 8).unwrap());
        child.write_all(b" and long").unwrap();
        assert!(crate::managed::rotate_log(&directory, 8).unwrap());
        assert_eq!(std::fs::read(&rotated).unwrap(), b"short and long");
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0);
        child.write_all(b"second file").unwrap();
        assert_eq!(std::fs::read(&log).unwrap(), b"second file");
        // The next rotation replaces the previous copy.
        assert!(crate::managed::rotate_log(&directory, 8).unwrap());
        assert_eq!(std::fs::read(&rotated).unwrap(), b"second file");
        // A link planted where the log belongs is never followed or emptied.
        #[cfg(unix)]
        {
            let outside = directory.join("outside");
            std::fs::write(&outside, b"not a server log").unwrap();
            std::fs::remove_file(&log).unwrap();
            std::os::unix::fs::symlink(&outside, &log).unwrap();
            assert!(crate::managed::rotate_log(&directory, 8).is_err());
            assert_eq!(std::fs::read(&outside).unwrap(), b"not a server log");
        }
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn children_lead_their_own_group_and_kill_reaches_helpers() {
        let directory =
            std::env::temp_dir().join(format!("rift-spawn-group-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let command = ["sh", "-c", "sleep 60 & echo $! > helper.pid; wait"].map(String::from);
        // Spawning from a short-lived blocking thread works like any other.
        let mut child = {
            let directory = directory.clone();
            tokio::task::spawn_blocking(move || {
                spawn(
                    &command,
                    &directory,
                    &ProcessEnvironment::default(),
                    &Default::default(),
                    None,
                )
            })
            .await
            .unwrap()
            .unwrap()
        };
        let pid = child.id().unwrap() as libc::pid_t;
        // SAFETY: a plain syscall without memory arguments.
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);
        let helper = loop {
            match std::fs::read_to_string(directory.join("helper.pid")) {
                Ok(text) if text.ends_with('\n') => break text.trim().to_owned(),
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        };
        assert!(!exited(&helper));
        kill(&mut child).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !exited(&helper) {
            assert!(tokio::time::Instant::now() < deadline, "helper survived");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = std::fs::remove_dir_all(directory);
    }

    /// A spawned server sees neither the admin token nor any other secret
    /// the configuration names, but keeps every other inherited variable.
    /// The test binary runs this test again with the variables set, as a
    /// stand-in for Rift started with secrets in its environment (setting
    /// them here would race other test threads reading the environment).
    #[test]
    fn children_do_not_inherit_rifts_secrets() {
        const STAND_IN: &str = "RIFT_TEST_SECRETS_DIRECTORY";
        const ADMIN: &str = "RIFT_TEST_SPAWN_ADMIN_TOKEN";
        const NODE: &str = "RIFT_TEST_SPAWN_FORWARDING_SECRET";
        const UNRELATED: &str = "RIFT_TEST_SPAWN_DATABASE_PASSWORD";
        let name = format!(
            "{}::children_do_not_inherit_rifts_secrets",
            module_path!().split_once("::").unwrap().1
        );
        if let Ok(directory) = std::env::var(STAND_IN) {
            let directory = std::path::PathBuf::from(directory);
            crate::environment::harden_process();
            let config = crate::config::Config::from_lua(&format!(
                "return {{ listeners = {{ public = '127.0.0.1:0' }}, backends = {{ lobby = '127.0.0.1:25001' }}, \
                 routes = {{ public = 'lobby' }}, admin = {{ listen = '127.0.0.1:0', token_env = '{ADMIN}', permissions = {{ 'status' }} }}, \
                 authentication = {{ online_mode = true }}, forwarding = {{ mode = 'velocity', secret_env = '{NODE}' }} }}"
            ), "secrets.lua")
            .unwrap();
            super::super::protect_secrets(&config);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let report = |file: &str| {
                    [
                        "sh".to_owned(),
                        "-c".to_owned(),
                        format!(
                            "env > {file}.tmp; cat /proc/$PPID/environ > {file}.proc 2>/dev/null; echo $? > {file}.status; mv {file}.tmp {file}"
                        ),
                    ]
                };
                let mut restricted = ProcessEnvironment {
                    inherit: Some([UNRELATED.to_owned(), "PATH".to_owned()].into()),
                    ..Default::default()
                };
                restricted.set.insert("RIFT_TEST_EXTRA".into(), "added".into());
                for (file, environment) in [
                    ("default", ProcessEnvironment::default()),
                    ("restricted", restricted),
                ] {
                    let mut child =
                        spawn(&report(file), &directory, &environment, &Default::default(), None).unwrap();
                    child.wait().await.unwrap();
                }
            });
            return;
        }
        let directory =
            std::env::temp_dir().join(format!("rift-spawn-secrets-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--test-threads=1", "--nocapture"])
            .env(STAND_IN, &directory)
            .env(ADMIN, "admin-secret-0123456789abcdef0123456789")
            .env(NODE, "node-secret-0123456789abcdef0123456789ab")
            .env(UNRELATED, "keep-me")
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "stand-in Rift failed: {status}");
        let read = |file: &str| std::fs::read_to_string(directory.join(file)).unwrap();
        let default = read("default");
        assert!(
            !default.contains(ADMIN),
            "the admin token leaked:\n{default}"
        );
        assert!(!default.contains("admin-secret"), "the admin token leaked");
        assert!(!default.contains(NODE), "the forwarding secret leaked");
        assert!(
            default.contains(&format!("{UNRELATED}=keep-me")),
            "{default}"
        );
        assert!(default.lines().any(|line| line.starts_with("PATH=")));
        let restricted = read("restricted");
        assert!(restricted.contains(&format!("{UNRELATED}=keep-me")));
        assert!(restricted.contains("RIFT_TEST_EXTRA=added"));
        assert!(!restricted.contains(ADMIN));
        assert!(
            !restricted.contains("HOME="),
            "inherit_env must drop unlisted variables"
        );
        // A same-user process cannot read a hardened Rift's environment
        // through /proc either (root can, through CAP_SYS_PTRACE).
        #[cfg(target_os = "linux")]
        // SAFETY: a plain syscall without arguments.
        if unsafe { libc::geteuid() } != 0 {
            assert_ne!(
                read("default.status").trim(),
                "0",
                "/proc/<rift>/environ was readable"
            );
            assert!(!read("default.proc").contains("admin-secret"));
        }
        let _ = std::fs::remove_dir_all(directory);
    }

    /// Where Landlock is missing, `"landlock"` starts the server unconfined
    /// and `"required"` refuses to start it.
    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn isolation_falls_back_or_refuses_without_landlock() {
        use crate::config::{Isolation, IsolationMode};
        let directory = std::env::temp_dir().join(format!("rift-isolation-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let command = ["true".to_owned()];
        let isolation = |mode| Isolation {
            mode,
            ..Default::default()
        };
        let mut child = spawn(
            &command,
            &directory,
            &ProcessEnvironment::default(),
            &isolation(IsolationMode::Landlock),
            None,
        )
        .unwrap();
        assert!(child.wait().await.unwrap().success());
        let refused = spawn(
            &command,
            &directory,
            &ProcessEnvironment::default(),
            &isolation(IsolationMode::Required),
            None,
        )
        .unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::Unsupported);
        assert!(
            refused.to_string().contains("isolation = \"required\""),
            "{refused}"
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    /// Where Rift cannot manage a cgroup, a memory limit starts the server
    /// unlimited and a required one refuses to start it.
    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn a_memory_limit_falls_back_or_refuses_without_cgroups() {
        use crate::config::MemoryLimit;
        let directory = std::env::temp_dir().join(format!("rift-memory-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let command = ["true".to_owned()];
        let limit = |required| MemoryLimit {
            megabytes: 256,
            required,
        };
        let mut child = spawn(
            &command,
            &directory,
            &ProcessEnvironment::default(),
            &Default::default(),
            Some(("survival", limit(false))),
        )
        .unwrap();
        assert!(child.wait().await.unwrap().success());
        let refused = spawn(
            &command,
            &directory,
            &ProcessEnvironment::default(),
            &Default::default(),
            Some(("survival", limit(true))),
        )
        .unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::Unsupported);
        assert!(
            refused.to_string().contains("memory_limit is required"),
            "{refused}"
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    /// With a delegated cgroup, a limited server runs in a group of its own
    /// and the kernel kills it at the limit. Without one (most development
    /// shells and CI runners) only the refusal of a required limit is
    /// checked. Run it delegated with
    /// `systemd-run --user --scope -p Delegate=yes cargo test memory_limit`.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_memory_limit_confines_the_server_to_its_own_group() {
        use crate::config::MemoryLimit;
        use std::os::unix::process::ExitStatusExt;
        let directory = std::env::temp_dir().join(format!("rift-memory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let name = format!("test-{}", std::process::id());
        let limit = MemoryLimit {
            megabytes: 64,
            required: true,
        };
        let run = |script: &str| {
            spawn(
                &["sh".to_owned(), "-c".to_owned(), script.to_owned()],
                &directory,
                &ProcessEnvironment::default(),
                &Default::default(),
                Some((&name, limit)),
            )
        };
        if let Err(reason) = super::super::cgroup::describe() {
            let refused = run("true").unwrap_err();
            assert_eq!(refused.kind(), io::ErrorKind::Unsupported, "{refused}");
            eprintln!("skipping the enforcement half: {reason}");
            let _ = std::fs::remove_dir_all(directory);
            return;
        }
        let mut child = run("cat /proc/self/cgroup > group.txt; \
             cat \"/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)/memory.max\" > max.txt")
        .unwrap();
        assert!(child.wait().await.unwrap().success());
        let group = std::fs::read_to_string(directory.join("group.txt")).unwrap();
        assert!(
            group.trim_end().ends_with(&format!("/rift-server-{name}")),
            "{group}"
        );
        assert_eq!(
            std::fs::read_to_string(directory.join("max.txt")).unwrap(),
            format!("{}\n", 64 * 1024 * 1024)
        );
        // `tail` buffers a stream without newlines until memory runs out.
        let mut child = run("exec tail /dev/zero").unwrap();
        let status = tokio::time::timeout(Duration::from_secs(60), child.wait())
            .await
            .expect("the kernel kills the server at its limit")
            .unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");
        let _ = std::fs::remove_dir_all(directory);
    }

    /// A server outlives neither a SIGKILLed Rift nor an OOM kill. The test
    /// binary runs this test again as a stand-in for Rift, which spawns a
    /// server and waits; the outer run then kills it with SIGKILL.
    #[cfg(target_os = "linux")]
    #[test]
    fn children_die_with_a_killed_rift() {
        const PARENT: &str = "RIFT_TEST_KILLED_PARENT";
        let name = format!(
            "{}::children_die_with_a_killed_rift",
            module_path!().split_once("::").unwrap().1
        );
        if let Ok(directory) = std::env::var(PARENT) {
            let directory = std::path::PathBuf::from(directory);
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let child = spawn(
                    &["sleep".into(), "60".into()],
                    &directory,
                    &ProcessEnvironment::default(),
                    &Default::default(),
                    None,
                )
                .unwrap();
                let temporary = directory.join("child.pid.tmp");
                std::fs::write(&temporary, child.id().unwrap().to_string()).unwrap();
                std::fs::rename(temporary, directory.join("child.pid")).unwrap();
                std::future::pending::<()>().await;
            });
            unreachable!();
        }
        let directory =
            std::env::temp_dir().join(format!("rift-killed-parent-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut parent = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--test-threads=1", "--nocapture"])
            .env(PARENT, &directory)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let child = loop {
            if let Ok(pid) = std::fs::read_to_string(directory.join("child.pid")) {
                break pid;
            }
            if let Some(status) = parent.try_wait().unwrap() {
                panic!("stand-in Rift exited early: {status}");
            }
            assert!(std::time::Instant::now() < deadline, "no child spawned");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(!exited(&child));
        parent.kill().unwrap(); // SIGKILL: no destructors, no kill_on_drop.
        parent.wait().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !exited(&child) {
            assert!(
                std::time::Instant::now() < deadline,
                "server {child} survived a SIGKILLed Rift"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(directory);
    }
}
