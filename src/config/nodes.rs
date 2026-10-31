//! Hosts running `rift node`. The proxy stays the controller: it decides what
//! runs where, while each agent only prepares directories and runs processes.
use super::*;
use crate::node::protocol;

const MAX_NODES: usize = 64;
const MAX_LABELS: usize = 32;
/// Shown for servers that the controller runs itself.
pub const LOCAL_NODE: &str = "local";

/// A machine that runs game servers for this proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeConfig {
    /// IP address or DNS name the proxy connects to; game ports are added to it.
    pub address: String,
    /// Memory available to servers; `None` places without a memory limit.
    pub memory_mb: Option<usize>,
    pub max_instances: usize,
    pub labels: BTreeSet<String>,
    /// Environment variable holding the agent's messaging and file token.
    pub token_env: String,
}

impl NodeConfig {
    /// The backend address of a server listening on `port` on this node.
    pub fn backend_address(&self, port: u16) -> String {
        if self.address.contains(':') {
            format!("[{}]:{port}", self.address)
        } else {
            format!("{}:{port}", self.address)
        }
    }

    /// The messaging principal an agent authenticates as.
    pub fn principal(name: &str) -> String {
        format!("node.{name}")
    }
}

/// Which nodes may run a service group's instances. Every listed label must be
/// present on a node; an empty `names` list allows any node with those labels.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeSelector {
    pub names: BTreeSet<String>,
    pub labels: BTreeSet<String>,
}

impl NodeSelector {
    pub fn matches(&self, name: &str, node: &NodeConfig) -> bool {
        (self.names.is_empty() || self.names.contains(name))
            && self.labels.iter().all(|label| node.labels.contains(label))
    }
}

/// How a new instance picks one of the eligible nodes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Placement {
    /// The node with the most unallocated memory.
    #[default]
    LeastMemory,
    /// The node running the fewest instances.
    FewestInstances,
}

impl Placement {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LeastMemory => "least_memory",
            Self::FewestInstances => "fewest_instances",
        }
    }
}

pub(super) fn parse(root: &Table) -> Result<BTreeMap<String, NodeConfig>, String> {
    let mut nodes = BTreeMap::new();
    let Some(values) = options_map(root, "nodes")? else {
        return Ok(nodes);
    };
    for pair in values.pairs::<Value, Value>() {
        let (name, value) = pair.map_err(|e| e.to_string())?;
        let name = string(name, "nodes key")?;
        let path = format!("nodes.{name}");
        let values = table(value, &path)?;
        fields(
            &values,
            &[
                "address",
                "memory_mb",
                "max_instances",
                "labels",
                "token_env",
            ],
            &path,
        )?;
        let address = string(
            values.raw_get("address").map_err(|e| e.to_string())?,
            &format!("{path}.address"),
        )?;
        let memory_mb = match values
            .raw_get::<Value>("memory_mb")
            .map_err(|e| e.to_string())?
        {
            Value::Nil => None,
            _ => Some(integer(&values, &path, "memory_mb", 1, 16_777_216)?),
        };
        let labels = match values
            .raw_get::<Value>("labels")
            .map_err(|e| e.to_string())?
        {
            Value::Nil => BTreeSet::new(),
            value => string_set(value, &format!("{path}.labels"))?,
        };
        let token_env = match values
            .raw_get::<Value>("token_env")
            .map_err(|e| e.to_string())?
        {
            Value::Nil => default_token_env(&name),
            value => string(value, &format!("{path}.token_env"))?,
        };
        nodes.insert(
            name,
            NodeConfig {
                address,
                memory_mb,
                max_instances: integer(&values, &path, "max_instances", 128, 4096)?,
                labels,
                token_env,
            },
        );
    }
    Ok(nodes)
}

/// `RIFT_NODE_<NAME>_TOKEN`, with the name upper-cased and `-` replaced by `_`.
pub fn default_token_env(name: &str) -> String {
    format!(
        "RIFT_NODE_{}_TOKEN",
        name.to_ascii_uppercase().replace('-', "_")
    )
}

/// `nodes = { labels = {...}, names = {...} }`, or a plain list of node names.
pub(super) fn parse_selector(value: Value, path: &str) -> Result<NodeSelector, String> {
    let values = table(value.clone(), path)?;
    if values.raw_len() > 0 {
        return Ok(NodeSelector {
            names: string_set(value, path)?,
            labels: BTreeSet::new(),
        });
    }
    fields(&values, &["names", "labels"], path)?;
    let set = |key: &str| -> Result<BTreeSet<String>, String> {
        match values.raw_get::<Value>(key).map_err(|e| e.to_string())? {
            Value::Nil => Ok(BTreeSet::new()),
            value => string_set(value, &format!("{path}.{key}")),
        }
    };
    Ok(NodeSelector {
        names: set("names")?,
        labels: set("labels")?,
    })
}

pub(super) fn parse_placement(value: Value, path: &str) -> Result<Placement, String> {
    match string(value, path)?.as_str() {
        "least_memory" => Ok(Placement::LeastMemory),
        "fewest_instances" => Ok(Placement::FewestInstances),
        _ => Err(format!(
            "{path}: expected 'least_memory' or 'fewest_instances'"
        )),
    }
}

fn valid_address(address: &str) -> bool {
    if address.contains(':') {
        return address.parse::<std::net::Ipv6Addr>().is_ok();
    }
    !address.is_empty()
        && address.len() <= 253
        && !address.starts_with(['.', '-'])
        && address
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-')
}

fn valid_env(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// A directory below an agent's `--root`: relative, without `.` or `..`.
pub fn valid_node_directory(directory: &Path) -> bool {
    let bytes = directory.as_os_str().as_encoded_bytes();
    !bytes.is_empty()
        && bytes.len() <= 4096
        && !bytes.contains(&0)
        && directory
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

pub(super) fn validate(config: &Config) -> Result<(), String> {
    let nodes = &config.nodes;
    if nodes.len() > MAX_NODES {
        return Err(format!("nodes: at most {MAX_NODES} nodes"));
    }
    let mut tokens = BTreeSet::new();
    for (name, node) in nodes {
        let path = format!("nodes.{name}");
        if !services::safe_name(name) || name.len() > 64 || name == LOCAL_NODE {
            return Err(format!(
                "{path}: names must contain 1..=64 ASCII letters, digits, underscores or hyphens and must not be {LOCAL_NODE:?}"
            ));
        }
        if !valid_address(&node.address) {
            return Err(format!(
                "{path}.address: expected an IP address or DNS name without a port"
            ));
        }
        if node.labels.len() > MAX_LABELS
            || node.labels.iter().any(|label| !services::safe_name(label))
        {
            return Err(format!(
                "{path}.labels: at most {MAX_LABELS} labels of ASCII letters, digits, underscores or hyphens"
            ));
        }
        if node.max_instances == 0 || node.max_instances > 4096 {
            return Err(format!("{path}.max_instances: expected 1..=4096"));
        }
        if node.memory_mb == Some(0) {
            return Err(format!("{path}.memory_mb: expected a positive integer"));
        }
        if !valid_env(&node.token_env) || !tokens.insert(node.token_env.as_str()) {
            return Err(format!(
                "{path}.token_env: expected a unique environment variable name"
            ));
        }
    }
    if !nodes.is_empty() {
        let messaging = config
            .messaging
            .as_ref()
            .filter(|messaging| messaging.listen.is_some())
            .ok_or("nodes: agents connect over QUIC; configure messaging.listen, certificate and private_key")?;
        if messaging.max_payload_bytes < protocol::MAX_MESSAGE {
            return Err(format!(
                "messaging.max_payload_bytes: nodes exchange files and logs in messages of up to {} bytes; keep the default",
                protocol::MAX_MESSAGE
            ));
        }
        for principal in &messaging.principals {
            if principal.name.starts_with("node.") {
                return Err(format!(
                    "messaging.principals: {:?} is reserved; node agents get their own principal",
                    principal.name
                ));
            }
            if tokens.contains(principal.token_env.as_str()) {
                return Err(format!(
                    "messaging.principals: {} is already a node token_env",
                    principal.token_env
                ));
            }
        }
    }
    for (name, group) in &config.service_groups {
        let Some(selector) = &group.nodes else {
            continue;
        };
        let path = format!("service_groups.{name}.nodes");
        if nodes.is_empty() {
            return Err(format!("{path}: no nodes are configured"));
        }
        for node in &selector.names {
            if !nodes.contains_key(node) {
                return Err(format!("{path}: unknown node {node:?}"));
            }
        }
        if !nodes
            .iter()
            .any(|(node, definition)| selector.matches(node, definition))
        {
            return Err(format!("{path}: no configured node matches"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MESSAGING: &str = "rift.config.messaging={listen='0.0.0.0:7443',certificate='cert.pem',private_key='key.pem'}";

    fn config(extra: &str) -> io::Result<Config> {
        Config::from_lua(
            &format!(
                "rift.config.listeners.public='127.0.0.1:25565'
rift.config.backends.lobby='127.0.0.1:25566'
rift.config.routes.public='lobby'
{MESSAGING}
{extra}"
            ),
            "nodes.lua",
        )
    }

    #[test]
    fn nodes_parse_with_defaults_and_selectors() {
        let config = config(
            "rift.config.nodes.node1={address='10.0.0.11',memory_mb=32768,max_instances=40,labels={'games'}}
rift.config.nodes['eu-2']={address='fd00::2'}
rift.config.service_groups.bedwars={template='bedwars',storage='disposable',port_range={30000,30099},nodes={labels={'games'}},placement='fewest_instances'}
rift.config.service_groups.lobby2={command={'java'},port_range={30100,30109},nodes={'eu-2'}}
rift.config.templates.bedwars={server_jar='paper.jar'}",
        )
        .unwrap();
        let node = &config.nodes["node1"];
        assert_eq!(node.memory_mb, Some(32768));
        assert_eq!(node.max_instances, 40);
        assert_eq!(node.token_env, "RIFT_NODE_NODE1_TOKEN");
        assert_eq!(node.backend_address(30000), "10.0.0.11:30000");
        let other = &config.nodes["eu-2"];
        assert_eq!(other.token_env, "RIFT_NODE_EU_2_TOKEN");
        assert_eq!(other.max_instances, 128);
        assert_eq!(other.memory_mb, None);
        assert_eq!(other.backend_address(1), "[fd00::2]:1");
        let group = &config.service_groups["bedwars"];
        assert_eq!(
            group.nodes.as_ref().unwrap().labels,
            BTreeSet::from(["games".to_owned()])
        );
        assert_eq!(group.placement, Placement::FewestInstances);
        let lobby = &config.service_groups["lobby2"];
        assert_eq!(
            lobby.nodes.as_ref().unwrap().names,
            BTreeSet::from(["eu-2".to_owned()])
        );
        assert_eq!(lobby.placement, Placement::LeastMemory);
    }

    #[test]
    fn invalid_nodes_and_selectors_are_rejected() {
        for (extra, expected) in [
            (
                "rift.config.nodes['local']={address='10.0.0.1'}",
                "nodes.local",
            ),
            ("rift.config.nodes.a={address='10.0.0.1:25565'}", "address"),
            ("rift.config.nodes.a={address='bad host'}", "address"),
            ("rift.config.nodes.a={address='h',memory_mb=0}", "memory_mb"),
            ("rift.config.nodes.a={address='h',labels={'a b'}}", "labels"),
            (
                "rift.config.nodes.a={address='h',token_env='X'} rift.config.nodes.b={address='h',token_env='X'}",
                "token_env",
            ),
            ("rift.config.nodes.a={address='h',extra=1}", "unknown field"),
            (
                "rift.config.service_groups.g={command={'java'},port_range={1000,1001},nodes={labels={'games'}}}",
                "no nodes",
            ),
            (
                "rift.config.nodes.a={address='h'} rift.config.service_groups.g={command={'java'},port_range={1000,1001},nodes={labels={'games'}}}",
                "no configured node matches",
            ),
            (
                "rift.config.nodes.a={address='h'} rift.config.service_groups.g={command={'java'},port_range={1000,1001},nodes={'b'}}",
                "unknown node",
            ),
            (
                "rift.config.nodes.a={address='h'} rift.config.service_groups.g={command={'java'},port_range={1000,1001},placement='least_memory'}",
                "placement",
            ),
            (
                "rift.config.nodes.a={address='h'} rift.config.service_groups.g={command={'java'},port_range={1000,1001},nodes={'a'},placement='random'}",
                "placement",
            ),
            (
                "rift.config.nodes.a={address='h'} rift.config.messaging.principals={{name='node.a',token_env='T',publish={'x'},subscribe={'x'}}}",
                "reserved",
            ),
            (
                "rift.config.nodes.a={address='h'} rift.config.messaging.max_payload_bytes=65536",
                "max_payload_bytes",
            ),
        ] {
            let error = config(extra).unwrap_err().to_string();
            assert!(error.contains(expected), "{extra}: {error}");
        }
        let error = Config::from_lua(
            "rift.config.listeners.public='127.0.0.1:25565'
rift.config.backends.lobby='127.0.0.1:25566'
rift.config.routes.public='lobby'
rift.config.nodes.a={address='10.0.0.1'}",
            "nodes.lua",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("messaging.listen"), "{error}");
    }

    #[test]
    fn named_servers_on_nodes_use_the_node_address_and_relative_directories() {
        let config = config(
            "rift.config.nodes.node1={address='10.0.0.11'}
rift.config.backends.survival='10.0.0.11:25570'
rift.config.managed_servers.survival={node='node1',directory='survival',command={'java','-jar','server.jar'}}",
        )
        .unwrap();
        let server = &config.managed_servers["survival"];
        assert_eq!(server.node.as_deref(), Some("node1"));
        assert_eq!(server.directory, Path::new("survival"));
        for (backend, server, expected) in [
            (
                "10.0.0.12:25570",
                "node='node1',directory='survival'",
                "node1",
            ),
            (
                "10.0.0.11:25570",
                "node='node2',directory='survival'",
                "unknown node",
            ),
            (
                "10.0.0.11:25570",
                "node='node1',directory='../survival'",
                "directory",
            ),
            (
                "10.0.0.11:25570",
                "node='node1',directory='/srv/survival'",
                "directory",
            ),
        ] {
            let error = config_with_server(backend, server).unwrap_err().to_string();
            assert!(error.contains(expected), "{server}: {error}");
        }
    }

    fn config_with_server(backend: &str, server: &str) -> io::Result<Config> {
        config(&format!(
            "rift.config.nodes.node1={{address='10.0.0.11'}}
rift.config.backends.survival='{backend}'
rift.config.managed_servers.survival={{{server},command={{'java'}}}}"
        ))
    }
}
