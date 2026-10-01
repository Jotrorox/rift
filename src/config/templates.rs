use super::*;
use std::path::{Component, PathBuf};

/// Immutable source assets copied into an instance's own directory.
/// The jar becomes `server.jar`, plugins retain their filenames, configs are
/// copied to the server root and a map is copied to `world`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerTemplate {
    pub server_jar: PathBuf,
    pub plugins: Vec<PathBuf>,
    pub configs: Option<PathBuf>,
    pub map: Option<PathBuf>,
}

pub(super) fn parse(root: &Table, base: &Path) -> Result<BTreeMap<String, ServerTemplate>, String> {
    let mut result = BTreeMap::new();
    let Some(values) = options_map(root, "templates")? else {
        return Ok(result);
    };
    for pair in values.pairs::<Value, Value>() {
        let (name, value) = pair.map_err(|error| error.to_string())?;
        let name = string(name, "templates key")?;
        let path = format!("templates.{name}");
        let values = table(value, &path)?;
        fields(&values, &["server_jar", "plugins", "configs", "map"], &path)?;
        let source = |value: Value, field: &str| {
            let path = format!("{path}.{field}");
            let value = string(value, &path)?;
            validate_path(Path::new(&value), &path)?;
            normalize_lexical(&base.join(value)).map_err(|error| format!("{path}: {error}"))
        };
        let optional = |field: &str| -> Result<Option<PathBuf>, String> {
            match values
                .raw_get::<Value>(field)
                .map_err(|error| error.to_string())?
            {
                Value::Nil => Ok(None),
                value => source(value, field).map(Some),
            }
        };
        let plugins = match values
            .raw_get::<Value>("plugins")
            .map_err(|error| error.to_string())?
        {
            Value::Nil => Vec::new(),
            value => string_list(value, &format!("{path}.plugins"), 128)?
                .into_iter()
                .enumerate()
                .map(|(index, value)| {
                    let path = format!("{path}.plugins[{}]", index + 1);
                    validate_path(Path::new(&value), &path)?;
                    normalize_lexical(&base.join(value)).map_err(|error| format!("{path}: {error}"))
                })
                .collect::<Result<_, _>>()?,
        };
        result.insert(
            name,
            ServerTemplate {
                server_jar: source(
                    values
                        .raw_get("server_jar")
                        .map_err(|error| error.to_string())?,
                    "server_jar",
                )?,
                plugins,
                configs: optional("configs")?,
                map: optional("map")?,
            },
        );
    }
    Ok(result)
}

fn validate_path(value: &Path, path: &str) -> Result<(), String> {
    let bytes = value.as_os_str().as_encoded_bytes();
    if bytes.is_empty()
        || bytes.len() > 4096
        || bytes.contains(&0)
        || bytes.iter().all(u8::is_ascii_whitespace)
    {
        return Err(format!(
            "{path}: expected 1..=4096 nonblank bytes without NUL"
        ));
    }
    if bytes.contains(&b'{') || bytes.contains(&b'}') {
        return Err(format!(
            "{path}: template asset paths do not support placeholders"
        ));
    }
    Ok(())
}

/// Resolve existing ancestors to catch symlink aliases without creating or
/// requiring files. Source paths may name either files or directories.
fn normalize(value: &Path) -> io::Result<PathBuf> {
    let mut result = PathBuf::new();
    for component in std::path::absolute(value)?.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            component => result.push(component.as_os_str()),
        }
        match fs::canonicalize(&result) {
            Ok(canonical) => result = canonical,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(result)
}

/// Keep configured symlinks visible to provisioning, which rejects them.
/// Validation resolves aliases separately when checking directory overlap.
fn normalize_lexical(value: &Path) -> io::Result<PathBuf> {
    let mut result = PathBuf::new();
    for component in std::path::absolute(value)?.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            component => result.push(component.as_os_str()),
        }
    }
    Ok(result)
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

pub(super) fn validate(config: &Config) -> Result<(), String> {
    if config.templates.len() > 128 {
        return Err("templates: at most 128 templates".into());
    }
    let mut destinations = Vec::new();
    for (name, server) in &config.managed_servers {
        destinations.push((
            format!("managed_servers.{name}.directory"),
            normalize(&server.directory).map_err(|error| error.to_string())?,
        ));
    }
    for (name, group) in &config.service_groups {
        let rendered =
            services::render(name, &format!("{name}-1"), group.port_start, &group.server);
        let destination = normalize(&rendered.directory).map_err(|error| error.to_string())?;
        for (other, directory) in &destinations {
            // Reloads already include this group's first instance.
            if other == &format!("managed_servers.{name}-1.directory") {
                continue;
            }
            if overlaps(&destination, directory) {
                return Err(format!("service_groups.{name}.directory: overlaps {other}"));
            }
        }
        destinations.push((format!("service_groups.{name}.directory"), destination));
    }
    for (name, template) in &config.templates {
        let path = format!("templates.{name}");
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(format!(
                "{path}: names must contain 1..=128 ASCII letters, digits, underscores or hyphens"
            ));
        }
        if template.plugins.len() > 128 {
            return Err(format!("{path}.plugins: at most 128 plugins"));
        }
        let mut plugin_names = BTreeSet::new();
        for plugin in &template.plugins {
            let filename = plugin
                .file_name()
                .ok_or_else(|| format!("{path}.plugins: expected a filename"))?;
            if !plugin_names.insert(filename) {
                return Err(format!(
                    "{path}.plugins: duplicate plugin filename {:?}",
                    filename
                ));
            }
        }
        if template.server_jar.file_name().is_none() {
            return Err(format!("{path}.server_jar: expected a filename"));
        }
        for (field, source) in std::iter::once(("server_jar", &template.server_jar))
            .chain(template.plugins.iter().map(|path| ("plugins", path)))
            .chain(template.configs.iter().map(|path| ("configs", path)))
            .chain(template.map.iter().map(|path| ("map", path)))
        {
            validate_path(source, &format!("{path}.{field}"))?;
            let source = normalize(source).map_err(|error| format!("{path}.{field}: {error}"))?;
            for (destination_name, destination) in &destinations {
                if overlaps(&source, destination) {
                    return Err(format!(
                        "{path}.{field}: source overlaps {destination_name}"
                    ));
                }
            }
        }
    }
    for (name, instance) in &config.instances {
        let Some(server) = config.managed_servers.get(name) else {
            continue;
        };
        let directory = normalize(&server.directory).map_err(|error| error.to_string())?;
        for (other_name, other) in &config.managed_servers {
            if name != other_name
                && overlaps(
                    &directory,
                    &normalize(&other.directory).map_err(|error| error.to_string())?,
                )
            {
                return Err(format!(
                    "instances.{name}.directory: overlaps managed_servers.{other_name}"
                ));
            }
        }
        // Keep a diagnostic attached to an instance when metadata is malformed;
        // services::validate performs the complete metadata checks.
        if !config.service_groups.contains_key(&instance.group) {
            return Err(format!("instances.{name}: unknown service group"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fixture(template: &str, policy: &str) -> String {
        "return {listeners={public='127.0.0.1:25565'},routes={public='game'},templates={paper={TEMPLATE}},service_groups={game={directory='instances/{name}',command={'java','-jar','server.jar','--port','{port}'},port_range={25600,25602},POLICY}}}"
            .replace("TEMPLATE", template)
            .replace("POLICY", policy)
    }

    fn root() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "rift-template-config-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn parses_all_assets_relative_to_config_without_creating_them() {
        let root = root();
        let source = fixture(
            "server_jar='./assets/../assets/paper.jar',plugins={'assets/a.jar','assets/b.jar'},configs='assets/configs',map='assets/map'",
            "template='paper',storage='disposable'",
        );
        let config = Config::from_lua_at(&source, &root.join("rift.lua")).unwrap();
        assert!(!root.exists());
        let template = &config.templates["paper"];
        assert_eq!(template.server_jar, root.join("assets/paper.jar"));
        assert_eq!(
            template.plugins,
            [root.join("assets/a.jar"), root.join("assets/b.jar")]
        );
        assert_eq!(template.configs, Some(root.join("assets/configs")));
        assert_eq!(template.map, Some(root.join("assets/map")));
        assert_eq!(
            config.service_groups["game"].storage,
            InstanceStorage::Disposable
        );
        assert_eq!(
            config.service_groups["game"].template.as_deref(),
            Some("paper")
        );
        assert!(config.instances.is_empty());
    }

    #[test]
    fn persistent_worlds_are_default_and_templates_are_optional() {
        let source = fixture("server_jar='/assets/server.jar'", "");
        let config = Config::from_lua(&source, "persistent.lua").unwrap();
        let group = &config.service_groups["game"];
        assert_eq!(group.storage, InstanceStorage::Persistent);
        assert!(group.template.is_none());
        assert_eq!(group.storage.as_str(), "persistent");
        assert_eq!(InstanceStorage::Disposable.as_str(), "disposable");
        assert_eq!(InstanceStorage::default(), InstanceStorage::Persistent);
        assert_eq!(config.templates["paper"].plugins, Vec::<PathBuf>::new());
        assert_eq!(config.templates["paper"].configs, None);
        assert_eq!(config.templates["paper"].map, None);
        assert!(Config::default().templates.is_empty());
        Config::from_lua(
            &fixture(
                "server_jar='/assets/server.jar'",
                "template='paper',storage='persistent'",
            ),
            "persistent.lua",
        )
        .unwrap();
    }

    #[test]
    fn rejects_malformed_asset_definitions() {
        let invalid = [
            "",
            "server_jar=false",
            "server_jar=''",
            "server_jar='   '",
            "server_jar='assets/\\0.jar'",
            "server_jar='assets/{name}.jar'",
            "server_jar='assets/{group}.jar'",
            "server_jar='assets/{port}.jar'",
            "server_jar='assets/unmatched}.jar'",
            "server_jar='/'",
            "server_jar='paper.jar',unknown=true",
            "server_jar='paper.jar',plugins=false",
            "server_jar='paper.jar',plugins={[2]='x.jar'}",
            "server_jar='paper.jar',plugins={named='x.jar'}",
            "server_jar='paper.jar',plugins={'a.jar',false}",
            "server_jar='paper.jar',plugins={'a.jar','other/a.jar'}",
            "server_jar='paper.jar',plugins={'{name}.jar'}",
            "server_jar='paper.jar',configs=false",
            "server_jar='paper.jar',configs=''",
            "server_jar='paper.jar',map=3",
            "server_jar='paper.jar',map='{name}/world'",
        ];
        for template in invalid {
            assert!(
                Config::from_lua(&fixture(template, "template='paper'"), "invalid.lua").is_err(),
                "{template}"
            );
        }
        for name in ["", "../escape", "a.b", "with space", "ä"] {
            let source = fixture("server_jar='paper.jar'", "")
                .replace("templates={paper=", &format!("templates={{['{name}']="));
            assert!(Config::from_lua(&source, "name.lua").is_err(), "{name}");
        }
        let source = fixture("server_jar='paper.jar'", "").replace(
            "templates={paper=",
            &format!("templates={{['{}']=", "x".repeat(129)),
        );
        assert!(Config::from_lua(&source, "name.lua").is_err());
        let source = fixture(&format!("server_jar='{}'", "x".repeat(4097)), "");
        assert!(Config::from_lua(&source, "long.lua").is_err());
    }

    #[test]
    fn rejects_unknown_templates_invalid_storage_and_disposable_without_template() {
        for policy in [
            "template='missing'",
            "template=''",
            "template=false",
            "storage='temporary'",
            "storage=true",
            "storage='disposable'",
            "template='paper',storage='Disposable'",
        ] {
            assert!(
                Config::from_lua(
                    &fixture("server_jar='assets/server.jar'", policy),
                    "policy.lua"
                )
                .is_err(),
                "{policy}"
            );
        }
        let source = fixture("server_jar='assets/server.jar'", "template='paper'").replace(
            "command={'java','-jar','server.jar','--port','{port}'},",
            "",
        );
        assert!(Config::from_lua(&source, "command.lua").is_err());
    }

    #[test]
    fn sources_cannot_overlap_instance_directories() {
        for asset in [
            "server_jar='instances/game-1/server.jar'",
            "server_jar='assets/server.jar',configs='instances'",
            "server_jar='assets/server.jar',map='instances/game-1/world'",
            "server_jar='assets/server.jar',plugins={'instances/game-1/plugins/p.jar'}",
            "server_jar='assets/server.jar',configs='instances/game-1'",
        ] {
            let error = Config::from_lua(
                &fixture(asset, "template='paper',storage='disposable'"),
                "overlap.lua",
            )
            .unwrap_err();
            assert!(error.to_string().contains("overlaps"), "{asset}: {error}");
        }
        let mut config = Config::from_lua(
            &fixture(
                "server_jar='assets/server.jar',map='instances/game-2/world'",
                "template='paper',storage='disposable'",
            ),
            "later.lua",
        )
        .unwrap();
        config.add_instance("game", "game-1", 25600).unwrap();
        let previous = config.clone();
        let error = config.add_instance("game", "game-2", 25601).unwrap_err();
        assert!(error.to_string().contains("overlaps"));
        assert_eq!(config, previous);
    }

    #[test]
    fn instance_directories_cannot_be_ancestors_of_other_managed_servers() {
        for directory in ["instances", "instances/game-1/world"] {
            let source = fixture("server_jar='assets/server.jar'", "template='paper'")
                .replace("routes={public='game'}", &format!("routes={{public='game'}},backends={{static='127.0.0.1:25566'}},managed_servers={{static={{directory='{directory}',command={{'java'}}}}}}"));
            let error = Config::from_lua(&source, "managed-overlap.lua").unwrap_err();
            assert!(
                error.to_string().contains("overlaps"),
                "{directory}: {error}"
            );
        }
    }

    #[test]
    fn registration_bootstrap_provides_templates() {
        let source = "rift.config.listeners.public='127.0.0.1:25565'
rift.config.routes.public='game'
rift.config.templates.paper={server_jar='assets/server.jar'}
rift.config.service_groups.game={directory='instances/{name}',command={'java'},port_range={25600,25602},template='paper',storage='disposable'}";
        let config = Config::from_lua(source, "register.lua").unwrap();
        assert_eq!(config.templates.len(), 1);
        assert_eq!(
            config.service_groups["game"].template.as_deref(),
            Some("paper")
        );
    }

    #[test]
    fn validates_publicly_mutated_asset_paths_and_references() {
        let mut config = Config::from_lua(
            &fixture("server_jar='assets/server.jar'", "template='paper'"),
            "mutated.lua",
        )
        .unwrap();
        config.templates.get_mut("paper").unwrap().server_jar = PathBuf::from("bad/{name}.jar");
        assert!(config.validate().is_err());
        config.templates.clear();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("unknown template")
        );
    }

    #[cfg(unix)]
    #[test]
    fn canonicalization_detects_symlink_overlap_without_touching_assets() {
        let root = root();
        fs::create_dir_all(root.join("instances/game-1")).unwrap();
        std::os::unix::fs::symlink(root.join("instances/game-1"), root.join("assets")).unwrap();
        let source = fixture(
            "server_jar='elsewhere/server.jar',map='assets'",
            "template='paper',storage='disposable'",
        );
        let error = Config::from_lua_at(&source, &root.join("rift.lua")).unwrap_err();
        assert!(error.to_string().contains("overlaps"));
        assert!(
            root.join("instances/game-1")
                .read_dir()
                .unwrap()
                .next()
                .is_none()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn asset_symlinks_remain_visible_for_provisioning_to_reject() {
        let root = root();
        fs::create_dir_all(root.join("assets")).unwrap();
        fs::write(root.join("assets/original.jar"), b"server").unwrap();
        std::os::unix::fs::symlink("original.jar", root.join("assets/link.jar")).unwrap();
        let config = Config::from_lua_at(
            &fixture("server_jar='assets/link.jar'", "template='paper'"),
            &root.join("rift.lua"),
        )
        .unwrap();
        assert_eq!(
            config.templates["paper"].server_jar,
            root.join("assets/link.jar")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
