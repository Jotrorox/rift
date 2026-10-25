use super::*;
use std::path::{Component, PathBuf};

/// Immutable source assets copied into an instance's own directory.
/// The jar becomes `server.jar`, plugins retain their filenames, config
/// layers are overlaid onto the server root in order and a map is copied to
/// `world`. With `software`, `server_jar` is the jar's download cache entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerTemplate {
    pub server_jar: PathBuf,
    pub software: Option<crate::software::Software>,
    pub plugins: Vec<PathBuf>,
    pub configs: Vec<PathBuf>,
    pub map: Option<PathBuf>,
    /// Write `eula=true`; the configuration author accepts the Minecraft EULA.
    pub accept_eula: bool,
}

const MAX_CONFIG_LAYERS: usize = 16;

pub(super) fn parse(
    root: &Table,
    base: &Path,
    cache: &Path,
) -> Result<BTreeMap<String, ServerTemplate>, String> {
    let mut result = BTreeMap::new();
    let Some(values) = options_map(root, "templates")? else {
        return Ok(result);
    };
    for pair in values.pairs::<Value, Value>() {
        let (name, value) = pair.map_err(|error| error.to_string())?;
        let name = string(name, "templates key")?;
        let path = format!("templates.{name}");
        let values = table(value, &path)?;
        fields(
            &values,
            &[
                "server_jar",
                "software",
                "plugins",
                "configs",
                "map",
                "accept_eula",
            ],
            &path,
        )?;
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
        let configs = match values
            .raw_get::<Value>("configs")
            .map_err(|error| error.to_string())?
        {
            Value::Nil => Vec::new(),
            value @ Value::String(_) => vec![source(value, "configs")?],
            value => string_list(value, &format!("{path}.configs"), MAX_CONFIG_LAYERS)?
                .into_iter()
                .enumerate()
                .map(|(index, value)| {
                    let path = format!("{path}.configs[{}]", index + 1);
                    validate_path(Path::new(&value), &path)?;
                    normalize_lexical(&base.join(value)).map_err(|error| format!("{path}: {error}"))
                })
                .collect::<Result<_, _>>()?,
        };
        let jar = values
            .raw_get::<Value>("server_jar")
            .map_err(|error| error.to_string())?;
        let software = match values
            .raw_get::<Value>("software")
            .map_err(|error| error.to_string())?
        {
            Value::Nil => None,
            value => Some(software(value, &format!("{path}.software"))?),
        };
        let server_jar = match (&software, jar) {
            (Some(software), Value::Nil) => crate::software::jar_path(cache, software),
            (Some(_), _) => {
                return Err(format!(
                    "{path}: server_jar and software are mutually exclusive"
                ));
            }
            (None, Value::Nil) => {
                return Err(format!("{path}: set either server_jar or software"));
            }
            (None, jar) => source(jar, "server_jar")?,
        };
        result.insert(
            name,
            ServerTemplate {
                server_jar,
                software,
                plugins,
                configs,
                map: optional("map")?,
                accept_eula: boolean(&values, &path, "accept_eula", false)?,
            },
        );
    }
    Ok(result)
}

/// `{ type = "paper", version = "1.21.8" }` or `{ type = "url", url = ..., sha256 = ... }`.
fn software(value: Value, path: &str) -> Result<crate::software::Software, String> {
    let values = table(value, path)?;
    fields(&values, &["type", "version", "url", "sha256"], path)?;
    let field = |key: &str| -> Result<Option<String>, String> {
        match values
            .raw_get::<Value>(key)
            .map_err(|error| error.to_string())?
        {
            Value::Nil => Ok(None),
            value => string(value, &format!("{path}.{key}")).map(Some),
        }
    };
    let kind = field("type")?.ok_or_else(|| format!("{path}.type: required"))?;
    let kind = crate::software::Kind::parse(&kind).ok_or_else(|| {
        format!(
            "{path}.type: expected one of {}",
            crate::software::Kind::ALL
                .map(|kind| kind.as_str())
                .join(", ")
        )
    })?;
    let software = crate::software::Software {
        kind,
        version: field("version")?.unwrap_or_else(|| {
            if kind == crate::software::Kind::Url {
                String::new()
            } else {
                "latest".into()
            }
        }),
        url: field("url")?,
        sha256: field("sha256")?.map(|value| value.to_ascii_lowercase()),
    };
    software
        .validate()
        .map_err(|error| format!("{path}.{error}"))?;
    Ok(software)
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
    let servers = config
        .managed_servers
        .iter()
        .map(|(name, server)| {
            managed::canonicalize_existing(&server.directory)
                .map(|directory| (name, directory))
                .map_err(|error| error.to_string())
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let mut destinations: Vec<_> = servers
        .iter()
        .map(|(name, directory)| {
            (
                format!("managed_servers.{name}.directory"),
                directory.clone(),
            )
        })
        .collect();
    for (name, group) in &config.service_groups {
        let rendered =
            services::render(name, &format!("{name}-1"), group.port_start, &group.server);
        let destination = managed::canonicalize_existing(&rendered.directory)
            .map_err(|error| error.to_string())?;
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
        if !services::safe_name(name) {
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
        if template.configs.len() > MAX_CONFIG_LAYERS {
            return Err(format!(
                "{path}.configs: at most {MAX_CONFIG_LAYERS} directories"
            ));
        }
        if let Some(software) = &template.software {
            software
                .validate()
                .map_err(|error| format!("{path}.software.{error}"))?;
            if template.server_jar != crate::software::jar_path(&config.software_cache, software) {
                return Err(format!(
                    "{path}.server_jar: must be the software cache entry when software is set"
                ));
            }
        }
        for (field, source) in std::iter::once(("server_jar", &template.server_jar))
            .chain(template.plugins.iter().map(|path| ("plugins", path)))
            .chain(template.configs.iter().map(|path| ("configs", path)))
            .chain(template.map.iter().map(|path| ("map", path)))
        {
            validate_path(source, &format!("{path}.{field}"))?;
            let source = managed::canonicalize_existing(source)
                .map_err(|error| format!("{path}.{field}: {error}"))?;
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
        let Some(directory) = servers.get(name) else {
            continue;
        };
        for (other_name, other) in &servers {
            if name != *other_name && overlaps(directory, other) {
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
        assert_eq!(template.configs, [root.join("assets/configs")]);
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
        assert!(config.templates["paper"].configs.is_empty());
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
        // Template groups default to a java command; others still need one.
        let source = fixture("server_jar='assets/server.jar'", "template='paper'").replace(
            "command={'java','-jar','server.jar','--port','{port}'},",
            "",
        );
        let config = Config::from_lua(&source, "command.lua").unwrap();
        assert_eq!(
            config.service_groups["game"].server.command,
            [
                "java",
                "-Xms1024M",
                "-Xmx1024M",
                "-jar",
                "server.jar",
                "nogui"
            ]
        );
        let source = fixture("server_jar='assets/server.jar'", "").replace(
            "command={'java','-jar','server.jar','--port','{port}'},",
            "",
        );
        assert!(Config::from_lua(&source, "command.lua").is_err());
    }

    #[test]
    fn software_templates_use_cloudnet_defaults() {
        let root = root();
        let source = "rift.config.listeners.public='127.0.0.1:25565'
rift.config.routes.public='lobby'
rift.config.templates.lobby={software={type='paper',version='1.21.8'},configs={'local/templates/Global/server','local/templates/Lobby/default'},accept_eula=true}
rift.config.templates.newest={software={type='fabric'}}
rift.config.service_groups.lobby={template='lobby',storage='disposable',port_range={25600,25609},memory_mb=2048}
rift.config.service_groups.survival={template='newest',port_range={25610,25619}}";
        let config = Config::from_lua_at(source, &root.join("rift.lua")).unwrap();
        assert!(
            !root.exists(),
            "configuration loading never downloads or creates files"
        );
        let lobby = &config.templates["lobby"];
        assert_eq!(config.software_cache, root.join("local/versioncache"));
        assert_eq!(config.template_storage, root.join("local/templates"));
        assert_eq!(
            lobby.server_jar,
            root.join("local/versioncache/paper-1.21.8/server.jar")
        );
        assert_eq!(
            lobby.configs,
            [
                root.join("local/templates/Global/server"),
                root.join("local/templates/Lobby/default")
            ]
        );
        assert!(lobby.accept_eula);
        assert_eq!(
            config.templates["newest"]
                .software
                .as_ref()
                .unwrap()
                .version,
            "latest"
        );
        let group = &config.service_groups["lobby"];
        assert!(group.server.directory.ends_with("temp/services/{name}"));
        assert_eq!(group.server.command[1..3], ["-Xms2048M", "-Xmx2048M"]);
        assert!(
            config.service_groups["survival"]
                .server
                .directory
                .ends_with("local/services/{name}")
        );

        let custom = "rift.config.listeners.public='127.0.0.1:25565'
rift.config.routes.public='lobby'
rift.config.software_cache='cache'
rift.config.templates.lobby={software={type='url',url='https://example.com/a.jar',sha256='ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789'}}
rift.config.service_groups.lobby={template='lobby',port_range={25600,25609}}";
        let config = Config::from_lua_at(custom, &root.join("rift.lua")).unwrap();
        let software = config.templates["lobby"].software.as_ref().unwrap();
        assert_eq!(
            software.sha256.as_deref().unwrap(),
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        );
        let jar = &config.templates["lobby"].server_jar;
        assert!(jar.starts_with(root.join("cache")));
        assert!(
            jar.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("url-")
        );
    }

    #[test]
    fn rejects_invalid_software_definitions() {
        let base = |template: &str, group: &str| {
            format!(
                "rift.config.listeners.public='127.0.0.1:25565'
rift.config.routes.public='lobby'
rift.config.templates.lobby={{{template}}}
rift.config.service_groups.lobby={{template='lobby',port_range={{25600,25609}}{group}}}"
            )
        };
        for template in [
            "software={type='paper'},server_jar='paper.jar'",
            "software={type='spigot'}",
            "software={version='1.21.8'}",
            "software={type='paper',version='../x'}",
            "software={type='paper',url='https://example.com/a.jar'}",
            "software={type='url'}",
            "software={type='url',url='http://example.com/a.jar'}",
            "software={type='url',url='https://example.com/a.jar',sha256='abc'}",
            "software={type='paper',unknown=true}",
            "software='paper'",
            "software={type='paper'},accept_eula='yes'",
            "software={type='paper'},configs={'a','{name}'}",
            "plugins={}",
        ] {
            assert!(
                Config::from_lua(&base(template, ""), "software.lua").is_err(),
                "{template}"
            );
        }
        for group in [
            ",memory_mb=64",
            ",memory_mb='1G'",
            ",memory_mb=1024,command={'java'}",
        ] {
            assert!(
                Config::from_lua(&base("software={type='paper'}", group), "memory.lua").is_err(),
                "{group}"
            );
        }
        assert!(
            Config::from_lua(
                &base("software={type='paper'}", "").replace(
                    "rift.config.listeners",
                    "rift.config.software_cache=''\nrift.config.listeners"
                ),
                "cache.lua"
            )
            .is_err()
        );
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
