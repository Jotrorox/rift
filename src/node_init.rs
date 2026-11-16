//! `rift node init <name>`: everything needed to add a node, printed on the
//! controller. It generates the node's token (or reuses the one in this
//! environment), prints the `rift.lua` entry, the agent's environment file
//! and systemd unit, and checks what is easiest to get wrong offline: whether
//! the messaging certificate names the address the agent dials, and whether
//! the web API is reachable over HTTPS. It writes nothing.
use rift::config::{Config, WebTls, default_token_env};
use rustls::{
    RootCertStore,
    client::{WebPkiServerVerifier, danger::ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime, pem::PemObject},
};
use std::{
    fmt::Write as _,
    io,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
};

pub const USAGE: &str = "Usage: rift node init <name> [options]\n\
    Run on the controller next to rift.lua. Prints the node's token, its rift.lua entry, the\n\
    agent's environment file and systemd unit (Delegate=memory, KillMode=control-group), and\n\
    checks the messaging certificate and the HTTPS web API. Writes nothing. A token already\n\
    in this environment (RIFT_NODE_<NAME>_TOKEN, or the node's token_env) is reused.\n\
    --config <path>          the controller's configuration (default ./rift.lua)\n\
    --address <host>         the node's IP address or DNS name; required unless rift.lua defines the node\n\
    --controller <host:port> where the node reaches the message bus (default messaging.advertise,\n\
                             else messaging.listen)\n\
    --web <url>              where the node reaches the web API (default: web.listen when web.tls is set)\n\
    --ca <file>              PEM CA certificate(s) the node trusts (default messaging.certificate,\n\
                             which works when it is self-signed)\n\
    --root <directory>       the agent's --root on the node (default /var/lib/rift-node)\n\
    --user <name>            the account the agent runs as (default rift)\n\
    --binary <path>          the rift binary on the node (default /usr/local/bin/rift)";

const DEFAULT_ROOT: &str = "/var/lib/rift-node";
/// Where the printed unit expects the CA and the environment file.
const CA_PATH: &str = "/etc/rift/rift-ca.pem";
const ENV_PATH: &str = "/etc/rift/node.env";
const UNIT_PATH: &str = "/etc/systemd/system/rift-node.service";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub name: String,
    pub config: PathBuf,
    pub address: Option<String>,
    pub controller: Option<String>,
    pub web: Option<String>,
    pub ca: Option<PathBuf>,
    pub root: String,
    pub user: String,
    pub binary: String,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

impl Options {
    pub fn parse(args: &[String]) -> io::Result<Self> {
        let mut name = None;
        let mut options = Self {
            name: String::new(),
            config: PathBuf::from(crate::DEFAULT_CONFIG_PATH),
            address: None,
            controller: None,
            web: None,
            ca: None,
            root: DEFAULT_ROOT.into(),
            user: "rift".into(),
            binary: "/usr/local/bin/rift".into(),
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            if !arg.starts_with("--") {
                if name.replace(arg.clone()).is_some() {
                    return Err(invalid(format!("only one node name is allowed\n{USAGE}")));
                }
                continue;
            }
            let value = args
                .next()
                .ok_or_else(|| invalid(format!("{arg} requires a value\n{USAGE}")))?
                .clone();
            match arg.as_str() {
                "--config" => options.config = value.into(),
                "--address" => options.address = Some(value),
                "--controller" => options.controller = Some(value),
                "--web" => options.web = Some(value.trim_end_matches('/').to_owned()),
                "--ca" => options.ca = Some(value.into()),
                "--root" => options.root = value,
                "--user" => options.user = value,
                "--binary" => options.binary = value,
                _ => return Err(invalid(format!("unknown option {arg}\n{USAGE}"))),
            }
        }
        options.name = name.ok_or_else(|| invalid(format!("a node name is required\n{USAGE}")))?;
        let name = &options.name;
        if name.is_empty()
            || name.len() > 64
            || name == rift::config::LOCAL_NODE
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(invalid(format!(
                "node names contain 1..=64 ASCII letters, digits, underscores or hyphens and are not {:?}",
                rift::config::LOCAL_NODE
            )));
        }
        // These end up unquoted in a systemd unit; keep them to plain paths.
        for (flag, value) in [
            ("--root", &options.root),
            ("--user", &options.user),
            ("--binary", &options.binary),
        ] {
            if value.is_empty()
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_graphic() && !b"\"'\\$%;`".contains(&b))
            {
                return Err(invalid(format!(
                    "{flag}: expected a value without spaces, quotes, '$', '%', ';' or '\\'"
                )));
            }
        }
        if !options.root.starts_with('/') || !options.binary.starts_with('/') {
            return Err(invalid("--root and --binary must be absolute paths"));
        }
        Ok(options)
    }
}

pub fn run(args: &[String]) -> io::Result<()> {
    let options = Options::parse(args)?;
    let (config, _) = Config::check(&options.config)?;
    let token = |variable: &str| std::env::var(variable).ok();
    let plan = plan(&config, &options, &token, &random_token)?;
    print!("{}", plan.text);
    if plan.problems > 0 {
        eprintln!(
            "rift node init: {} problem(s) listed under Checks; the node will not work until they are fixed",
            plan.problems
        );
    }
    Ok(())
}

fn random_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    aws_lc_rs::rand::fill(&mut bytes).map_err(|_| io::Error::other("no entropy for a token"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub struct Plan {
    pub text: String,
    pub problems: usize,
}

/// One line of the Checks section.
enum Check {
    Ok(String),
    Problem(String),
    Note(String),
}

/// `host` without IPv6 brackets.
fn split_host_port(address: &str) -> Option<(String, u16)> {
    let (host, port) = address.rsplit_once(':')?;
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    Some((host.to_owned(), port.parse().ok()?))
}

/// `host:port`, bracketing IPv6 literals.
fn join_host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

pub fn plan(
    config: &Config,
    options: &Options,
    environment: &dyn Fn(&str) -> Option<String>,
    generate: &dyn Fn() -> io::Result<String>,
) -> io::Result<Plan> {
    let name = options.name.as_str();
    let configured = config.nodes.get(name);
    let address = match (configured, &options.address) {
        (Some(node), Some(address)) if !node.address.eq_ignore_ascii_case(address) => {
            return Err(invalid(format!(
                "rift.config.nodes.{name}.address is {:?}, not {address:?}; drop --address or change rift.lua",
                node.address
            )));
        }
        (Some(node), _) => node.address.clone(),
        (None, Some(address)) => address.clone(),
        (None, None) => {
            return Err(invalid(format!(
                "rift.lua does not define nodes.{name}; pass --address with the node's IP address or DNS name"
            )));
        }
    };
    let messaging = config
        .messaging
        .as_ref()
        .filter(|messaging| messaging.listen.is_some())
        .ok_or_else(|| {
            invalid(
                "nodes connect to the message bus, which is not configured; add to rift.lua first:\n\
                rift.config.messaging = {\n    listen = \"0.0.0.0:7443\",\n    certificate = \"certs/messaging.pem\",\n    private_key = \"certs/messaging-key.pem\",\n}\n\
                The certificate must name the address nodes dial (see docs/nodes.md).",
            )
        })?;
    let listen = messaging.listen.expect("filtered above");
    let controller = match (&options.controller, &messaging.advertise) {
        (Some(controller), _) => controller.clone(),
        (None, Some(advertise)) => advertise.clone(),
        (None, None) if !listen.ip().is_unspecified() => listen.to_string(),
        (None, None) => {
            return Err(invalid(format!(
                "messaging.listen is {listen}, which is not an address a node can dial; pass --controller <host:port> (or set messaging.advertise)"
            )));
        }
    };
    let (controller_host, _) =
        split_host_port(&controller).ok_or_else(|| invalid("--controller must be host:port"))?;
    // The agent verifies the certificate for --server-name, which defaults to
    // the host it dials. advertise_server_name is the same name for instances.
    let server_name = messaging
        .advertise_server_name
        .clone()
        .filter(|name| !name.eq_ignore_ascii_case(&controller_host));
    let certificate = messaging
        .certificate
        .as_deref()
        .expect("validation requires a certificate with messaging.listen");
    let ca = options.ca.as_deref().unwrap_or(certificate);

    let token_env = configured
        .map(|node| node.token_env.clone())
        .unwrap_or_else(|| default_token_env(name));
    let (token, reused) = match environment(&token_env)
        .filter(|token| (32..=1024).contains(&token.len()) && !token.chars().any(char::is_control))
    {
        Some(token) => (token, true),
        None => (generate()?, false),
    };

    let mut checks = Vec::new();
    let verify_name = server_name.as_deref().unwrap_or(&controller_host);
    checks.push(match verify(certificate, ca, verify_name) {
        Ok(()) => Check::Ok(format!(
            "the messaging certificate {} is valid for {verify_name} and trusted through {}",
            certificate.display(),
            ca.display()
        )),
        Err(error) => Check::Problem(format!(
            "the agent will reject the messaging certificate {} for {verify_name}: {error}{}",
            certificate.display(),
            if options.ca.is_none() {
                "; if a CA signed it, pass that CA with --ca"
            } else {
                ""
            }
        )),
    });

    let web = web_url(config, options, &controller_host, ca, &mut checks);

    let mut ports = Vec::new();
    if let Some(node) = configured {
        for group in config.service_groups.values() {
            if group
                .nodes
                .as_ref()
                .is_some_and(|selector| selector.matches(name, node))
            {
                ports.push(if group.port_start == group.port_end {
                    group.port_start.to_string()
                } else {
                    format!("{}-{}", group.port_start, group.port_end)
                });
            }
        }
        for (server, definition) in &config.managed_servers {
            if definition.node.as_deref() == Some(name)
                && !config.instances.contains_key(server)
                && let Some(port) = config
                    .backends
                    .get(server)
                    .and_then(|backend| split_host_port(backend.address()))
                    .map(|(_, port)| port)
            {
                ports.push(port.to_string());
            }
        }
    }
    // The port the node dials, which differs from web.listen behind a proxy.
    let web_port = web
        .as_deref()
        .and_then(|url| reqwest::Url::parse(url).ok())
        .and_then(|url| url.port_or_known_default());
    let mut network = format!("allow UDP {} ", listen.port());
    if let Some(port) = web_port {
        let _ = write!(network, "and TCP {port} ");
    }
    let _ = write!(network, "from {address} to this host");
    if ports.is_empty() {
        network.push_str(
            "; allow the game ports of groups that select this node from this host to it",
        );
    } else {
        let _ = write!(
            network,
            "; allow TCP {} from this host to {address}, and from nowhere else",
            ports.join(", ")
        );
    }
    checks.push(Check::Note(network));

    let mut text = String::new();
    let out = &mut text;
    let _ = writeln!(out, "# Node {name:?} at {address}\n");
    let _ = writeln!(out, "## 1. On the controller (this host)\n");
    match configured {
        Some(_) => {
            let _ = writeln!(
                out,
                "rift.lua already defines nodes.{name}; nothing to add.\n"
            );
        }
        None => {
            let _ = writeln!(
                out,
                "Add the node to rift.lua:\n\nrift.config.nodes[{name:?}] = {{\n    address = {address:?},\n    -- memory_mb = 16384,       -- memory placement may allocate here\n    -- max_instances = 128,\n    -- labels = {{ \"games\" }},   -- service_groups.*.nodes select by label\n}}\n"
            );
        }
    }
    let _ = writeln!(
        out,
        "{} Rift's environment (with examples/rift.service: /etc/rift/admin.env).\nRift reads its environment at startup: restart it if this variable is new to it;\notherwise a reload of rift.lua adds the node (docs/nodes.md#change-nodes-while-rift-runs):\n\n{token_env}={token}\n",
        if reused {
            "This token is already set in this shell. Keep it in"
        } else {
            "Put this new token in"
        }
    );
    let _ = writeln!(out, "## 2. On the node ({address})\n");
    let _ = writeln!(
        out,
        "Install the same rift version as the controller at {binary}, create the account and\ncopy the CA certificate the agent trusts for the message bus{}:\n\nuseradd --system --user-group --home-dir {root} --shell /usr/sbin/nologin {user}\ninstall -d -m 0755 /etc/rift\nscp {ca} root@{address}:{CA_PATH}    # run on the controller\n",
        if web.is_some() { " and web API" } else { "" },
        binary = options.binary,
        root = options.root,
        user = options.user,
        ca = ca.display(),
    );
    let _ = writeln!(
        out,
        "# {ENV_PATH} (owner root, mode 0600)\nRIFT_NODE_TOKEN={token}"
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "# {UNIT_PATH}\n{}",
        unit(options, &controller, server_name.as_deref(), web.as_deref())
    );
    let _ = writeln!(
        out,
        "Start it and follow its log until it reports the controller connection:\n\nsystemctl daemon-reload\nsystemctl enable --now rift-node\njournalctl -u rift-node -f\n"
    );
    let _ = writeln!(out, "## 3. Checks\n");
    let mut problems = 0;
    for check in &checks {
        let (label, message) = match check {
            Check::Ok(message) => ("ok", message),
            Check::Problem(message) => {
                problems += 1;
                ("PROBLEM", message)
            }
            Check::Note(message) => ("note", message),
        };
        let _ = writeln!(out, "{label:<8} {message}");
    }
    Ok(Plan { text, problems })
}

/// The `--web` URL for the unit, or `None` with a problem explaining why
/// templates cannot reach the node.
fn web_url(
    config: &Config,
    options: &Options,
    controller_host: &str,
    ca: &Path,
    checks: &mut Vec<Check>,
) -> Option<String> {
    if let Some(url) = &options.web {
        if url.starts_with("https://") {
            checks.push(Check::Note(format!(
                "the web API at {url} was given with --web and is not checked; its certificate must be public or signed by {}",
                ca.display()
            )));
        } else {
            checks.push(Check::Problem(format!(
                "--web {url}: the agent only accepts https:// (plain http:// only on loopback)"
            )));
        }
        return Some(url.clone());
    }
    let Some(web) = &config.web else {
        checks.push(Check::Problem(
            "the web API is disabled, so nodes cannot download templates; add rift.config.web = { listen = \"0.0.0.0:8443\", token_env = \"RIFT_WEB_TOKEN\", tls = true }".into(),
        ));
        return None;
    };
    if !web.api {
        checks.push(Check::Problem(
            "web.api is false, so nodes cannot download templates".into(),
        ));
        return None;
    }
    let listen = web.listen;
    if listen.ip().is_loopback() {
        checks.push(Check::Problem(format!(
            "web.listen is {listen}, which nodes cannot reach; listen on an address they reach and set web.tls = true"
        )));
        return None;
    }
    let host = reachable_host(listen, controller_host);
    let url = format!("https://{}", join_host_port(&host, listen.port()));
    let Some((certificate, _)) = config.web_tls_files() else {
        checks.push(Check::Problem(format!(
            "web.listen {listen} serves plain HTTP, and the agent sends its token only over https://; set web.tls = true to serve HTTPS with the messaging certificate (or put a TLS reverse proxy in front and pass --web https://...)"
        )));
        return None;
    };
    let source = match web.tls {
        Some(WebTls::Messaging) => "the messaging certificate",
        _ => "its own certificate",
    };
    checks.push(match verify(certificate, ca, &host) {
        Ok(()) => Check::Ok(format!(
            "the web API serves HTTPS on {listen} with {source}, valid for {host}"
        )),
        // A web.tls certificate from a public CA is fine: the agent trusts
        // public CAs for --web, but this offline check only knows --ca.
        Err(error) if !matches!(web.tls, Some(WebTls::Messaging)) => Check::Note(format!(
            "the web certificate {} is not trusted through {} for {host} ({error}); fine if a public CA issued it for that name",
            certificate.display(),
            ca.display()
        )),
        Err(error) => Check::Problem(format!(
            "the agent will reject the web API's certificate for {host}: {error}; add {host} to the messaging certificate, or pass --web with a name it has"
        )),
    });
    Some(url)
}

/// The host a node dials for `listen`: its own address, or the controller's
/// host when it listens on every interface.
fn reachable_host(listen: SocketAddr, controller_host: &str) -> String {
    if listen.ip().is_unspecified() {
        controller_host.to_owned()
    } else {
        listen.ip().to_string()
    }
}

/// Verify the chain in `certificate` for `name` against the roots in `ca`,
/// exactly as the agent's QUIC client does.
fn verify(certificate: &Path, ca: &Path, name: &str) -> Result<(), String> {
    let read = |path: &Path| -> Result<Vec<CertificateDer<'static>>, String> {
        let certs = CertificateDer::pem_file_iter(path)
            .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if certs.is_empty() {
            return Err(format!("{}: no certificates found", path.display()));
        }
        Ok(certs)
    };
    let chain = read(certificate)?;
    let mut roots = RootCertStore::empty();
    for root in read(ca)? {
        roots
            .add(root)
            .map_err(|error| format!("{}: {error}", ca.display()))?;
    }
    let verifier = WebPkiServerVerifier::builder_with_provider(
        Arc::new(roots),
        Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
    )
    .build()
    .map_err(|error| error.to_string())?;
    let server_name = match name.parse::<IpAddr>() {
        Ok(ip) => ServerName::IpAddress(ip.into()),
        Err(_) => ServerName::try_from(name.to_owned())
            .map_err(|_| format!("{name:?} is not a valid DNS name"))?,
    };
    verifier
        .verify_server_cert(&chain[0], &chain[1..], &server_name, &[], UnixTime::now())
        .map(drop)
        .map_err(|error| error.to_string())
}

/// The agent's unit. KillMode=control-group makes every server die with the
/// agent; Delegate=memory gives it the cgroup memory limits need.
fn unit(
    options: &Options,
    controller: &str,
    server_name: Option<&str>,
    web: Option<&str>,
) -> String {
    let mut exec = format!(
        "{} node --controller {controller} --name {} --root {} --ca {CA_PATH}",
        options.binary, options.name, options.root
    );
    if let Some(server_name) = server_name {
        let _ = write!(exec, " --server-name {server_name}");
    }
    if let Some(web) = web {
        let _ = write!(exec, " --web {web}");
    }
    exec.push_str(" --allow-program java");
    // StateDirectory creates the default root owned by the service account;
    // any other root must exist and be writable by it.
    let directory = match options.root.strip_prefix("/var/lib/") {
        Some(state) if !state.is_empty() => {
            format!("StateDirectory={state}\nStateDirectoryMode=0750")
        }
        _ => format!(
            "# Create {root} owned by {user} first: install -d -o {user} -g {user} -m 0750 {root}\nReadWritePaths={root}",
            root = options.root,
            user = options.user
        ),
    };
    format!(
        "[Unit]
Description=Rift node agent ({name})
Wants=network-online.target
After=network-online.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Type=exec
User={user}
Group={user}
{directory}
WorkingDirectory={root}
EnvironmentFile={ENV_PATH}
ExecStart={exec}
Restart=on-failure
RestartSec=5s
# Required: stopping or killing the agent also kills every server it started,
# so none outlives it holding its port (docs/nodes.md#failure-behavior).
KillMode=control-group
# Must exceed the agent's 30 second server stop grace.
TimeoutStopSec=45s
# Required for memory_limit: the agent puts limited servers into cgroups below
# this unit. DelegateSubgroup needs systemd 254; older versions ignore it.
Delegate=memory
DelegateSubgroup=rift
LimitNOFILE=65536
UMask=0027
NoNewPrivileges=true
CapabilityBoundingSet=
AmbientCapabilities=
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ProtectProc=invisible
ProtectKernelModules=true
ProtectKernelLogs=true
ProtectClock=true
ProtectHostname=true
RestrictNamespaces=true
RestrictRealtime=true
RestrictSUIDSGID=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK
LockPersonality=true
SystemCallArchitectures=native
RemoveIPC=true
# Left out on purpose, as in examples/rift.service: MemoryDenyWriteExecute
# (the JVM generates code), ProtectKernelTunables and ProtectControlGroups
# (both make the delegated cgroup read-only) and a SystemCallFilter.

[Install]
WantedBy=multi-user.target
",
        name = options.name,
        user = options.user,
        root = options.root,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _directory: PathBuf,
        config: Config,
    }

    /// A configuration in its own directory with a self-signed messaging
    /// certificate for `names`.
    fn fixture(names: &[&str], extra: &str) -> Fixture {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "rift-node-init-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let certificate = rcgen::generate_simple_self_signed(
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let cert = directory.join("bus.pem");
        let key = directory.join("bus-key.pem");
        std::fs::write(&cert, certificate.cert.pem()).unwrap();
        std::fs::write(&key, certificate.signing_key.serialize_pem()).unwrap();
        let config = Config::from_lua(
            &format!(
                "rift.config.listeners.public='0.0.0.0:25565'
rift.config.backends.lobby='127.0.0.1:25566'
rift.config.routes.public='lobby'
rift.config.messaging={{listen='0.0.0.0:7443',certificate={cert:?},private_key={key:?},principals={{{{name='p',token_env='RIFT_P_TOKEN',publish={{'a'}},subscribe={{'a'}}}}}}}}
{extra}"
            ),
            "rift.lua",
        )
        .unwrap();
        Fixture {
            _directory: directory,
            config,
        }
    }

    fn options(args: &[&str]) -> Options {
        Options::parse(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>()).unwrap()
    }

    fn render(fixture: &Fixture, args: &[&str], token: Option<&str>) -> io::Result<Plan> {
        let token = token.map(str::to_owned);
        plan(
            &fixture.config,
            &options(args),
            &move |_| token.clone(),
            &|| Ok("f".repeat(64)),
        )
    }

    #[test]
    fn a_configured_node_with_web_tls_gets_a_complete_unit_and_passing_checks() {
        let fixture = fixture(
            &["10.0.0.2"],
            "rift.config.web={listen='0.0.0.0:8443',token='0123456789abcdef',tls=true}
rift.config.forwarding={mode='velocity',secret_env='RIFT_FORWARDING_SECRET'}
rift.config.nodes.node1={address='10.0.0.11',labels={'games'}}
rift.config.service_groups.bedwars={command={'java'},port_range={30000,30099},nodes={labels={'games'}}}",
        );
        let plan = render(&fixture, &["node1", "--controller", "10.0.0.2:7443"], None).unwrap();
        let text = &plan.text;
        assert_eq!(plan.problems, 0, "{text}");
        assert!(text.contains("already defines nodes.node1"), "{text}");
        assert!(text.contains(&format!("RIFT_NODE_NODE1_TOKEN={}", "f".repeat(64))));
        assert!(text.contains(&format!("RIFT_NODE_TOKEN={}", "f".repeat(64))));
        // The controller seals the forwarding secret into prepare commands.
        assert!(!text.contains("RIFT_FORWARDING_SECRET"), "{text}");
        assert!(text.contains(
            "ExecStart=/usr/local/bin/rift node --controller 10.0.0.2:7443 --name node1 --root /var/lib/rift-node --ca /etc/rift/rift-ca.pem --web https://10.0.0.2:8443 --allow-program java"
        ), "{text}");
        for line in [
            "KillMode=control-group",
            "Delegate=memory",
            "StateDirectory=rift-node",
            "EnvironmentFile=/etc/rift/node.env",
        ] {
            assert!(text.lines().any(|l| l == line), "{line}: {text}");
        }
        assert!(
            text.contains("ok       the messaging certificate"),
            "{text}"
        );
        assert!(
            text.contains("ok       the web API serves HTTPS on 0.0.0.0:8443"),
            "{text}"
        );
        assert!(
            text.contains("allow TCP 30000-30099 from this host to 10.0.0.11"),
            "{text}"
        );
    }

    #[test]
    fn a_token_in_the_environment_is_reused_and_new_nodes_get_a_snippet() {
        let fixture = fixture(
            &["rift.internal"],
            "rift.config.messaging.advertise='rift.internal:7443'",
        );
        let token = "t".repeat(40);
        let plan = render(&fixture, &["eu-2", "--address", "10.0.0.12"], Some(&token)).unwrap();
        let text = &plan.text;
        assert!(text.contains("rift.config.nodes[\"eu-2\"] = {"), "{text}");
        assert!(
            text.contains(&format!("RIFT_NODE_EU_2_TOKEN={token}")),
            "{text}"
        );
        assert!(text.contains("already set in this shell"), "{text}");
        assert!(text.contains("--controller rift.internal:7443"), "{text}");
        // Web is not configured: templates cannot reach the node.
        assert_eq!(plan.problems, 1, "{text}");
        assert!(text.contains("PROBLEM  the web API is disabled"), "{text}");
        assert!(!text.contains("--web"), "{text}");
    }

    #[test]
    fn certificate_names_and_plain_http_are_reported() {
        let fixture = fixture(
            &["localhost"],
            "rift.config.web={listen='10.0.0.2:8080',token='0123456789abcdef'}",
        );
        let plan = render(
            &fixture,
            &[
                "node1",
                "--address",
                "10.0.0.11",
                "--controller",
                "10.0.0.2:7443",
            ],
            None,
        )
        .unwrap();
        let text = &plan.text;
        assert_eq!(plan.problems, 2, "{text}");
        assert!(
            text.contains("PROBLEM  the agent will reject the messaging certificate"),
            "{text}"
        );
        assert!(text.contains("10.0.0.2"), "{text}");
        assert!(text.contains("serves plain HTTP"), "{text}");
    }

    #[test]
    fn unusable_inputs_are_refused() {
        let fixture = fixture(
            &["10.0.0.2"],
            "rift.config.nodes.node1={address='10.0.0.11'}",
        );
        let error = |args: &[&str]| render(&fixture, args, None).err().unwrap().to_string();
        assert!(error(&["node1"]).contains("--controller"));
        assert!(error(&["node2", "--controller", "10.0.0.2:7443"]).contains("--address"));
        assert!(
            error(&[
                "node1",
                "--address",
                "10.0.0.99",
                "--controller",
                "10.0.0.2:7443"
            ])
            .contains("not \"10.0.0.99\"")
        );
        let parse = |args: &[&str]| {
            Options::parse(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
                .err()
                .unwrap()
                .to_string()
        };
        assert!(parse(&[]).contains("node name is required"));
        assert!(parse(&["local"]).contains("node names"));
        assert!(parse(&["a", "b"]).contains("only one"));
        assert!(parse(&["a", "--root", "/srv/my root"]).contains("--root"));
        assert!(parse(&["a", "--root", "srv"]).contains("absolute"));
        assert!(parse(&["a", "--bogus", "x"]).contains("unknown option"));
    }

    #[test]
    fn a_root_outside_var_lib_must_be_created_and_made_writable() {
        let unit = unit(
            &options(&["node1", "--root", "/srv/rift"]),
            "10.0.0.2:7443",
            Some("rift.internal"),
            None,
        );
        assert!(unit.contains("ReadWritePaths=/srv/rift"), "{unit}");
        assert!(
            unit.contains("install -d -o rift -g rift -m 0750 /srv/rift"),
            "{unit}"
        );
        assert!(unit.contains("--server-name rift.internal"), "{unit}");
        assert!(!unit.contains("StateDirectory"), "{unit}");
    }
}
