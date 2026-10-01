//! Bounded, authenticated local control plane. One JSON request/response per TCP connection.
use crate::{admission::Admission, events, metrics::Metrics, runtime::Snapshot};
use rift::config::{Admin, Config};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, watch},
    task::JoinSet,
    time::timeout,
};

pub type Reply = oneshot::Sender<Result<Value, String>>;
pub enum Command {
    Reload(Reply),
    CreateInstance { group: String, reply: Reply },
    RemoveInstance { name: String, reply: Reply },
    // The handler confirms that the response has been written before shutdown begins.
    Shutdown(oneshot::Receiver<()>),
}

pub struct Transfer {
    pub snapshot: Arc<Snapshot>,
    pub backend: String,
    pub reply: TransferReply,
}

/// Backend plugin transfers have no wire acknowledgement. They still share the
/// bounded per-player queue and the same completion and admission path as admin.
pub enum TransferReply {
    Admin(Reply),
    BungeeCord,
}

impl TransferReply {
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Admin(reply) if reply.is_closed())
    }

    pub fn reason(&self) -> &'static str {
        match self {
            Self::Admin(_) => "admin",
            Self::BungeeCord => "bungeecord",
        }
    }

    pub fn send(self, value: Result<Value, String>) -> Result<(), Result<Value, String>> {
        match self {
            Self::Admin(reply) => reply.send(value),
            Self::BungeeCord => Ok(()),
        }
    }
}

struct Entry {
    name: String,
    backend: String,
    protocol: i32,
    peer: SocketAddr,
    transfer: mpsc::Sender<Transfer>,
}

#[derive(Default)]
pub struct Control {
    maintenance: Mutex<Option<bool>>,
    draining: Mutex<BTreeMap<String, bool>>,
    pub logins: Mutex<Admission>,
    players: Mutex<BTreeMap<u64, Entry>>,
}

impl Control {
    pub fn maintenance(&self, config: &Config) -> bool {
        self.maintenance
            .lock()
            .unwrap()
            .unwrap_or(config.maintenance)
    }

    pub fn draining(&self, config: &Config, backend: &str) -> bool {
        self.draining
            .lock()
            .unwrap()
            .get(backend)
            .copied()
            .unwrap_or_else(|| config.draining.contains(backend))
    }

    pub fn register(
        self: &Arc<Self>,
        id: u64,
        backend: String,
        protocol: i32,
        name: String,
        peer: SocketAddr,
    ) -> (Registration, mpsc::Receiver<Transfer>) {
        let (transfer, receiver) = mpsc::channel(1);
        self.players.lock().unwrap().insert(
            id,
            Entry {
                name,
                backend,
                protocol,
                peer,
                transfer,
            },
        );
        (
            Registration {
                control: self.clone(),
                id,
            },
            receiver,
        )
    }

    pub fn moved(&self, id: u64, backend: &str) {
        if let Some(entry) = self.players.lock().unwrap().get_mut(&id) {
            entry.backend = backend.to_owned();
        }
    }

    pub fn player_peer(&self, name: &str) -> Option<SocketAddr> {
        self.players
            .lock()
            .unwrap()
            .values()
            .find(|entry| entry.name.eq_ignore_ascii_case(name))
            .map(|entry| entry.peer)
    }

    pub fn plugin_transfer(&self, name: &str, backend: &str, snapshot: Arc<Snapshot>) {
        let players = self.players.lock().unwrap();
        if let Some(entry) = players
            .values()
            .find(|entry| entry.name.eq_ignore_ascii_case(name))
            && entry.backend != backend
        {
            // No task or unbounded queue per request, including self-transfers.
            // The recipient rechecks access, health, draining and extensions.
            let _ = entry.transfer.try_send(Transfer {
                snapshot,
                backend: backend.to_owned(),
                reply: TransferReply::BungeeCord,
            });
        }
    }

    fn status(&self, snapshot: &Snapshot, metrics: &Metrics) -> Value {
        let players: Vec<Value> = self.players.lock().unwrap().iter().map(|(id, entry)| json!({"connection_id":id,"name":entry.name,"backend":entry.backend,"protocol":entry.protocol})).collect();
        let backends: Vec<Value> = snapshot.health.metrics().into_iter().map(|(name, up)| {
            json!({"draining":self.draining(&snapshot.config, &name),"backend":name,"up":up})
        }).collect();
        json!({"maintenance":self.maintenance(&snapshot.config),"connections":metrics.active.get(),"players_online":metrics.players.get(),"players":players,"backends":backends,"managed_servers":managed_servers(snapshot)})
    }
}

pub struct Registration {
    control: Arc<Control>,
    id: u64,
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.control.players.lock().unwrap().remove(&self.id);
    }
}

pub fn token(settings: &Admin) -> io::Result<String> {
    let token = std::env::var(&settings.token_env).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("admin.token_env: set {} to a random secret of at least 32 bytes before starting Rift", settings.token_env)))?;
    if token.len() < 32 || token.len() > 1024 || token.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "admin.token_env: secret must contain 32..=1024 bytes without control characters",
        ));
    }
    Ok(token)
}

fn authenticated(expected: &str, supplied: &str) -> bool {
    let mut difference = expected.len() ^ supplied.len();
    for (index, byte) in expected.bytes().enumerate() {
        difference |= usize::from(byte ^ supplied.as_bytes().get(index).copied().unwrap_or(0));
    }
    difference == 0
}

async fn read_line(stream: &mut TcpStream, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let byte = stream.read_u8().await?;
        if byte == b'\n' {
            return Ok(bytes);
        }
        if bytes.len() >= limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "admin request/response exceeds size limit",
            ));
        }
        bytes.push(byte);
    }
}

fn request(value: &Value, settings: &Admin, secret: &str) -> Result<Vec<String>, String> {
    if !value
        .get("token")
        .and_then(Value::as_str)
        .is_some_and(|s| authenticated(secret, s))
    {
        return Err("authentication failed; supply RIFT_ADMIN_TOKEN".into());
    }
    let args = value
        .get("args")
        .and_then(Value::as_array)
        .ok_or("expected args array")?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or("command arguments must be strings".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let command = args.first().ok_or("missing command")?;
    let permission = match command.as_str() {
        "start" | "stop" | "groups" | "create" | "remove" => "servers",
        command => command,
    };
    if !settings.permissions.contains(permission) {
        return Err(format!(
            "permission denied: {permission}; grant it in admin.permissions and restart"
        ));
    }
    Ok(args)
}

/// Operational state only: never include process arguments, environment or paths.
pub(crate) fn managed_servers(snapshot: &Snapshot) -> Value {
    let config = &snapshot.config;
    let servers: Vec<_> = snapshot
        .managed
        .snapshot()
        .into_iter()
        .map(|server| {
            let instance = config.instances.get(&server.name);
            let address = config
                .backends
                .get(&server.name)
                .map(|backend| backend.address());
            json!({
                "name": server.name,
                "state": server.state.as_str(),
                "pid": server.pid,
                "players": server.players,
                "reservations": server.reservations,
                "automatic_start": server.automatic_start,
                "automatic_enabled": server.automatic_enabled,
                "last_error": server.last_error,
                "restart_attempts": server.restart_attempts,
                "restart_exhausted": server.restart_exhausted,
                "group": instance.map(|instance| &instance.group),
                "template": instance.and_then(|instance| instance.template.as_ref()),
                "storage": instance.map_or("persistent", |instance| instance.storage.as_str()),
                "address": address,
                "port": address
                    .and_then(|address| address.parse::<SocketAddr>().ok())
                    .map(|address| address.port()),
            })
        })
        .collect();
    json!(servers)
}

/// Group metadata only: templates contain executable arguments and filesystem paths.
pub(crate) fn service_groups(snapshot: &Snapshot) -> Value {
    json!({"groups":snapshot.config.service_groups.iter().map(|(name, group)| {
        json!({
            "name":name,
            "template":group.template,
            "storage":group.storage.as_str(),
            "port_range":[group.port_start,group.port_end],
            "scaling":group.scaling.as_ref().map(scaling_values),
            "instances":snapshot.config.instances.iter()
                .filter(|(_, instance)| instance.group == *name)
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
        })
    }).collect::<Vec<_>>()})
}

/// The same field names as `service_groups.*.scaling` in Lua.
pub(crate) fn scaling_values(policy: &rift::config::ServiceScaling) -> Value {
    json!({
        "min_instances": policy.min_instances,
        "max_instances": policy.max_instances,
        "spare_instances": policy.spare_instances,
        "capacity_per_instance": policy.capacity_per_instance,
        "target_occupancy_percent": policy.target_occupancy_percent,
        "queue_threshold": policy.queue_threshold,
        "cooldown_ms": policy.cooldown.as_millis(),
    })
}

pub(crate) fn managed_backend(snapshot: &Snapshot, backend: &str) -> io::Result<()> {
    if !snapshot.config.backends.contains_key(backend) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("unknown backend {backend:?}"),
        ));
    }
    if !snapshot.managed.is_managed(backend) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("backend {backend:?} is not managed; configure managed_servers and restart"),
        ));
    }
    Ok(())
}

pub(crate) async fn execute(
    args: &[String],
    snapshot: Arc<Snapshot>,
    metrics: &Metrics,
    commands: &mpsc::Sender<Command>,
) -> Result<Value, String> {
    let control = &snapshot.control;
    let enabled = |value: &str| match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err("expected on or off".to_owned()),
    };
    match args {
        [command] if command == "status" => Ok(control.status(&snapshot, metrics)),
        [command] if command == "servers" => Ok(json!({"servers":managed_servers(&snapshot)})),
        [command] if command == "groups" => Ok(service_groups(&snapshot)),
        [command, group] if command == "create" => {
            let (reply, receiver) = oneshot::channel();
            commands.try_send(Command::CreateInstance { group: group.clone(), reply })
                .map_err(|_| "administrator command queue is full or shutting down")?;
            receiver.await.map_err(|_| "proxy is shutting down".to_owned())?
        }
        [command, name] if command == "remove" => {
            let (reply, receiver) = oneshot::channel();
            commands.try_send(Command::RemoveInstance { name: name.clone(), reply })
                .map_err(|_| "administrator command queue is full or shutting down")?;
            receiver.await.map_err(|_| "proxy is shutting down".to_owned())?
        }
        [command, backend] if command == "start" || command == "stop" => {
            managed_backend(&snapshot, backend).map_err(|error| error.to_string())?;
            if command == "start" {
                snapshot.managed.request_start(backend)
            } else {
                snapshot.managed.request_stop(backend)
            }.map_err(|error| error.to_string())?;
            Ok(json!({"backend":backend,"operation":command,"accepted":true,"message":"Request accepted; use servers to check completion"}))
        }
        [command, value] if command == "maintenance" => {
            let value = enabled(value)?;
            *control.maintenance.lock().unwrap() = Some(value);
            Ok(json!({"maintenance":value}))
        }
        [command, backend, value] if command == "drain" => {
            if !snapshot.config.backends.contains_key(backend) { return Err(format!("unknown backend {backend:?}; use status to list backends")); }
            let value = enabled(value)?;
            control.draining.lock().unwrap().insert(backend.clone(), value);
            Ok(json!({"backend":backend,"draining":value}))
        }
        [command, id, backend] if command == "transfer" => {
            let id: u64 = id.parse().map_err(|_| "connection ID must be a positive integer")?;
            if !snapshot.config.backends.contains_key(backend) { return Err(format!("unknown backend {backend:?}")); }
            if control.draining(&snapshot.config, backend) || !snapshot.reachable(backend) { return Err("target backend is draining, stopped or unhealthy".into()); }
            let sender = {
                let players = control.players.lock().unwrap();
                let entry = players.get(&id).ok_or("player is no longer online; refresh status")?;
                if !rift::protocol::ProtocolVersion::new(entry.protocol).is_ok_and(rift::protocol::ProtocolVersion::supports_switching) { return Err("player transfers require Minecraft 1.8.9–26.3".into()); }
                if entry.backend == *backend { return Err("player is already on that backend".into()); }
                entry.transfer.clone()
            };
            let (reply, receiver) = oneshot::channel();
            sender.try_send(Transfer {snapshot: snapshot.clone(), backend: backend.clone(), reply: TransferReply::Admin(reply)}).map_err(|_| "player disconnected or already has a queued transfer")?;
            receiver.await.map_err(|_| "player disconnected during transfer".to_owned())?
        }
        [command] if command == "reload" => {
            let (reply, receiver) = oneshot::channel();
            commands.try_send(Command::Reload(reply)).map_err(|_| "administrator command queue is full or shutting down")?;
            receiver.await.map_err(|_| "proxy is shutting down".to_owned())?
        }
        _ => Err("usage: status | servers | groups | create <group> | remove <instance> | start <backend> | stop <backend> | maintenance on/off | drain <backend> on/off | transfer <connection-id> <backend> | reload | shutdown".into()),
    }
}

pub async fn serve(
    listener: TcpListener,
    settings: Admin,
    secret: String,
    current: watch::Receiver<Arc<Snapshot>>,
    metrics: Arc<Metrics>,
    commands: mpsc::Sender<Command>,
    workflows: Arc<crate::operator::Store>,
) {
    let settings = Arc::new(settings);
    let secret = Arc::new(secret);
    let mut handlers = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = handlers.join_next(), if !handlers.is_empty() => {},
            result = listener.accept() => {
                let (mut stream, _) = match result { Ok(result) => result, Err(_) => {tokio::time::sleep(Duration::from_millis(100)).await; continue;} };
                if handlers.len() >= 16 { continue; }
                let settings = settings.clone();
                let secret = secret.clone();
                let current = current.clone();
                let metrics = metrics.clone();
                let commands = commands.clone();
                let workflows = workflows.clone();
                handlers.spawn(async move {
                    let _ = async {
                        let bytes = timeout(Duration::from_secs(2), read_line(&mut stream, 8192)).await??;
                        let parsed = serde_json::from_slice::<Value>(&bytes).map_err(|_| "invalid JSON request".to_owned()).and_then(|value| request(&value, &settings, &secret));
                        let mut permission = String::new();
                        let mut shutdown_ack = None;
                        let result = match parsed {
                            Ok(args) => {
                                permission = args[0].clone();
                                let audit = workflows.audit_async("local-admin", &permission, "", "requested", 0).await;
                                if audit.is_err() {
                                    Err("operator audit unavailable; command was not submitted".into())
                                } else if args == ["shutdown"] {
                                    let (ack, receiver) = oneshot::channel();
                                    match commands.try_send(Command::Shutdown(receiver)) {
                                        Ok(()) => {shutdown_ack = Some(ack); Ok(json!({"shutdown":"draining"}))}
                                        Err(_) => Err("proxy is already shutting down or busy".into()),
                                    }
                                } else {
                                    let snapshot = current.borrow().clone();
                                    let mut deadline = Duration::from_secs(40);
                                    if let [command, _, backend] = args.as_slice()
                                        && command == "transfer"
                                        && let Some(server) = snapshot.config.managed_servers.get(backend)
                                    {
                                        deadline += server.start_timeout;
                                    }
                                    if let [command, name] = args.as_slice()
                                        && command == "remove"
                                        && let Some(server) = snapshot.config.managed_servers.get(name)
                                    {
                                        deadline += server.stop_timeout;
                                    }
                                    timeout(deadline, execute(&args, snapshot, &metrics, &commands)).await
                                        .unwrap_or_else(|_| Err("administrator operation deadline exceeded; refresh status before retrying".into()))
                                }
                            }
                            Err(error) => Err(error),
                        };
                        let actor = if permission.is_empty() { "anonymous" } else { "local-admin" };
                        let (outcome, status) = if result.is_ok() { ("accepted", 200) } else { ("rejected", 400) };
                        if let Err(error) = workflows.audit_async(actor, &permission, "", outcome, status).await {
                            eprintln!("rift: local administrator audit failed: {error}");
                        }
                        let response = match result {
                            Ok(data) => { events::admin("admin_command", "ok", &permission, &"command completed"); json!({"ok":true,"data":data}) }
                            Err(error) => { events::admin("admin_command", "rejected", &permission, &error); json!({"ok":false,"error":error}) }
                        };
                        timeout(Duration::from_secs(2), stream.write_all(format!("{response}\n").as_bytes())).await??;
                        timeout(Duration::from_secs(2), stream.shutdown()).await??;
                        if let Some(ack) = shutdown_ack { let _ = ack.send(()); }
                        Ok::<_,io::Error>(())
                    }.await;
                });
            }
        }
    }
}

pub async fn client(args: &[String]) -> io::Result<()> {
    let (address, args) = match args {
        [flag, address, args @ ..] if flag == "--address" => (address.as_str(), args),
        _ => ("127.0.0.1:9091", args),
    };
    if args.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: rift admin [--address 127.0.0.1:9091] status|servers|groups|create <group>|remove <instance>|start <backend>|stop <backend>|maintenance on/off|drain <backend> on/off|transfer <connection-id> <backend>|reload|shutdown",
        ));
    }
    let address: SocketAddr = address.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "admin address requires a loopback IP and port",
        )
    })?;
    if !address.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "admin address must be loopback; run this command on the proxy host",
        ));
    }
    let secret = std::env::var("RIFT_ADMIN_TOKEN").map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "set RIFT_ADMIN_TOKEN to the server's admin secret",
        )
    })?;
    let deadline = if args
        .first()
        .is_some_and(|command| command == "transfer" || command == "remove")
    {
        Duration::from_secs(24 * 60 * 60 + 45)
    } else {
        Duration::from_secs(40)
    };
    timeout(deadline, async {
        let mut stream = timeout(Duration::from_secs(2), TcpStream::connect(address)).await??;
        timeout(
            Duration::from_secs(2),
            stream.write_all(format!("{}\n", json!({"token":secret,"args":args})).as_bytes()),
        )
        .await??;
        let response: Value =
            serde_json::from_slice(&read_line(&mut stream, 16 * 1024 * 1024).await?)
                .map_err(io::Error::other)?;
        if response["ok"] != true {
            return Err(io::Error::other(
                response["error"]
                    .as_str()
                    .unwrap_or("invalid admin response"),
            ));
        }
        println!("{}", serde_json::to_string_pretty(&response["data"])?);
        Ok(())
    })
    .await?
}

#[cfg(test)]
mod tests;
