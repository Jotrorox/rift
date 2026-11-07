//! Process handling behind the supervision worker: a child of this process
//! ([`Local`]) or a process run by a `rift node` agent ([`Remote`]). The worker
//! keeps phases, retries, readiness deadlines and idle stops for both.
use std::{fs::OpenOptions, io, process::Stdio, time::Duration};

use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    process::{Child, Command},
};

use super::{LOG_FILE, Server, lock, loopback_address};
use crate::node::{Nodes, protocol};

/// What a status check found.
#[derive(Debug)]
pub(super) enum Status {
    /// No process is owned.
    Idle,
    Running,
    /// The process ended; the text describes its exit status.
    Exited(String),
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
    /// remote servers keep running while the controller restarts.
    fn adoptable(&self, server: &Server) -> Option<Option<u32>>;
    fn adopt(&mut self, server: &Server, pid: Option<u32>);
    /// On controller shutdown, local processes stop; remote ones keep running
    /// and are adopted again after the controller reconnects.
    fn survives_controller(&self) -> bool;
}

#[derive(Debug, Default)]
pub(super) struct Local {
    child: Option<Child>,
}

impl ProcessBackend for Local {
    fn owns(&self) -> bool {
        self.child.is_some()
    }

    fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
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
        self.child = Some(spawn(&server.config.command, &server.config.directory)?);
        Ok(())
    }

    async fn ready(&mut self, server: &Server, wait: Duration) -> bool {
        let Ok(address) = loopback_address(server.address) else {
            return false;
        };
        accepted(tokio::time::timeout(wait, TcpStream::connect(address)).await)
    }

    async fn status(&mut self, _server: &Server) -> Status {
        let Some(process) = self.child.as_mut() else {
            return Status::Idle;
        };
        match process.try_wait() {
            Ok(Some(status)) => {
                self.child = None;
                Status::Exited(status.to_string())
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

    async fn stop(&mut self, _server: &Server, grace: Duration) -> io::Result<()> {
        match self.child.take() {
            Some(process) => stop_child(process, grace).await,
            None => Ok(()),
        }
    }

    async fn kill(&mut self, _server: &Server) {
        if let Some(mut process) = self.child.take() {
            let _ = kill(&mut process).await;
            let _ = process.wait().await;
        }
    }

    async fn send_command(&mut self, _server: &Server, command: &str) -> io::Result<()> {
        write_console(&mut self.child, command).await
    }

    fn adoptable(&self, _server: &Server) -> Option<Option<u32>> {
        None
    }

    fn adopt(&mut self, _server: &Server, _pid: Option<u32>) {}

    fn survives_controller(&self) -> bool {
        false
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
/// The child leads its own process group, so a kill also reaches helper
/// processes a start script left behind. `kill_on_drop` covers normal exits
/// only: after SIGKILL or the OOM killer an orphaned server would keep its
/// port, and the next start would refuse to take the port over. On Linux the
/// kernel therefore kills the child when Rift dies (`PR_SET_PDEATHSIG`).
pub(crate) fn spawn(command: &[String], directory: &std::path::Path) -> io::Result<Child> {
    let (program, arguments) = command.split_first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed server command is empty",
        )
    })?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join(LOG_FILE))?;
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(directory)
        .stdin(Stdio::piped())
        .stderr(Stdio::from(log.try_clone()?))
        .stdout(Stdio::from(log))
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(target_os = "linux")]
    {
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: runs in the forked child before exec and only makes
        // async-signal-safe calls, building errors without allocating.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(io::Error::last_os_error());
                }
                // Rift may have died before the signal was armed.
                if libc::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
    }
    spawner::spawn(command)
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
}

impl Remote {
    pub(super) fn new(nodes: Nodes, node: String) -> Self {
        Self {
            nodes,
            node,
            running: None,
            agent: None,
            revision: 0,
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
        let result = self
            .request(
                protocol::Command::Start(protocol::Start {
                    server: server.name.clone(),
                    command: server.config.command.clone(),
                    directory: server.config.directory.clone(),
                    port,
                }),
                COMMAND_WAIT,
            )
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
            return Status::Exited(format!(
                "the agent on node {} restarted and its servers stopped with it",
                self.node
            ));
        }
        if observation.revision < self.revision || observation.running.is_some() {
            return Status::Running;
        }
        self.running = None;
        Status::Exited(observation.exit.map_or_else(
            || format!("no longer running on node {}", self.node),
            |exit| format!("{exit} on node {}", self.node),
        ))
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

    fn adoptable(&self, server: &Server) -> Option<Option<u32>> {
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

    fn survives_controller(&self) -> bool {
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

    /// Whether `pid` has exited (gone, or a zombie awaiting its new parent).
    fn exited(pid: &str) -> bool {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&output.stdout);
        state.trim().is_empty() || state.trim_start().starts_with('Z')
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
            tokio::task::spawn_blocking(move || spawn(&command, &directory))
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
                let child = spawn(&["sleep".into(), "60".into()], &directory).unwrap();
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
