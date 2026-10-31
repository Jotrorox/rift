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
        // longer than a short preflight deadline on Windows.
        match std::net::TcpListener::bind(address) {
            Ok(listener) => drop(listener),
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
        matches!(
            tokio::time::timeout(wait, TcpStream::connect(address)).await,
            Ok(Ok(_))
        )
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
                    let _ = process.kill().await;
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
            let _ = process.kill().await;
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

/// Spawn `command` in `directory`, appending stdout and stderr to its log.
/// Shared with the node agent, which runs the same launch locally.
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
    command.spawn()
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
            let result = process.kill().await;
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
        matches!(
            tokio::time::timeout(wait, TcpStream::connect(endpoint)).await,
            Ok(Ok(_))
        )
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
