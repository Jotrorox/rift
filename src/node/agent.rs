//! `rift node`: runs game servers for a controller on another host.
//!
//! The agent connects to the controller's QUIC message bus as principal
//! `node.<name>`, answers commands on `rift.node.<name>.cmd` and publishes its
//! process list on `rift.node.<name>.state`. It only runs programs from its
//! `--allow-program` list and only touches directories below `--root`, so a
//! stolen node token does not grant arbitrary code execution on this host.
//!
//! If the controller connection drops, servers keep running and the agent
//! reconnects; the controller then reconciles its instance list. If the agent
//! itself exits, its servers stop with it.
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use bytes::Bytes;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    process::Child,
    sync::{Mutex, Notify},
};

use super::protocol::{self, Command, ExitState, NodeState, ProcessState};
use crate::{
    config::{InstanceStorage, valid_node_directory},
    messaging::quic,
    provisioning,
};

pub const USAGE: &str = "Usage: rift node --controller <host:port> --name <node> --root <directory> --ca <certificate.pem> [options]\n\
    Runs game servers for a Rift controller on this host.\n\
    --controller <host:port>  the controller's messaging.listen address (QUIC)\n\
    --name <node>             this node's name in the controller's rift.config.nodes\n\
    --root <directory>        all server and cache directories live below it\n\
    --ca <file>               PEM certificate(s) that sign the controller's messaging certificate\n\
    --token-env <variable>    environment variable with this node's token (default RIFT_NODE_TOKEN)\n\
    --server-name <name>      TLS name of the controller certificate (default: the controller host)\n\
    --web <url>               the controller's web API, e.g. http://10.0.0.2:8080, for templates\n\
    --allow-program <name>    a program servers may run; repeatable (default: java)";

/// Parsed `rift node` arguments.
#[derive(Debug, Clone)]
pub struct Options {
    pub controller: String,
    pub server_name: String,
    pub ca: PathBuf,
    pub name: String,
    pub token: String,
    pub root: PathBuf,
    pub web: Option<String>,
    pub allowed_programs: BTreeSet<String>,
}

impl Options {
    pub fn parse(args: &[String]) -> io::Result<Self> {
        let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidInput, message);
        let mut values: BTreeMap<&str, String> = BTreeMap::new();
        let mut allowed_programs = BTreeSet::new();
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let flag = flag.as_str();
            if !matches!(
                flag,
                "--controller"
                    | "--name"
                    | "--root"
                    | "--ca"
                    | "--token-env"
                    | "--server-name"
                    | "--web"
                    | "--allow-program"
            ) {
                return Err(invalid(format!("unknown option {flag:?}\n{USAGE}")));
            }
            let value = args
                .next()
                .ok_or_else(|| invalid(format!("{flag} requires a value\n{USAGE}")))?
                .clone();
            if flag == "--allow-program" {
                if value.trim().is_empty() {
                    return Err(invalid("--allow-program must not be empty".into()));
                }
                allowed_programs.insert(value);
            } else if values.insert(flag, value).is_some() {
                return Err(invalid(format!("{flag} was given twice")));
            }
        }
        let mut required = |flag: &str| {
            values
                .remove(flag)
                .ok_or_else(|| invalid(format!("{flag} is required\n{USAGE}")))
        };
        let controller = required("--controller")?;
        let name = required("--name")?;
        let root = PathBuf::from(required("--root")?);
        let ca = PathBuf::from(required("--ca")?);
        if !crate::config::NodeConfig::principal(&name)
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
            || name.is_empty()
            || name.len() > 64
            || name.contains('.')
        {
            return Err(invalid(
                "--name must be a node name from rift.config.nodes".into(),
            ));
        }
        let token_env = values
            .remove("--token-env")
            .unwrap_or_else(|| "RIFT_NODE_TOKEN".into());
        let token = std::env::var(&token_env)
            .map_err(|_| invalid(format!("set {token_env} to this node's token")))?;
        if !(32..=1024).contains(&token.len()) || token.chars().any(char::is_control) {
            return Err(invalid(
                "node token must contain 32..=1024 bytes without control characters".into(),
            ));
        }
        let host = controller
            .rsplit_once(':')
            .map(|(host, _)| host.trim_matches(['[', ']']).to_owned())
            .ok_or_else(|| invalid("--controller must be host:port".into()))?;
        let server_name = values.remove("--server-name").unwrap_or(host);
        let web = values
            .remove("--web")
            .map(|web| web.trim_end_matches('/').to_owned());
        if let Some(web) = &web
            && reqwest::Url::parse(web)
                .ok()
                .is_none_or(|url| !matches!(url.scheme(), "http" | "https"))
        {
            return Err(invalid("--web must be an http:// or https:// URL".into()));
        }
        if allowed_programs.is_empty() {
            allowed_programs.insert("java".into());
        }
        Ok(Self {
            controller,
            server_name,
            ca,
            name,
            token,
            root,
            web,
            allowed_programs,
        })
    }
}

/// Run until SIGINT or SIGTERM, then stop all servers gracefully.
pub async fn run(options: Options) -> io::Result<()> {
    let agent = Agent::new(options)?;
    let tls = quic::tls_client_config(&agent.options.ca)?;
    eprintln!(
        "rift: node {} serving {} for controller {}",
        agent.options.name,
        agent.root.display(),
        agent.options.controller
    );
    let monitor = tokio::spawn(agent.clone().monitor());
    tokio::select! {
        () = agent.clone().connect_forever(tls) => {}
        () = shutdown_signal() => {}
    }
    monitor.abort();
    eprintln!("rift: node {} stopping its servers", agent.options.name);
    agent.stop_all().await;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) {
            (Ok(mut interrupt), Ok(mut terminate)) => {
                tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
            }
            _ => std::future::pending().await,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Exits remembered for the controller's error messages.
const MAX_EXITS: usize = 64;
/// How long an instance prepared by this agent survives reconciliation
/// without being registered, covering the controller's creation window.
const PREPARE_GRACE: Duration = Duration::from_secs(600);
const SERVER_STOP_GRACE: Duration = Duration::from_secs(30);

pub struct Agent {
    options: Options,
    root: PathBuf,
    id: String,
    processes: Mutex<BTreeMap<String, Process>>,
    revision: AtomicU64,
    exits: StdMutex<VecDeque<ExitState>>,
    changed: Notify,
    /// Serializes filesystem work: prepare, remove and reconcile.
    storage: Mutex<()>,
    prepared: StdMutex<HashMap<String, std::time::Instant>>,
    /// Cached template files by path: size, modification time and SHA-256.
    hashes: StdMutex<HashMap<PathBuf, (u64, SystemTime, String)>>,
    http: reqwest::Client,
}

struct Process {
    child: Option<Child>,
    pid: Option<u32>,
}

impl Agent {
    pub fn new(options: Options) -> io::Result<Arc<Self>> {
        std::fs::create_dir_all(&options.root)?;
        let root = std::fs::canonicalize(&options.root)?;
        let mut random = [0u8; 16];
        aws_lc_rs::rand::fill(&mut random).map_err(|_| io::Error::other("random source failed"))?;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(io::Error::other)?;
        Ok(Arc::new(Self {
            options,
            root,
            id: random.iter().map(|byte| format!("{byte:02x}")).collect(),
            processes: Mutex::new(BTreeMap::new()),
            revision: AtomicU64::new(1),
            exits: StdMutex::new(VecDeque::new()),
            changed: Notify::new(),
            storage: Mutex::new(()),
            prepared: StdMutex::new(HashMap::new()),
            hashes: StdMutex::new(HashMap::new()),
            http,
        }))
    }

    fn bump(&self) -> u64 {
        let revision = self.revision.fetch_add(1, Ordering::AcqRel) + 1;
        self.changed.notify_one();
        revision
    }

    async fn connect_forever(self: Arc<Self>, tls: quinn::ClientConfig) {
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.connect(tls.clone()).await {
                Ok(client) => {
                    eprintln!("rift: node {} connected to controller", self.options.name);
                    backoff = Duration::from_secs(1);
                    let error = self.clone().serve(client).await;
                    eprintln!(
                        "rift: node {} lost the controller connection: {error}; servers keep running",
                        self.options.name
                    );
                }
                Err(error) => eprintln!(
                    "rift: node {} cannot reach controller {}: {error}",
                    self.options.name, self.options.controller
                ),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }

    async fn connect(&self, tls: quinn::ClientConfig) -> io::Result<quic::Client> {
        let address: SocketAddr = tokio::net::lookup_host(&self.options.controller)
            .await?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "controller has no address"))?;
        quic::Client::connect(address, &self.options.server_name, tls, &self.options.token).await
    }

    /// Answer commands and publish state until the connection fails.
    pub async fn serve(self: Arc<Self>, client: quic::Client) -> io::Error {
        let client = Arc::new(client);
        let mut commands = match client
            .subscribe(&protocol::command_subject(&self.options.name), None)
            .await
        {
            Ok(subscription) => subscription,
            Err(error) => return error,
        };
        let publisher = {
            let agent = self.clone();
            let client = client.clone();
            async move {
                loop {
                    let state = agent.state().await;
                    let payload = match serde_json::to_vec(&state) {
                        Ok(payload) => payload,
                        Err(error) => return io::Error::other(error),
                    };
                    if let Err(error) = client
                        .publish(
                            &protocol::state_subject(&agent.options.name),
                            Bytes::from(payload),
                        )
                        .await
                    {
                        return error;
                    }
                    let _ =
                        tokio::time::timeout(protocol::STATE_INTERVAL, agent.changed.notified())
                            .await;
                }
            }
        };
        let receiver = async {
            loop {
                let message = match commands.recv().await {
                    Ok(message) => message,
                    Err(error) => return error,
                };
                let Some(reply) = message.reply.as_deref().map(str::to_owned) else {
                    continue;
                };
                let (agent, client) = (self.clone(), client.clone());
                tokio::spawn(async move {
                    let result = match serde_json::from_slice::<Command>(&message.payload) {
                        Ok(command) => agent.handle(command).await,
                        Err(error) => Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("invalid node command: {error}"),
                        )),
                    };
                    let mut payload = protocol::reply(result).to_string();
                    if payload.len() > protocol::MAX_MESSAGE {
                        payload = protocol::reply(Err(io::Error::new(
                            io::ErrorKind::FileTooLarge,
                            "node reply exceeds the 1 MiB message limit",
                        )))
                        .to_string();
                    }
                    let _ = client.publish(&reply, Bytes::from(payload)).await;
                });
            }
        };
        tokio::select! {
            error = publisher => error,
            error = receiver => error,
        }
    }

    pub async fn state(&self) -> NodeState {
        let processes = self.processes.lock().await;
        NodeState {
            agent: self.id.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
            revision: self.revision.load(Ordering::Acquire),
            servers: processes
                .iter()
                .map(|(name, process)| ProcessState {
                    name: name.clone(),
                    pid: process.pid,
                })
                .collect(),
            exits: self
                .exits
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .cloned()
                .collect(),
        }
    }

    /// Reap servers that exited on their own and report them.
    pub async fn monitor(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let mut exited = Vec::new();
            {
                let mut processes = self.processes.lock().await;
                for (name, process) in processes.iter_mut() {
                    let Some(child) = process.child.as_mut() else {
                        continue;
                    };
                    match child.try_wait() {
                        Ok(Some(status)) => exited.push((name.clone(), status.to_string())),
                        Ok(None) => {}
                        Err(error) => {
                            let _ = child.start_kill();
                            exited.push((name.clone(), format!("cannot inspect process: {error}")));
                        }
                    }
                }
                for (name, _) in &exited {
                    processes.remove(name);
                }
            }
            if !exited.is_empty() {
                let mut exits = self.exits.lock().unwrap_or_else(|e| e.into_inner());
                for (name, status) in exited {
                    eprintln!("rift: node server {name} exited: {status}");
                    exits.retain(|exit| exit.name != name);
                    exits.push_back(ExitState { name, status });
                    while exits.len() > MAX_EXITS {
                        exits.pop_front();
                    }
                }
                drop(exits);
                self.bump();
            }
        }
    }

    async fn stop_all(&self) {
        let children: Vec<_> = std::mem::take(&mut *self.processes.lock().await)
            .into_values()
            .filter_map(|process| process.child)
            .collect();
        let stops = children
            .into_iter()
            .map(|child| crate::managed::stop_child(child, SERVER_STOP_GRACE));
        for result in futures_join(stops).await {
            if let Err(error) = result {
                eprintln!("rift: stopping a server failed: {error}");
            }
        }
    }

    pub async fn handle(&self, command: Command) -> io::Result<Value> {
        match command {
            Command::Start(start) => self.start(start).await,
            Command::Stop { server, timeout_ms } => {
                self.stop(&server, Duration::from_millis(timeout_ms)).await
            }
            Command::Console { server, command } => {
                if command.trim().is_empty()
                    || command.len() > 4096
                    || command.chars().any(char::is_control)
                {
                    return Err(invalid(
                        "console command must contain 1..=4096 bytes without control characters",
                    ));
                }
                let mut processes = self.processes.lock().await;
                let process = processes.get_mut(&server).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotConnected, "managed server is not running")
                })?;
                crate::managed::write_console(&mut process.child, &command).await?;
                Ok(json!({"sent": true}))
            }
            Command::Logs {
                server,
                directory,
                cursor,
            } => {
                check_name(&server)?;
                let path = self.directory(&directory)?.join(crate::managed::LOG_FILE);
                tokio::task::spawn_blocking(move || crate::managed::read_log(&path, cursor))
                    .await
                    .map_err(io::Error::other)?
            }
            Command::Prepare(prepare) => {
                let _storage = self.storage.lock().await;
                self.prepare(&prepare).await
            }
            Command::Remove {
                server,
                directory,
                storage,
            } => {
                check_name(&server)?;
                let _storage = self.storage.lock().await;
                if self.processes.lock().await.contains_key(&server) {
                    return Err(io::Error::new(
                        io::ErrorKind::ResourceBusy,
                        "the server is still running",
                    ));
                }
                let directory = self.directory(&directory)?;
                self.prepared
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&server);
                let removed = tokio::task::spawn_blocking(move || {
                    provisioning::remove_on_node(&server, &directory, storage)
                })
                .await
                .map_err(io::Error::other)??;
                Ok(json!({"removed": removed}))
            }
            Command::Files(files) => {
                check_name(&files.server)?;
                let base = self.directory(&files.directory)?;
                tokio::task::spawn_blocking(move || serve_files(&base, &files))
                    .await
                    .map_err(io::Error::other)?
            }
            Command::Reconcile { servers } => self.reconcile(servers).await,
        }
    }

    /// `relative` below the root, refusing symlinks and `..`.
    fn directory(&self, relative: &Path) -> io::Result<PathBuf> {
        if !valid_node_directory(relative) {
            return Err(invalid(
                "server directories must be relative to the node root, without . or ..",
            ));
        }
        let path = provisioning::checked_path(&self.root.join(relative))?;
        if !path.starts_with(&self.root) || path == self.root {
            return Err(invalid("server directory is outside the node root"));
        }
        Ok(path)
    }

    async fn start(&self, start: protocol::Start) -> io::Result<Value> {
        check_name(&start.server)?;
        let program = start
            .command
            .first()
            .ok_or_else(|| invalid("managed server command is empty"))?;
        if !self.options.allowed_programs.contains(program) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "program {program:?} is not allowed on node {}; start the agent with --allow-program {program}",
                    self.options.name
                ),
            ));
        }
        if start.port == 0 {
            return Err(invalid("server port must be nonzero"));
        }
        let directory = self.directory(&start.directory)?;
        let mut processes = self.processes.lock().await;
        if let Some(process) = processes.get(&start.server) {
            // Already running, e.g. after the controller restarted: adopt it.
            return Ok(json!({
                "pid": process.pid,
                "adopted": true,
                "agent": self.id,
                "revision": self.revision.load(Ordering::Acquire),
            }));
        }
        if !directory.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "server directory {} does not exist on this node",
                    directory.display()
                ),
            ));
        }
        match std::net::TcpListener::bind(("0.0.0.0", start.port)) {
            Ok(listener) => drop(listener),
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!(
                        "port {} is in use on node {}; Rift will not adopt an unowned process",
                        start.port, self.options.name
                    ),
                ));
            }
            Err(error) => return Err(error),
        }
        let child = crate::managed::spawn_process(&start.command, &directory)?;
        let pid = child.id();
        processes.insert(
            start.server.clone(),
            Process {
                child: Some(child),
                pid,
            },
        );
        self.exits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|exit| exit.name != start.server);
        drop(processes);
        let revision = self.bump();
        eprintln!("rift: node started {} (pid {pid:?})", start.server);
        Ok(json!({"pid": pid, "adopted": false, "agent": self.id, "revision": revision}))
    }

    async fn stop(&self, server: &str, grace: Duration) -> io::Result<Value> {
        check_name(server)?;
        let process = self.processes.lock().await.remove(server);
        let result = match process.and_then(|process| process.child) {
            Some(child) => crate::managed::stop_child(child, grace).await,
            None => Ok(()),
        };
        let revision = self.bump();
        result.map(|()| json!({"stopped": true, "agent": self.id, "revision": revision}))
    }

    async fn reconcile(&self, servers: Vec<String>) -> io::Result<Value> {
        let keep: BTreeSet<_> = servers.into_iter().collect();
        let _storage = self.storage.lock().await;
        let recent: BTreeSet<String> = {
            let mut prepared = self.prepared.lock().unwrap_or_else(|e| e.into_inner());
            prepared.retain(|_, at| at.elapsed() < PREPARE_GRACE);
            prepared.keys().cloned().collect()
        };
        let stale: Vec<_> = {
            let mut processes = self.processes.lock().await;
            let names: Vec<_> = processes
                .keys()
                .filter(|name| !keep.contains(*name) && !recent.contains(*name))
                .cloned()
                .collect();
            names
                .into_iter()
                .filter_map(|name| processes.remove(&name).map(|process| (name, process)))
                .collect()
        };
        let stopped: Vec<_> = stale.iter().map(|(name, _)| name.clone()).collect();
        if !stale.is_empty() {
            self.bump();
            for (name, process) in stale {
                eprintln!("rift: node stopping {name}: the controller no longer assigns it here");
                if let Some(child) = process.child {
                    let _ = crate::managed::stop_child(child, SERVER_STOP_GRACE).await;
                }
            }
        }
        let running: BTreeSet<_> = self.processes.lock().await.keys().cloned().collect();
        let services = self.root.join("temp/services");
        let removed = tokio::task::spawn_blocking(move || -> io::Result<Vec<String>> {
            let mut removed = Vec::new();
            let entries = match std::fs::read_dir(&services) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(removed),
                Err(error) => return Err(error),
            };
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let path = entry.path();
                if !std::fs::symlink_metadata(&path)?.is_dir() {
                    continue;
                }
                if name.starts_with(".rift-stage-") {
                    // Serialized with prepare, so no copy is in progress.
                    std::fs::remove_dir_all(&path)?;
                    continue;
                }
                let Some(owner) = provisioning::disposable_owner(&path) else {
                    continue;
                };
                if owner == name
                    && !keep.contains(&owner)
                    && !recent.contains(&owner)
                    && !running.contains(&owner)
                    && provisioning::remove_on_node(&owner, &path, InstanceStorage::Disposable)?
                {
                    removed.push(owner);
                }
            }
            Ok(removed)
        })
        .await
        .map_err(io::Error::other)??;
        for name in &removed {
            eprintln!("rift: node removed unassigned disposable instance {name}");
        }
        Ok(json!({"stopped": stopped, "removed": removed}))
    }

    async fn prepare(&self, prepare: &protocol::Prepare) -> io::Result<Value> {
        check_name(&prepare.server)?;
        let directory = self.directory(&prepare.directory)?;
        let needs_assets = prepare.template.is_some() && !directory.exists();
        let assets = if needs_assets {
            Some(self.fetch_assets(prepare).await?)
        } else {
            None
        };
        let request = prepare.clone();
        let result = tokio::task::spawn_blocking(move || {
            provisioning::prepare_on_node(&request, &directory, assets.as_ref())
        })
        .await
        .map_err(io::Error::other)??;
        self.prepared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(prepare.server.clone(), std::time::Instant::now());
        Ok(result)
    }

    async fn fetch_assets(&self, prepare: &protocol::Prepare) -> io::Result<provisioning::Assets> {
        let jar = match (&prepare.software, &prepare.server_jar) {
            (Some(software), _) => {
                let software = crate::software::Software::try_from(software)?;
                crate::software::ensure(&self.root.join("cache/versioncache"), &software).await?
            }
            (None, Some(jar)) => self.fetch(jar).await?,
            (None, None) => return Err(invalid("template has neither software nor server_jar")),
        };
        let mut configs = Vec::new();
        for layer in &prepare.configs {
            configs.push(self.fetch(layer).await?);
        }
        let mut plugins = Vec::new();
        for plugin in &prepare.plugins {
            plugins.push(self.fetch(plugin).await?);
        }
        let map = match &prepare.map {
            Some(map) => Some(self.fetch(map).await?),
            None => None,
        };
        let properties = prepare
            .properties
            .iter()
            .map(|(name, value, replace)| {
                if *replace {
                    provisioning::Property::Replace(name.clone().into(), value.clone().into())
                } else {
                    provisioning::Property::Default(name.clone().into(), value.clone().into())
                }
            })
            .collect();
        Ok(provisioning::Assets {
            jar,
            configs,
            plugins,
            map,
            accept_eula: prepare.accept_eula,
            forwarding: prepare.forwarding.clone(),
            properties,
        })
    }

    fn template_url(&self, relative: &str) -> io::Result<reqwest::Url> {
        let web = self.options.web.as_deref().ok_or_else(|| {
            invalid("templates on nodes need the controller's web API; start the agent with --web")
        })?;
        let mut url = reqwest::Url::parse(web).map_err(|error| invalid(error.to_string()))?;
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| invalid("--web cannot be a base URL"))?;
            path.pop_if_empty().extend(["api", "files", "templates"]);
            for segment in crate::files::segments(relative)? {
                path.push(&segment);
            }
        }
        Ok(url)
    }

    async fn get(&self, url: reqwest::Url) -> io::Result<reqwest::Response> {
        let response = self
            .http
            .get(url.clone())
            .bearer_auth(&self.options.token)
            .send()
            .await
            .map_err(|error| io::Error::other(format!("fetching {url}: {error}")))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(io::Error::other(format!(
                "fetching {url}: HTTP {status}: {}",
                body.chars().take(512).collect::<String>()
            )));
        }
        Ok(response)
    }

    /// Mirror one template file or folder into the local cache, downloading
    /// only files whose SHA-256 differs, and return its cached path.
    async fn fetch(&self, relative: &str) -> io::Result<PathBuf> {
        let mut manifest_url = self.template_url(relative)?;
        manifest_url.set_query(Some("manifest"));
        let body = self
            .get(manifest_url)
            .await?
            .bytes()
            .await
            .map_err(io::Error::other)?;
        let manifest: Value = serde_json::from_slice(&body)
            .map_err(|error| io::Error::other(format!("invalid template manifest: {error}")))?;
        let cache = self.root.join("cache/templates");
        let mut target = cache.clone();
        for segment in crate::files::segments(relative)? {
            target.push(segment);
        }
        let files = manifest["files"]
            .as_array()
            .ok_or_else(|| io::Error::other("invalid template manifest"))?;
        let single = files.len() == 1 && files[0]["path"] == "";
        let mut wanted = BTreeSet::new();
        for file in files {
            let path = file["path"]
                .as_str()
                .ok_or_else(|| io::Error::other("invalid template manifest"))?;
            let sha256 = file["sha256"]
                .as_str()
                .filter(|hash| hash.len() == 64)
                .ok_or_else(|| io::Error::other("invalid template manifest"))?;
            let segments = crate::files::segments(path)?;
            let mut local = target.clone();
            for segment in &segments {
                local.push(segment);
            }
            if segments.is_empty() && !single {
                return Err(io::Error::other("invalid template manifest"));
            }
            wanted.insert(local.clone());
            if self.cached_hash(&local).as_deref() == Some(sha256) {
                continue;
            }
            let mut url = self.template_url(relative)?;
            if !segments.is_empty() {
                url.path_segments_mut()
                    .map_err(|_| invalid("--web cannot be a base URL"))?
                    .extend(&segments);
            }
            let bytes = self
                .get(url)
                .await?
                .bytes()
                .await
                .map_err(io::Error::other)?;
            let digest: String = Sha256::digest(&bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            if digest != sha256 {
                return Err(io::Error::other(format!(
                    "template file {relative}/{path} changed during download; retry"
                )));
            }
            write_cached(&cache, &local, &bytes)?;
            self.cached_hash(&local);
        }
        if !single {
            std::fs::create_dir_all(&target)?;
            for directory in manifest["directories"].as_array().into_iter().flatten() {
                if let Some(directory) = directory.as_str() {
                    let mut local = target.clone();
                    for segment in crate::files::segments(directory)? {
                        local.push(segment);
                    }
                    std::fs::create_dir_all(local)?;
                }
            }
            // Files deleted from the template disappear from the cache as well.
            prune(&target, &wanted)?;
        }
        provisioning::checked_path(&target)
    }

    fn cached_hash(&self, path: &Path) -> Option<String> {
        let metadata = std::fs::symlink_metadata(path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        let modified = metadata.modified().ok()?;
        let mut hashes = self.hashes.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((size, time, hash)) = hashes.get(path)
            && *size == metadata.len()
            && *time == modified
        {
            return Some(hash.clone());
        }
        let hash = crate::files::file_hash(path).ok()?;
        hashes.insert(path.to_owned(), (metadata.len(), modified, hash.clone()));
        Some(hash)
    }
}

/// Atomically replace one cached template file below `cache`.
fn write_cached(cache: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("cache file has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let checked = provisioning::checked_path(path)?;
    if !checked.starts_with(provisioning::checked_path(cache)?) {
        return Err(invalid("template cache path escapes the cache"));
    }
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
        std::fs::remove_dir_all(path)?;
    }
    let staged = parent.join(format!(
        ".rift-download-{}",
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let result = (|| {
        std::fs::write(&staged, bytes)?;
        std::fs::rename(&staged, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

/// Delete cached files below `directory` that are not in `wanted`.
fn prune(directory: &Path, wanted: &BTreeSet<PathBuf>) -> io::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            prune(&path, wanted)?;
        } else if !wanted.contains(&path) {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

fn serve_files(base: &Path, request: &protocol::Files) -> io::Result<Value> {
    use crate::files;
    let segments = files::segments(&request.path)?;
    let target = files::resolve(base, &segments)?;
    match request.method.as_str() {
        "GET" | "HEAD" => {
            let metadata = std::fs::symlink_metadata(&target)?;
            if metadata.is_dir() {
                return files::list(&target, &segments);
            }
            if !metadata.is_file() {
                return Err(invalid("only regular files and folders are served"));
            }
            if metadata.len() > protocol::MAX_REMOTE_FILE as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    "files on nodes are limited to 512 KiB through the web console",
                ));
            }
            Ok(json!({"content": protocol::encode_base64(&std::fs::read(&target)?)}))
        }
        "PUT" => {
            files::protect(&segments)?;
            if segments.is_empty() {
                return Err(invalid("choose a path inside the folder"));
            }
            if !base.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "the server directory does not exist yet",
                ));
            }
            let content = match &request.content {
                Some(content) => protocol::decode_base64(content)
                    .ok_or_else(|| invalid("file contents must be base64"))?,
                None => Vec::new(),
            };
            if content.len() > protocol::MAX_REMOTE_FILE {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    "files on nodes are limited to 512 KiB through the web console",
                ));
            }
            files::write(base, &segments, &target, request.folder, &content)
        }
        "DELETE" => files::delete(&target, &segments),
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "use GET, PUT or DELETE",
        )),
    }
}

fn check_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(invalid("invalid server name"));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

/// Await futures concurrently without an extra dependency.
async fn futures_join<F>(futures: impl IntoIterator<Item = F>) -> Vec<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handles: Vec<_> = futures.into_iter().map(tokio::spawn).collect();
    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        if let Ok(result) = handle.await {
            results.push(result);
        }
    }
    results
}
