use super::*;

/// A managed server definition rendered separately for each dynamic instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceGroup {
    pub server: ManagedServer,
    pub template: Option<String>,
    pub storage: InstanceStorage,
    pub port_start: u16,
    pub port_end: u16,
    /// Absent for groups managed entirely through explicit instance operations.
    pub scaling: Option<ServiceScaling>,
}

/// Occupancy and queue policy for automatically managed service instances.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceScaling {
    pub min_instances: usize,
    pub max_instances: usize,
    pub spare_instances: usize,
    pub capacity_per_instance: usize,
    pub target_occupancy_percent: usize,
    pub queue_threshold: usize,
    pub cooldown: Duration,
}

/// Persistent worlds retain their data when their process or instance stops.
/// Disposable game instances own a fresh copy of their template assets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InstanceStorage {
    #[default]
    Persistent,
    Disposable,
}

impl InstanceStorage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Persistent => "persistent",
            Self::Disposable => "disposable",
        }
    }
}

/// Runtime metadata; instance definitions cannot be supplied by Lua configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInstance {
    pub group: String,
    pub port: u16,
    /// Ownership policy captured at creation and preserved across reloads.
    pub template: Option<String>,
    pub storage: InstanceStorage,
}

pub(super) fn parse(
    lua: &mlua::Lua,
    root: &Table,
    base: &Path,
) -> Result<BTreeMap<String, ServiceGroup>, String> {
    let Some(values) = options_map(root, "service_groups")? else {
        return Ok(BTreeMap::new());
    };
    let managed_root = lua.create_table().map_err(|e| e.to_string())?;
    let definitions = lua.create_table().map_err(|e| e.to_string())?;
    let mut ranges = BTreeMap::new();
    let mut policies = BTreeMap::new();
    let mut scaling_policies = BTreeMap::new();
    for pair in values.pairs::<Value, Value>() {
        let (name, value) = pair.map_err(|e| e.to_string())?;
        let name = string(name, "service_groups key")?;
        let path = format!("service_groups.{name}");
        let value = table(value, &path)?;
        let template = match value
            .raw_get::<Value>("template")
            .map_err(|e| e.to_string())?
        {
            Value::Nil => None,
            value => Some(string(value, &format!("{path}.template"))?),
        };
        let storage = match value
            .raw_get::<Value>("storage")
            .map_err(|e| e.to_string())?
        {
            Value::Nil => InstanceStorage::Persistent,
            value => match string(value, &format!("{path}.storage"))?.as_str() {
                "persistent" => InstanceStorage::Persistent,
                "disposable" => InstanceStorage::Disposable,
                _ => {
                    return Err(format!(
                        "{path}.storage: expected 'persistent' or 'disposable'"
                    ));
                }
            },
        };
        policies.insert(name.clone(), (template, storage));
        let directory = string(
            value.raw_get("directory").map_err(|e| e.to_string())?,
            &format!("{path}.directory"),
        )?;
        if !directory.contains("{name}") {
            return Err(format!(
                "{path}.directory: must contain {{name}} to isolate instances"
            ));
        }
        let range = table(
            value.raw_get("port_range").map_err(|e| e.to_string())?,
            &format!("{path}.port_range"),
        )?;
        let mut ports = BTreeMap::new();
        for pair in range.pairs::<Value, Value>() {
            let (index, port) = pair.map_err(|e| e.to_string())?;
            let Value::Integer(index) = index else {
                return Err(format!("{path}.port_range: expected {{start, end}}"));
            };
            let port = match port {
                Value::Integer(port) if (1..=65535).contains(&port) => port as u16,
                Value::Number(port) if port.fract() == 0.0 && (1.0..=65535.0).contains(&port) => {
                    port as u16
                }
                _ => {
                    return Err(format!(
                        "{path}.port_range: ports must be integers in 1..=65535"
                    ));
                }
            };
            ports.insert(index, port);
        }
        if ports.len() != 2
            || !ports.contains_key(&1)
            || !ports.contains_key(&2)
            || ports[&1] > ports[&2]
        {
            return Err(format!(
                "{path}.port_range: expected ordered {{start, end}}"
            ));
        }
        ranges.insert(name.clone(), (ports[&1], ports[&2]));
        scaling_policies.insert(
            name.clone(),
            parse_scaling(&value, &path, usize::from(ports[&2] - ports[&1]) + 1)?,
        );
        let definition = lua.create_table().map_err(|e| e.to_string())?;
        for pair in value.pairs::<Value, Value>() {
            let (key, field) = pair.map_err(|e| e.to_string())?;
            if key.as_string().is_some_and(|key| {
                matches!(
                    key.as_bytes().as_ref(),
                    b"port_range" | b"template" | b"storage" | b"scaling"
                )
            }) {
                continue;
            }
            definition.raw_set(key, field).map_err(|e| e.to_string())?;
        }
        definitions
            .raw_set(name, definition)
            .map_err(|e| e.to_string())?;
    }
    managed_root
        .raw_set("managed_servers", definitions)
        .map_err(|e| e.to_string())?;
    Ok(managed::parse(&managed_root, base)
        .map_err(|e| e.replace("managed_servers", "service_groups"))?
        .into_iter()
        .map(|(name, server)| {
            let (port_start, port_end) = ranges[&name];
            let (template, storage) = policies.remove(&name).unwrap();
            let scaling = scaling_policies.remove(&name).unwrap();
            (
                name,
                ServiceGroup {
                    server,
                    template,
                    storage,
                    port_start,
                    port_end,
                    scaling,
                },
            )
        })
        .collect::<BTreeMap<_, _>>())
}

fn parse_scaling(
    values: &Table,
    path: &str,
    ports: usize,
) -> Result<Option<ServiceScaling>, String> {
    let value: Value = values.raw_get("scaling").map_err(|e| e.to_string())?;
    if matches!(value, Value::Nil) {
        return Ok(None);
    }
    let path = format!("{path}.scaling");
    let values = table(value, &path)?;
    fields(
        &values,
        &[
            "min_instances",
            "max_instances",
            "spare_instances",
            "capacity_per_instance",
            "target_occupancy_percent",
            "queue_threshold",
            "cooldown_ms",
        ],
        &path,
    )?;
    if matches!(
        values
            .raw_get::<Value>("capacity_per_instance")
            .map_err(|e| e.to_string())?,
        Value::Nil
    ) {
        return Err(format!(
            "{path}.capacity_per_instance: required positive integer"
        ));
    }
    let scaling = ServiceScaling {
        min_instances: managed::bounded_count(&values, &path, "min_instances", 1, 128)?,
        max_instances: integer(&values, &path, "max_instances", ports.min(128), 128)?,
        spare_instances: managed::bounded_count(&values, &path, "spare_instances", 0, 128)?,
        capacity_per_instance: integer(&values, &path, "capacity_per_instance", 1, 100_000)?,
        target_occupancy_percent: integer(&values, &path, "target_occupancy_percent", 80, 100)?,
        queue_threshold: integer(&values, &path, "queue_threshold", 1, 1024)?,
        cooldown: Duration::from_millis(
            integer(&values, &path, "cooldown_ms", 5000, 86_400_000)? as u64
        ),
    };
    validate_scaling(&scaling, &path, ports)?;
    Ok(Some(scaling))
}

fn validate_scaling(scaling: &ServiceScaling, path: &str, ports: usize) -> Result<(), String> {
    if scaling.max_instances == 0 || scaling.max_instances > 128 || scaling.max_instances > ports {
        return Err(format!(
            "{path}.max_instances: expected 1..={} within the port range",
            ports.min(128)
        ));
    }
    for (field, count) in [
        ("min_instances", scaling.min_instances),
        ("spare_instances", scaling.spare_instances),
    ] {
        if count > scaling.max_instances {
            return Err(format!("{path}.{field}: must not exceed max_instances"));
        }
    }
    for (field, value, max) in [
        (
            "capacity_per_instance",
            scaling.capacity_per_instance,
            100_000,
        ),
        (
            "target_occupancy_percent",
            scaling.target_occupancy_percent,
            100,
        ),
        ("queue_threshold", scaling.queue_threshold, 1024),
    ] {
        if value == 0 || value > max {
            return Err(format!("{path}.{field}: expected an integer in 1..={max}"));
        }
    }
    if scaling.cooldown < Duration::from_millis(1)
        || scaling.cooldown > Duration::from_millis(86_400_000)
    {
        return Err(format!(
            "{path}.cooldown_ms: expected 1..=86400000 milliseconds"
        ));
    }
    Ok(())
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn valid_instance_name(group: &str, name: &str) -> bool {
    safe_name(name)
        && name
            .strip_prefix(group)
            .and_then(|suffix| suffix.strip_prefix('-'))
            .is_some_and(|number| {
                number.bytes().all(|byte| byte.is_ascii_digit())
                    && number.parse::<u64>().is_ok_and(|number| number > 0)
            })
}

pub(super) fn render(
    group: &str,
    name: &str,
    port: u16,
    definition: &ManagedServer,
) -> ManagedServer {
    let substitute = |value: &str| {
        value
            .replace("{name}", name)
            .replace("{group}", group)
            .replace("{port}", &port.to_string())
    };
    let mut server = definition.clone();
    server.command = server
        .command
        .iter()
        .map(|value| substitute(value))
        .collect();
    server.directory = substitute(&server.directory.to_string_lossy()).into();
    server
}

fn render_group(group: &str, name: &str, port: u16, definition: &ServiceGroup) -> ManagedServer {
    let mut server = render(group, name, port, &definition.server);
    // The scaler owns idle capacity, so the process supervisor must not stop
    // instances that satisfy the minimum or spare-capacity policy.
    if definition.scaling.is_some() {
        server.idle_timeout = None;
    }
    server
}

pub(super) fn validate(config: &Config) -> Result<(), String> {
    if config.service_groups.len() > 128 {
        return Err("service_groups: at most 128 groups".into());
    }
    for (name, group) in &config.service_groups {
        let path = format!("service_groups.{name}");
        if let Some(template) = &group.template {
            if !config.templates.contains_key(template) {
                return Err(format!("{path}.template: unknown template {template:?}"));
            }
        } else if group.storage == InstanceStorage::Disposable {
            return Err(format!(
                "{path}.storage: disposable instances require a template"
            ));
        }
        if !safe_name(name) || name.len() > 107 {
            return Err(format!(
                "{path}: names must contain 1..=107 ASCII letters, digits, underscores or hyphens"
            ));
        }
        if config.backends.contains_key(name) || config.managed_servers.contains_key(name) {
            return Err(format!("{path}: name conflicts with an existing backend"));
        }
        if group.port_start == 0 || group.port_start > group.port_end {
            return Err(format!(
                "{path}.port_range: expected ordered ports in 1..=65535"
            ));
        }
        if let Some(scaling) = &group.scaling {
            validate_scaling(
                scaling,
                &format!("{path}.scaling"),
                usize::from(group.port_end - group.port_start) + 1,
            )?;
            if config
                .instances
                .values()
                .filter(|instance| instance.group == *name)
                .count()
                > scaling.max_instances
            {
                return Err(format!(
                    "{path}.scaling.max_instances: existing instance count exceeds maximum"
                ));
            }
        }
        if !group.server.directory.to_string_lossy().contains("{name}") {
            return Err(format!(
                "{path}.directory: must contain {{name}} to isolate instances"
            ));
        }
        // Apply all ordinary managed-server command, directory and policy bounds.
        let server = render_group(name, &format!("{name}-1"), group.port_start, group);
        let backends = BTreeMap::from([(
            name.clone(),
            Backend::parse(&format!("127.0.0.1:{}", group.port_start))
                .map_err(|e| e.to_string())?,
        )]);
        for definition in [&group.server, &server] {
            managed::validate(
                &BTreeMap::from([(name.clone(), definition.clone())]),
                &backends,
            )
            .map_err(|e| e.replace("managed_servers", "service_groups"))?;
        }
    }
    for (name, instance) in &config.instances {
        let path = format!("instances.{name}");
        let group = config
            .service_groups
            .get(&instance.group)
            .ok_or_else(|| format!("{path}: unknown service group"))?;
        if !valid_instance_name(&instance.group, name) {
            return Err(format!("{path}: expected <group>-<positive integer>"));
        }
        if !(group.port_start..=group.port_end).contains(&instance.port) {
            return Err(format!("{path}: port is outside the service group range"));
        }
        let expected = render_group(&instance.group, name, instance.port, group);
        if config.managed_servers.get(name) != Some(&expected) {
            return Err(format!(
                "{path}: managed definition does not match the service group"
            ));
        }
        let address = format!("127.0.0.1:{}", instance.port);
        if !config
            .backends
            .get(name)
            .is_some_and(|backend| backend.address() == address)
        {
            return Err(format!(
                "{path}: backend address does not match the allocated port"
            ));
        }
    }
    Ok(())
}

/// Populate all live names before validating references between instances.
/// The caller owns a fresh candidate, so failures leave the active config intact.
pub(super) fn restore_instances(config: &mut Config, previous: &Config) -> Result<(), String> {
    for (name, instance) in &previous.instances {
        if config.is_destination(name) || config.managed_servers.contains_key(name) {
            return Err(format!(
                "instances.{name}: name conflicts with configuration"
            ));
        }
        let group = config.service_groups.get(&instance.group).ok_or_else(|| {
            format!(
                "instances.{name}: unknown service group {:?}",
                instance.group
            )
        })?;
        let server = render_group(&instance.group, name, instance.port, group);
        config.backends.insert(
            name.clone(),
            Backend::parse(&format!("127.0.0.1:{}", instance.port))
                .map_err(|error| error.to_string())?,
        );
        config.managed_servers.insert(name.clone(), server);
        config.instances.insert(name.clone(), instance.clone());
    }
    Ok(())
}

impl Config {
    pub fn is_destination(&self, name: &str) -> bool {
        self.backends.contains_key(name) || self.service_groups.contains_key(name)
    }

    pub fn instance_names(&self, group: &str) -> Vec<String> {
        self.instances
            .iter()
            .filter(|(_, instance)| instance.group == group)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Expand a destination into its current concrete backend names.
    pub fn destination_names(&self, name: &str) -> Vec<String> {
        if self.service_groups.contains_key(name) {
            self.instance_names(name)
        } else if self.backends.contains_key(name) {
            vec![name.to_owned()]
        } else {
            Vec::new()
        }
    }

    /// Atomically render and register an instance. Runtime code allocates the port.
    pub fn add_instance(&mut self, group: &str, name: &str, port: u16) -> io::Result<()> {
        let definition = self
            .service_groups
            .get(group)
            .ok_or_else(|| invalid(format!("unknown service group {group:?}")))?;
        if !valid_instance_name(group, name) {
            return Err(invalid("instance name must be <group>-<positive integer>"));
        }
        if self.backends.contains_key(name)
            || self.managed_servers.contains_key(name)
            || self.service_groups.contains_key(name)
            || self.instances.contains_key(name)
        {
            return Err(invalid(format!("instance {name:?} already exists")));
        }
        let server = render_group(group, name, port, definition);
        let mut candidate = self.clone();
        candidate
            .backends
            .insert(name.into(), Backend::parse(&format!("127.0.0.1:{port}"))?);
        candidate.managed_servers.insert(name.into(), server);
        candidate.instances.insert(
            name.into(),
            ServiceInstance {
                group: group.into(),
                port,
                template: definition.template.clone(),
                storage: definition.storage,
            },
        );
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn group_source(definition: &str) -> String {
        format!(
            "return {{ listeners={{public='127.0.0.1:25565'}}, backends={{}}, routes={{public='lobby'}}, service_groups={{lobby={{{definition}}}}} }}"
        )
    }

    fn config() -> Config {
        Config::from_lua(&group_source("directory='servers/{group}/{name}', command={'java','-jar','/absolute/paper.jar','--port','{port}','--name={name}','--group={group}'}, port_range={25600,25602}"), "groups.lua").unwrap()
    }

    fn scaled_config(policy: &str) -> io::Result<Config> {
        Config::from_lua(
            &group_source(&format!(
                "directory='servers/{{name}}',command={{'java'}},port_range={{25600,25602}},scaling={{{policy}}}"
            )),
            "scaling.lua",
        )
    }

    #[test]
    fn scaling_defaults_are_opt_in_and_bounded_by_ports() {
        assert!(config().service_groups["lobby"].scaling.is_none());
        let config = scaled_config("capacity_per_instance=20").unwrap();
        assert_eq!(
            config.service_groups["lobby"].scaling,
            Some(ServiceScaling {
                min_instances: 1,
                max_instances: 3,
                spare_instances: 0,
                capacity_per_instance: 20,
                target_occupancy_percent: 80,
                queue_threshold: 1,
                cooldown: Duration::from_secs(5),
            })
        );
        let source = group_source(
            "directory='servers/{name}',command={'java'},port_range={25600,26000},scaling={capacity_per_instance=20}",
        );
        let config = Config::from_lua(&source, "wide-range.lua").unwrap();
        assert_eq!(
            config.service_groups["lobby"]
                .scaling
                .as_ref()
                .unwrap()
                .max_instances,
            128
        );
    }

    #[test]
    fn accepts_zero_minimum_and_explicit_scaling_boundaries() {
        let config = scaled_config("min_instances=0, max_instances=3, spare_instances=3, capacity_per_instance=100000, target_occupancy_percent=100, queue_threshold=1024, cooldown_ms=86400000").unwrap();
        let scaling = config.service_groups["lobby"].scaling.as_ref().unwrap();
        assert_eq!(scaling.min_instances, 0);
        assert_eq!(scaling.spare_instances, 3);
        assert_eq!(scaling.cooldown, Duration::from_secs(86_400));
        scaled_config("min_instances=0.0, spare_instances=0.0, capacity_per_instance=1.0").unwrap();
    }

    #[test]
    fn rejects_invalid_scaling_values_and_relationships() {
        for policy in [
            "",
            "capacity_per_instance=0",
            "capacity_per_instance=-1",
            "capacity_per_instance=100001",
            "capacity_per_instance=1.5",
            "capacity_per_instance='20'",
            "capacity_per_instance=20,unknown=1",
            "capacity_per_instance=20,min_instances=-1",
            "capacity_per_instance=20,min_instances=4",
            "capacity_per_instance=20,max_instances=0",
            "capacity_per_instance=20,max_instances=4",
            "capacity_per_instance=20,max_instances=129",
            "capacity_per_instance=20,spare_instances=4",
            "capacity_per_instance=20,spare_instances=-1",
            "capacity_per_instance=20,target_occupancy_percent=0",
            "capacity_per_instance=20,target_occupancy_percent=101",
            "capacity_per_instance=20,queue_threshold=0",
            "capacity_per_instance=20,queue_threshold=1025",
            "capacity_per_instance=20,cooldown_ms=0",
            "capacity_per_instance=20,cooldown_ms=86400001",
            "capacity_per_instance=0/0",
            "capacity_per_instance=math.huge",
        ] {
            let error = scaled_config(policy).unwrap_err().to_string();
            assert!(
                error.contains("service_groups.lobby.scaling"),
                "{policy}: {error}"
            );
        }
        let source = group_source(
            "directory='servers/{name}',command={'java'},port_range={25600,25602},scaling=true",
        );
        assert!(Config::from_lua(&source, "wrong-type.lua").is_err());
    }

    #[test]
    fn validates_programmatically_changed_scaling() {
        let config = scaled_config("capacity_per_instance=20").unwrap();
        for mutate in [
            |scaling: &mut ServiceScaling| scaling.max_instances = 0,
            |scaling: &mut ServiceScaling| scaling.max_instances = 4,
            |scaling: &mut ServiceScaling| scaling.min_instances = 4,
            |scaling: &mut ServiceScaling| scaling.spare_instances = 4,
            |scaling: &mut ServiceScaling| scaling.capacity_per_instance = 0,
            |scaling: &mut ServiceScaling| scaling.capacity_per_instance = 100_001,
            |scaling: &mut ServiceScaling| scaling.target_occupancy_percent = 0,
            |scaling: &mut ServiceScaling| scaling.target_occupancy_percent = 101,
            |scaling: &mut ServiceScaling| scaling.queue_threshold = 0,
            |scaling: &mut ServiceScaling| scaling.queue_threshold = 1025,
            |scaling: &mut ServiceScaling| scaling.cooldown = Duration::ZERO,
            |scaling: &mut ServiceScaling| scaling.cooldown = Duration::from_millis(86_400_001),
        ] {
            let mut changed = config.clone();
            mutate(
                changed
                    .service_groups
                    .get_mut("lobby")
                    .unwrap()
                    .scaling
                    .as_mut()
                    .unwrap(),
            );
            assert!(changed.validate().is_err());
        }
    }

    #[test]
    fn scaled_instances_leave_idle_capacity_to_the_scaler_on_create_and_reload() {
        let source = group_source(
            "directory='servers/{name}',command={'java'},port_range={25600,25602},idle_timeout_ms=1,scaling={capacity_per_instance=20}",
        );
        let path = Path::new("/tmp/rift-scaled-instance-config/rift.lua");
        let mut config = Config::from_lua_at(&source, path).unwrap();
        config.add_instance("lobby", "lobby-1", 25600).unwrap();
        assert_eq!(
            config.service_groups["lobby"].server.idle_timeout,
            Some(Duration::from_millis(1))
        );
        assert_eq!(config.managed_servers["lobby-1"].idle_timeout, None);
        let restored = Config::from_lua_at_with_instances(&source, path, &config).unwrap();
        assert_eq!(restored.managed_servers["lobby-1"].idle_timeout, None);
        restored.validate().unwrap();
    }

    #[test]
    fn extension_queues_accept_groups_and_reject_unknown_destinations() {
        let source = group_source("directory='servers/{name}',command={'java'},port_range={25600,25602},scaling={capacity_per_instance=20}")
            .replace("service_groups=", "authentication={online_mode=true},forwarding={mode='velocity',secret_env='TEST_SECRET'},extensions={api_version=1,queues={lobby=20}},service_groups=");
        let config = Config::from_lua(&source, "group-queue.lua").unwrap();
        assert!(config.backends.is_empty());
        assert_eq!(config.extensions.as_ref().unwrap().queues["lobby"], 20);
        let unknown = source.replace("queues={lobby=20}", "queues={unknown=20}");
        assert!(
            Config::from_lua(&unknown, "unknown-queue.lua")
                .unwrap_err()
                .to_string()
                .contains("extensions.queues: unknown backend")
        );
    }

    #[test]
    fn scaling_maximum_limits_creation_mutation_and_reload() {
        let mut config = scaled_config("capacity_per_instance=20,max_instances=2").unwrap();
        config.add_instance("lobby", "lobby-1", 25600).unwrap();
        config.add_instance("lobby", "lobby-2", 25601).unwrap();
        config.validate().unwrap();
        let before = config.clone();
        assert!(
            config
                .add_instance("lobby", "lobby-3", 25602)
                .unwrap_err()
                .to_string()
                .contains("existing instance count exceeds maximum")
        );
        assert_eq!(config, before);
        config
            .service_groups
            .get_mut("lobby")
            .unwrap()
            .scaling
            .as_mut()
            .unwrap()
            .max_instances = 1;
        assert!(config.validate().is_err());
        let source = group_source(
            "directory='servers/{name}',command={'java'},port_range={25600,25602},scaling={capacity_per_instance=20,max_instances=1}",
        );
        assert!(
            Config::from_lua_at_with_instances(&source, Path::new("groups.lua"), &before)
                .unwrap_err()
                .to_string()
                .contains("existing instance count exceeds maximum")
        );
    }

    #[test]
    fn parses_groups_without_instances_or_filesystem_effects() {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let directory = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "rift-service-config-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        let source = group_source(
            "directory='servers/{name}', command={'java','-jar','/absolute/paper.jar','--port','{port}'}, port_range={25600,25602}, autostart=true, start_on_connect=false, idle_timeout_ms=123, start_timeout_ms=456, stop_timeout_ms=789, restart_delay_ms=321",
        );
        let config = Config::from_lua_at(&source, &directory.join("rift.lua")).unwrap();
        assert!(!directory.exists());
        assert!(config.backends.is_empty());
        assert!(config.instances.is_empty());
        assert!(config.managed_servers.is_empty());
        assert!(config.is_destination("lobby"));
        assert!(config.destination_names("lobby").is_empty());
        let group = &config.service_groups["lobby"];
        assert_eq!((group.port_start, group.port_end), (25600, 25602));
        assert_eq!(group.server.directory, directory.join("servers/{name}"));
        assert!(group.server.autostart);
        assert!(!group.server.start_on_connect);
        assert_eq!(group.server.idle_timeout, Some(Duration::from_millis(123)));
        assert_eq!(group.server.start_timeout, Duration::from_millis(456));
        assert_eq!(group.server.stop_timeout, Duration::from_millis(789));
        assert_eq!(group.server.restart_delay, Duration::from_millis(321));
    }

    #[test]
    fn instances_render_templates_and_expand_destinations() {
        let mut config = config();
        config.add_instance("lobby", "lobby-2", 25601).unwrap();
        config.add_instance("lobby", "lobby-1", 25600).unwrap();
        assert_eq!(config.instance_names("lobby"), ["lobby-1", "lobby-2"]);
        assert_eq!(config.destination_names("lobby"), ["lobby-1", "lobby-2"]);
        assert_eq!(config.destination_names("lobby-2"), ["lobby-2"]);
        assert!(config.destination_names("missing").is_empty());
        assert_eq!(config.backends["lobby-2"].address(), "127.0.0.1:25601");
        assert_eq!(
            config.instances["lobby-2"],
            ServiceInstance {
                group: "lobby".into(),
                port: 25601,
                template: None,
                storage: InstanceStorage::Persistent,
            }
        );
        let server = &config.managed_servers["lobby-2"];
        assert!(server.directory.ends_with("servers/lobby/lobby-2"));
        assert_eq!(
            server.command,
            [
                "java",
                "-jar",
                "/absolute/paper.jar",
                "--port",
                "25601",
                "--name=lobby-2",
                "--group=lobby"
            ]
        );
        assert_eq!(
            server.start_timeout,
            config.service_groups["lobby"].server.start_timeout
        );
        config.validate().unwrap();
    }

    #[test]
    fn invalid_and_conflicting_allocations_leave_config_unchanged() {
        let mut config = config();
        config.add_instance("lobby", "lobby-1", 25600).unwrap();
        for (group, name, port) in [
            ("missing", "missing-1", 25601),
            ("lobby", "lobby-0", 25601),
            ("lobby", "lobby--1", 25601),
            ("lobby", "lobby-../escape", 25601),
            ("lobby", "lobby-1", 25601),
            ("lobby", "lobby-2", 25600),
            ("lobby", "lobby-2", 25599),
            ("lobby", "lobby-2", 25603),
            ("lobby", "lobby-2", 0),
        ] {
            let before = config.clone();
            assert!(
                config.add_instance(group, name, port).is_err(),
                "{group}/{name}/{port}"
            );
            assert_eq!(config, before);
        }
        config
            .backends
            .insert("static".into(), Backend::parse("localhost:25601").unwrap());
        let before = config.clone();
        assert!(config.add_instance("lobby", "lobby-2", 25601).is_err());
        assert_eq!(config, before);
    }

    #[test]
    fn live_instances_obey_listener_and_admin_socket_conflicts() {
        let mut config = config();
        config
            .listeners
            .insert("public".into(), "0.0.0.0:25601".parse().unwrap());
        assert!(config.add_instance("lobby", "lobby-1", 25601).is_err());
        config
            .listeners
            .insert("public".into(), "127.0.0.1:25565".parse().unwrap());
        config.admin = Some(Admin {
            listen: "127.0.0.1:25601".parse().unwrap(),
            token_env: "RIFT_ADMIN_TOKEN".into(),
            permissions: BTreeSet::from(["servers".into()]),
        });
        assert!(config.add_instance("lobby", "lobby-1", 25601).is_err());
        assert!(config.instances.is_empty());
    }

    #[test]
    fn rejects_malformed_group_templates_and_policy() {
        let definition = "directory='servers/{name}', command={'java','--port','{port}'}, port_range={25600,25602}";
        for source in [
            definition.replace("{25600,25602}", "{0,25602}"),
            definition.replace("{25600,25602}", "{25602,25600}"),
            definition.replace("{25600,25602}", "{25600,65536}"),
            definition.replace("{25600,25602}", "{25600,25600.5}"),
            definition.replace("{25600,25602}", "{25600,25602,25603}"),
            definition.replace("{25600,25602}", "{start=25600,['end']=25602}"),
            definition.replace("servers/{name}", "servers/shared"),
            definition.replace("{'java','--port','{port}'}", "{}"),
            format!("{definition}, start_timeout_ms=0"),
            format!("{definition}, restart_delay_ms=86400001"),
            format!("{definition}, unknown=true"),
        ] {
            assert!(
                Config::from_lua(&group_source(&source), "bad.lua").is_err(),
                "{source}"
            );
        }
        let source = group_source(definition)
            .replace("service_groups={lobby=", "service_groups={['../lobby']=")
            .replace("routes={public='lobby'}", "routes={public='../lobby'}");
        assert!(Config::from_lua(&source, "bad.lua").is_err());
        let source =
            group_source(definition).replace("backends={}", "backends={lobby='127.0.0.1:25566'}");
        assert!(Config::from_lua(&source, "bad.lua").is_err());
        let source = group_source(definition).replace("backends={}", "backends={}, instances={}");
        assert!(Config::from_lua(&source, "bad.lua").is_err());
    }

    #[test]
    fn groups_are_valid_routes_fallbacks_and_network_destinations() {
        let source = group_source("directory='servers/{name}',command={'java'},port_range={25600,25602}")
            .replace("backends={}", "backends={static='127.0.0.1:25566'}, fallbacks={static={'lobby'},lobby={'static'}}, network={initial={'lobby'},hubs={'lobby'},access={lobby={allow={'Notch'},deny={'Blocked'}}}}")
            .replace("routes={public='lobby'}", "routes={public={['*.example.com']='lobby',['*']='static'}}");
        let mut config = Config::from_lua(&source, "network.lua").unwrap();
        assert!(config.can_access("lobby", "Notch"));
        assert!(!config.can_access("lobby", "Other"));
        config.add_instance("lobby", "lobby-1", 25600).unwrap();
        assert!(config.can_access("lobby-1", "Notch"));
        assert!(!config.can_access("lobby-1", "Other"));
        config.network.access.insert(
            "lobby-1".into(),
            ServerAccess {
                allow: None,
                deny: vec!["Notch".into()],
            },
        );
        assert!(!config.can_access("lobby-1", "Notch"));
        assert!(!config.can_access("unknown", "Notch"));
    }

    #[test]
    fn detects_tampered_instance_metadata_definitions_and_endpoints() {
        let mut config = config();
        config.add_instance("lobby", "lobby-1", 25600).unwrap();
        let mut altered = config.clone();
        altered.instances.get_mut("lobby-1").unwrap().group = "missing".into();
        assert!(altered.validate().is_err());
        let mut altered = config.clone();
        altered.instances.get_mut("lobby-1").unwrap().port = 25601;
        assert!(altered.validate().is_err());
        let mut altered = config.clone();
        altered
            .managed_servers
            .get_mut("lobby-1")
            .unwrap()
            .command
            .push("altered".into());
        assert!(altered.validate().is_err());
        let mut altered = config.clone();
        altered
            .backends
            .insert("lobby-1".into(), Backend::parse("[::1]:25600").unwrap());
        assert!(altered.validate().is_err());
        let mut altered = config.clone();
        altered.service_groups.get_mut("lobby").unwrap().port_start = 0;
        assert!(altered.validate().is_err());
        let mut altered = config.clone();
        altered
            .service_groups
            .get_mut("lobby")
            .unwrap()
            .server
            .start_timeout = Duration::ZERO;
        assert!(altered.validate().is_err());
    }

    #[test]
    fn lua_bootstrap_initializes_service_groups_for_registration() {
        let source = "rift.config.listeners.public='127.0.0.1:25565'
rift.config.routes.public='lobby'
rift.config.service_groups.lobby={directory='servers/{name}',command={'java'},port_range={25600,25600}}";
        let mut config = Config::from_lua(source, "registered.lua").unwrap();
        config.add_instance("lobby", "lobby-1", 25600).unwrap();
        assert_eq!(config.instances.len(), 1);
    }

    #[test]
    fn rendered_commands_receive_the_same_argument_bounds() {
        let mut config = config();
        config
            .service_groups
            .get_mut("lobby")
            .unwrap()
            .server
            .command = vec!["java".into(), "{name}".repeat(1000)];
        config.validate().unwrap();
        let before = config.clone();
        assert!(
            config
                .add_instance("lobby", "lobby-9999999999999999999", 25600)
                .is_err()
        );
        assert_eq!(config, before);
        config
            .service_groups
            .get_mut("lobby")
            .unwrap()
            .server
            .command[1] = "x".repeat(8193);
        assert!(config.validate().is_err());
    }

    #[test]
    fn maximum_group_name_reserves_space_for_the_instance_sequence() {
        let mut config = config();
        let definition = config.service_groups.remove("lobby").unwrap();
        let name = "x".repeat(107);
        config
            .routes
            .insert("public".into(), Route::Direct(name.clone()));
        config.service_groups.insert(name.clone(), definition);
        config.validate().unwrap();
        config
            .add_instance(&name, &format!("{name}-{}", u64::MAX), 25600)
            .unwrap();
        let mut too_long = config.clone();
        too_long.instances.clear();
        too_long.backends.clear();
        too_long.managed_servers.clear();
        let definition = too_long.service_groups.remove(&name).unwrap();
        let long_name = "x".repeat(108);
        too_long.service_groups.insert(long_name, definition);
        assert!(
            too_long
                .validate()
                .unwrap_err()
                .to_string()
                .contains("1..=107")
        );
    }

    #[test]
    fn reload_restores_all_instances_before_validating_explicit_references() {
        let path = Path::new("/tmp/rift-services-reload-fixture/rift.lua");
        let source = group_source(
            "directory='servers/{name}',command={'java','{port}'},port_range={25600,25602}",
        )
        .replace("backends={}, ", "");
        let mut previous = Config::from_lua_at(&source, path).unwrap();
        previous.add_instance("lobby", "lobby-1", 25600).unwrap();
        previous.add_instance("lobby", "lobby-2", 25601).unwrap();
        let changed = source.replace("routes={public='lobby'}", "routes={public='lobby-2'}, network={initial={'lobby-2'},hubs={'lobby-1'},access={['lobby-2']={allow={'Notch'}}}}, fallbacks={['lobby-1']={'lobby-2'}}");
        assert!(Config::from_lua_at(&changed, path).is_err());
        let restored = Config::from_lua_at_with_instances(&changed, path, &previous).unwrap();
        assert_eq!(restored.instances, previous.instances);
        assert_eq!(restored.managed_servers, previous.managed_servers);
        assert_eq!(restored.backends, previous.backends);
        assert_eq!(restored.routes["public"], Route::Direct("lobby-2".into()));
        restored.validate().unwrap();
        let invalid = changed.replace("public='lobby-2'", "public='lobby-3'");
        assert!(Config::from_lua_at_with_instances(&invalid, path, &previous).is_err());
        assert_eq!(previous.routes["public"], Route::Direct("lobby".into()));
    }

    #[test]
    fn reload_rejects_instance_collisions_missing_groups_and_invalid_ranges() {
        let path = Path::new("/tmp/rift-services-reload-fixture/rift.lua");
        let source =
            group_source("directory='servers/{name}',command={'java'},port_range={25600,25602}");
        let mut previous = Config::from_lua_at(&source, path).unwrap();
        previous.add_instance("lobby", "lobby-1", 25600).unwrap();
        for changed in [
            source.replace("backends={}", "backends={['lobby-1']='127.0.0.1:25601'}"),
            source.replace("service_groups={lobby=", "service_groups={other="),
            source.replace("{25600,25602}", "{25601,25602}"),
        ] {
            assert!(
                Config::from_lua_at_with_instances(&changed, path, &previous).is_err(),
                "{changed}"
            );
        }
        assert_eq!(previous.backends["lobby-1"].address(), "127.0.0.1:25600");
    }

    #[test]
    fn instance_directory_cannot_alias_a_static_managed_server() {
        let mut config = config();
        let mut server = config.service_groups["lobby"].server.clone();
        server.directory = render("lobby", "lobby-1", 25600, &server).directory;
        config
            .backends
            .insert("static".into(), Backend::parse("127.0.0.1:25566").unwrap());
        config.managed_servers.insert("static".into(), server);
        let before = config.clone();
        assert!(config.add_instance("lobby", "lobby-1", 25600).is_err());
        assert_eq!(config, before);
    }
}
