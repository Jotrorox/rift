//! Isolated filesystem materialization for service instances.
//!
//! Runtime callers must stop and reap a server before removing its directory.
use std::{
    fs,
    io::{self, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

use crate::config::{Config, InstanceStorage, ServiceInstance};

const MARKER: &str = ".rift-instance.json";
static NEXT: AtomicU64 = AtomicU64::new(0);

/// A receipt allows failed registration to roll back only newly created data.
#[derive(Debug)]
pub struct Provisioned {
    created: bool,
    marker: Option<Value>,
}

pub fn provision(config: &Config, name: &str) -> io::Result<()> {
    provision_tracked(config, name).map(|_| ())
}

pub fn provision_tracked(config: &Config, name: &str) -> io::Result<Provisioned> {
    let (instance, directory) = definition(config, name)?;
    let Some(template_name) = &instance.template else {
        if instance.storage == InstanceStorage::Disposable {
            return Err(invalid("disposable instances require a template"));
        }
        fs::create_dir_all(&directory)?;
        return Ok(Provisioned {
            created: false,
            marker: None,
        });
    };
    if fs::symlink_metadata(&directory).is_ok() {
        if instance.storage != InstanceStorage::Persistent {
            return Err(invalid("disposable instance directory already exists"));
        }
        let marker = read_marker(&directory)?;
        verify_marker(&marker, instance, name)?;
        validate_tree(&directory, false)?;
        write_properties(&directory, instance.port)?;
        return Ok(Provisioned {
            created: false,
            marker: Some(marker),
        });
    }
    let template = config
        .templates
        .get(template_name)
        .ok_or_else(|| invalid("unknown server template"))?;
    // Check every input before creating any output, including directory aliases.
    let jar = source(&template.server_jar, &directory, false)?;
    let configs = template
        .configs
        .as_ref()
        .map(|path| source(path, &directory, true))
        .transpose()?;
    let map = template
        .map
        .as_ref()
        .map(|path| source(path, &directory, true))
        .transpose()?;
    let mut plugins = Vec::new();
    let mut plugin_names = std::collections::BTreeSet::new();
    for path in &template.plugins {
        let path = source(path, &directory, false)?;
        let basename = path
            .file_name()
            .ok_or_else(|| invalid("plugin has no filename"))?
            .to_owned();
        if !plugin_names.insert(basename.clone()) {
            return Err(invalid("plugin filenames must be unique"));
        }
        plugins.push((path, basename));
    }
    let parent = directory
        .parent()
        .ok_or_else(|| invalid("instance directory has no parent"))?;
    fs::create_dir_all(parent)?;
    checked_path(parent)?;
    let stage = staging(parent)?;
    let result = (|| {
        if let Some(configs) = configs {
            copy_tree(&configs, &stage, true)?;
        }
        copy_file(&jar, &stage.join("server.jar"))?;
        if !plugins.is_empty() {
            fs::create_dir_all(stage.join("plugins"))?;
            for (plugin, basename) in plugins {
                copy_file(&plugin, &stage.join("plugins").join(basename))?;
            }
        }
        if let Some(map) = map {
            copy_tree(&map, &stage.join("world"), false)?;
        }
        write_properties(&stage, instance.port)?;
        let marker = json!({
            "format": 1,
            "group": instance.group,
            "name": name,
            "template": template_name,
            "storage": instance.storage.as_str(),
            "generation": generation(),
        });
        fs::write(stage.join(MARKER), serde_json::to_vec_pretty(&marker)?)?;
        // Recheck immediately before publishing, so an existing world is never replaced.
        checked_path(&directory)?;
        if fs::symlink_metadata(&directory).is_ok() {
            return Err(invalid("instance directory appeared during provisioning"));
        }
        fs::rename(&stage, &directory)?;
        Ok(Provisioned {
            created: true,
            marker: Some(marker),
        })
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

/// Preflight removal before changing runtime registration.
pub fn validate_remove(config: &Config, name: &str) -> io::Result<()> {
    let (instance, directory) = definition(config, name)?;
    if instance.storage == InstanceStorage::Persistent {
        return Ok(());
    }
    let marker = read_marker(&directory)?;
    verify_marker(&marker, instance, name)?;
    validate_tree(&directory, false)
}

/// Remove disposable data only. The caller must first stop and reap the process.
pub fn remove(config: &Config, name: &str) -> io::Result<()> {
    validate_remove(config, name)?;
    let (instance, directory) = definition(config, name)?;
    if instance.storage == InstanceStorage::Disposable {
        fs::remove_dir_all(directory)?;
    }
    Ok(())
}

/// Roll back new materialization if runtime registration fails before spawning.
pub fn rollback(config: &Config, name: &str, receipt: &Provisioned) -> io::Result<()> {
    if !receipt.created {
        return Ok(());
    }
    let (_, directory) = definition(config, name)?;
    if Some(read_marker(&directory)?) != receipt.marker {
        return Err(invalid("instance ownership changed; refusing rollback"));
    }
    validate_tree(&directory, false)?;
    fs::remove_dir_all(directory)
}

fn definition<'a>(config: &'a Config, name: &str) -> io::Result<(&'a ServiceInstance, PathBuf)> {
    let instance = config
        .instances
        .get(name)
        .ok_or_else(|| invalid("unknown instance"))?;
    config
        .service_groups
        .get(&instance.group)
        .ok_or_else(|| invalid("unknown service group"))?;
    let server = config
        .managed_servers
        .get(name)
        .ok_or_else(|| invalid("instance has no managed server"))?;
    let directory = checked_path(&server.directory)?;
    for (other_name, other) in &config.managed_servers {
        if other_name == name {
            continue;
        }
        let other = checked_path(&other.directory)?;
        if directory.starts_with(&other) || other.starts_with(&directory) {
            return Err(invalid(
                "instance directory overlaps another managed server",
            ));
        }
    }
    Ok((instance, directory))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

// Reject symlink ancestors as well as symlink leaves, including paths with '..'.
fn checked_path(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut result = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => continue,
            Component::ParentDir => {
                result.pop();
            }
            _ => result.push(component.as_os_str()),
        }
        match fs::symlink_metadata(&result) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(invalid(format!(
                    "symlinks are not allowed: {}",
                    result.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    if result.parent().is_none() {
        return Err(invalid("filesystem root cannot be an instance or source"));
    }
    Ok(result)
}

fn source(path: &Path, destination: &Path, directory: bool) -> io::Result<PathBuf> {
    let path = checked_path(path)?;
    let metadata = fs::symlink_metadata(&path)?;
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err(invalid(format!(
            "unexpected template source type: {}",
            path.display()
        )));
    }
    if path.starts_with(destination) || (directory && destination.starts_with(&path)) {
        return Err(invalid("template source overlaps instance directory"));
    }
    if directory {
        validate_tree(&path, true)?;
    }
    Ok(path)
}

fn validate_tree(path: &Path, reject_marker: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_file() {
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(invalid(format!(
            "symlinks and special files are not allowed: {}",
            path.display()
        )));
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if reject_marker && entry.file_name() == MARKER {
            return Err(invalid(
                "template may not supply an instance ownership marker",
            ));
        }
        validate_tree(&entry.path(), reject_marker)?;
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path, reject_marker: bool) -> io::Result<()> {
    checked_path(source)?;
    if !fs::symlink_metadata(source)?.is_dir() {
        return Err(invalid("template directory changed during copy"));
    }
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if reject_marker && entry.file_name() == MARKER {
            return Err(invalid(
                "template may not supply an instance ownership marker",
            ));
        }
        let source = entry.path();
        let destination = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source)?;
        if metadata.is_dir() {
            copy_tree(&source, &destination, reject_marker)?;
        } else if metadata.is_file() {
            copy_file(&source, &destination)?;
        } else {
            return Err(invalid(
                "symlinks and special files are not allowed in templates",
            ));
        }
    }
    Ok(())
}

fn copy_file(source: &Path, destination: &Path) -> io::Result<()> {
    checked_path(source)?;
    if !fs::symlink_metadata(source)?.is_file() {
        return Err(invalid("template file changed during copy"));
    }
    fs::copy(source, destination)?;
    // Copies are independently writable, even when distributed templates are read-only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(destination)?.permissions().mode();
        fs::set_permissions(destination, fs::Permissions::from_mode(mode | 0o200))?;
    }
    #[cfg(windows)]
    {
        let mut permissions = fs::metadata(destination)?.permissions();
        permissions.set_readonly(false);
        fs::set_permissions(destination, permissions)?;
    }
    Ok(())
}

fn read_marker(directory: &Path) -> io::Result<Value> {
    checked_path(directory)?;
    if !fs::symlink_metadata(directory)?.is_dir() {
        return Err(invalid("instance directory is not a directory"));
    }
    let marker = directory.join(MARKER);
    checked_path(&marker)?;
    if !fs::symlink_metadata(&marker)?.is_file() {
        return Err(invalid("ownership marker is not a regular file"));
    }
    serde_json::from_slice(&fs::read(marker)?)
        .map_err(|error| invalid(format!("invalid instance ownership marker: {error}")))
}

fn verify_marker(marker: &Value, instance: &ServiceInstance, name: &str) -> io::Result<()> {
    if marker.get("format") != Some(&json!(1))
        || marker.get("group") != Some(&json!(instance.group))
        || marker.get("name") != Some(&json!(name))
        || marker.get("template") != Some(&json!(instance.template))
        || marker.get("storage") != Some(&json!(instance.storage.as_str()))
        || marker
            .get("generation")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Err(invalid(
            "instance ownership marker does not match its service group, template, or storage",
        ));
    }
    Ok(())
}

fn write_properties(directory: &Path, port: u16) -> io::Result<()> {
    let path = directory.join("server.properties");
    checked_path(&path)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(invalid("server.properties must be a regular file"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let mut output = String::new();
    let mut continuation = false;
    let mut generated = false;
    for line in contents.lines() {
        if !continuation {
            let trimmed = line.trim_start();
            let key = trimmed.split(['=', ':', ' ', '\t']).next().unwrap_or("");
            generated = matches!(key, "server-port" | "server-ip" | "level-name");
        }
        if !generated {
            output.push_str(line);
            output.push('\n');
        }
        continuation = (continuation || !line.trim_start().starts_with(['#', '!']))
            && line
                .as_bytes()
                .iter()
                .rev()
                .take_while(|&&b| b == b'\\')
                .count()
                % 2
                == 1;
    }
    // A blank line terminates any dangling Java-properties continuation before
    // appending the settings controlled by the allocated instance endpoint.
    if !output.is_empty() && !output.ends_with("\n\n") {
        output.push('\n');
    }
    output.push_str(&format!(
        "server-port={port}\nserver-ip=127.0.0.1\nlevel-name=world\n"
    ));
    let temporary = directory.join(format!(".rift-properties-{}", generation()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(output.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn generation() -> String {
    format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn staging(parent: &Path) -> io::Result<PathBuf> {
    for _ in 0..16 {
        let path = parent.join(format!(".rift-stage-{}", generation()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(invalid(
        "could not allocate a unique provisioning staging directory",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        config: Config,
        source: String,
    }
    impl Fixture {
        fn new(storage: &str) -> Self {
            let root = std::env::temp_dir().join(format!("rift-provision-test-{}", generation()));
            fs::create_dir_all(root.join("template/configs/nested")).unwrap();
            let root = fs::canonicalize(root).unwrap();
            fs::create_dir_all(root.join("template/map/region")).unwrap();
            fs::write(root.join("template/server.jar"), b"server bytes").unwrap();
            fs::write(root.join("template/plugin.jar"), b"plugin bytes").unwrap();
            fs::write(
                root.join("template/configs/nested/settings.yml"),
                b"value: seed",
            )
            .unwrap();
            fs::write(root.join("template/configs/server.properties"), "# keep\nmotd=seed\nserver-port=1\nserver-ip=0.0.0.0\nlevel-name=old\nserver-port:2\n").unwrap();
            fs::write(root.join("template/map/level.dat"), b"map seed").unwrap();
            fs::write(root.join("template/map/region/r.0.0.mca"), b"region seed").unwrap();
            let source = format!(
                "return {{listeners={{public='127.0.0.1:25565'}},backends={{}},routes={{public='games'}},templates={{game={{server_jar='template/server.jar',plugins={{'template/plugin.jar'}},configs='template/configs',map='template/map'}}}},service_groups={{games={{template='game',storage='{storage}',directory='instances/{{name}}',command={{'java','-jar','server.jar'}},port_range={{25600,25609}}}}}}}}"
            );
            let mut config = Config::from_lua_at(&source, &root.join("rift.lua")).unwrap();
            config.add_instance("games", "games-1", 25600).unwrap();
            config.add_instance("games", "games-2", 25601).unwrap();
            Self {
                root,
                config,
                source,
            }
        }
        fn directory(&self, name: &str) -> PathBuf {
            self.config.managed_servers[name].directory.clone()
        }
        fn template(&self, suffix: &str) -> PathBuf {
            self.root.join("template").join(suffix)
        }
        fn stages(&self) -> Vec<PathBuf> {
            fs::read_dir(self.root.join("instances"))
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".rift-stage-")
                })
                .collect()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn copies_all_assets_and_generates_instance_properties_without_assuming_eula() {
        let fixture = Fixture::new("disposable");
        provision(&fixture.config, "games-1").unwrap();
        let directory = fixture.directory("games-1");
        assert_eq!(
            fs::read(directory.join("server.jar")).unwrap(),
            b"server bytes"
        );
        assert_eq!(
            fs::read(directory.join("plugins/plugin.jar")).unwrap(),
            b"plugin bytes"
        );
        assert_eq!(
            fs::read(directory.join("nested/settings.yml")).unwrap(),
            b"value: seed"
        );
        assert_eq!(
            fs::read(directory.join("world/region/r.0.0.mca")).unwrap(),
            b"region seed"
        );
        assert!(!directory.join("eula.txt").exists());
        let properties = fs::read_to_string(directory.join("server.properties")).unwrap();
        assert_eq!(
            properties,
            "# keep\nmotd=seed\n\nserver-port=25600\nserver-ip=127.0.0.1\nlevel-name=world\n"
        );
        assert!(fixture.stages().is_empty());
        fs::write(fixture.template("configs/eula.txt"), "eula=true\n").unwrap();
        provision(&fixture.config, "games-2").unwrap();
        assert_eq!(
            fs::read_to_string(fixture.directory("games-2").join("eula.txt")).unwrap(),
            "eula=true\n"
        );
    }

    #[test]
    fn instances_and_template_have_independent_worlds_plugins_and_configs() {
        let fixture = Fixture::new("disposable");
        for name in ["games-1", "games-2"] {
            provision(&fixture.config, name).unwrap();
        }
        let first = fixture.directory("games-1");
        for (file, source, expected) in [
            ("world/level.dat", "map/level.dat", "map seed"),
            ("plugins/plugin.jar", "plugin.jar", "plugin bytes"),
            (
                "nested/settings.yml",
                "configs/nested/settings.yml",
                "value: seed",
            ),
        ] {
            fs::write(first.join(file), b"edited").unwrap();
            assert_eq!(
                fs::read_to_string(fixture.directory("games-2").join(file)).unwrap(),
                expected
            );
            assert_eq!(
                fs::read_to_string(fixture.template(source)).unwrap(),
                expected
            );
        }
        remove(&fixture.config, "games-1").unwrap();
        assert!(!first.exists());
        assert!(fixture.directory("games-2").exists());
        assert!(fixture.template("map/level.dat").exists());
    }

    #[test]
    fn template_switch_preserves_ownership_and_removes_only_selected_instances() {
        let fixture = Fixture::new("disposable");
        provision(&fixture.config, "games-1").unwrap();
        provision(&fixture.config, "games-2").unwrap();
        let first = fixture.directory("games-1");
        let original_marker = fs::read(first.join(MARKER)).unwrap();
        let unrelated = fixture.root.join("instances/unrelated/precious");
        fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
        fs::write(&unrelated, "keep").unwrap();
        fs::write(fixture.template("other.jar"), "other server").unwrap();
        // The original template can disappear from configuration after the switch.
        let changed = fixture
            .source
            .replace("game={", "other={")
            .replace("template='game'", "template='other'")
            .replace("template/server.jar", "template/other.jar");
        let mut reloaded = Config::from_lua_at_with_instances(
            &changed,
            &fixture.root.join("rift.lua"),
            &fixture.config,
        )
        .unwrap();
        assert_eq!(reloaded.instances, fixture.config.instances);
        reloaded.add_instance("games", "games-3", 25602).unwrap();
        provision(&reloaded, "games-3").unwrap();
        let third = &reloaded.managed_servers["games-3"].directory;
        assert_eq!(fs::read(first.join(MARKER)).unwrap(), original_marker);
        assert_eq!(read_marker(&first).unwrap()["template"], "game");
        assert_eq!(read_marker(third).unwrap()["template"], "other");
        assert_eq!(fs::read(first.join("server.jar")).unwrap(), b"server bytes");
        assert_eq!(fs::read(third.join("server.jar")).unwrap(), b"other server");
        // A second reload must preserve both generations of ownership.
        let reloaded =
            Config::from_lua_at_with_instances(&changed, &fixture.root.join("rift.lua"), &reloaded)
                .unwrap();
        remove(&reloaded, "games-1").unwrap();
        assert!(!first.exists());
        assert!(third.exists());
        remove(&reloaded, "games-3").unwrap();
        assert!(!third.exists());
        assert_eq!(fs::read_to_string(&unrelated).unwrap(), "keep");
        assert_eq!(
            fs::read(fixture.directory("games-2").join("server.jar")).unwrap(),
            b"server bytes"
        );
        assert_eq!(
            fs::read(fixture.template("server.jar")).unwrap(),
            b"server bytes"
        );
        assert_eq!(
            fs::read(fixture.template("other.jar")).unwrap(),
            b"other server"
        );
    }

    #[test]
    fn persistent_instance_reuses_original_ownership_after_template_is_unset() {
        let fixture = Fixture::new("persistent");
        provision(&fixture.config, "games-1").unwrap();
        let directory = fixture.directory("games-1");
        let marker = fs::read(directory.join(MARKER)).unwrap();
        fs::write(directory.join("world/level.dat"), "player progress").unwrap();
        let changed = fixture.source.replace("template='game',", "");
        let mut reloaded = Config::from_lua_at_with_instances(
            &changed,
            &fixture.root.join("rift.lua"),
            &fixture.config,
        )
        .unwrap();
        provision(&reloaded, "games-1").unwrap();
        assert_eq!(fs::read(directory.join(MARKER)).unwrap(), marker);
        remove(&reloaded, "games-1").unwrap();
        assert_eq!(
            fs::read_to_string(directory.join("world/level.dat")).unwrap(),
            "player progress"
        );
        reloaded.add_instance("games", "games-3", 25602).unwrap();
        provision(&reloaded, "games-3").unwrap();
        assert!(reloaded.instances["games-3"].template.is_none());
        assert!(
            !reloaded.managed_servers["games-3"]
                .directory
                .join(MARKER)
                .exists()
        );
        // Original ownership still requires a matching marker on reuse.
        fs::write(directory.join(MARKER), "{}").unwrap();
        assert!(provision(&reloaded, "games-1").is_err());
    }

    #[test]
    fn persistent_world_reuses_owned_directory_without_reseeding_and_refreshes_port() {
        let mut fixture = Fixture::new("persistent");
        provision(&fixture.config, "games-1").unwrap();
        let directory = fixture.directory("games-1");
        fs::write(directory.join("world/level.dat"), "player builds").unwrap();
        fs::write(directory.join("server.jar"), "upgraded server").unwrap();
        fs::write(directory.join("plugins/plugin.jar"), "upgraded plugin").unwrap();
        fs::write(directory.join("nested/settings.yml"), "custom settings").unwrap();
        // Seed files may be removed after the first materialization.
        fs::remove_dir_all(fixture.root.join("template")).unwrap();
        fixture.config.instances.get_mut("games-1").unwrap().port = 25609;
        let receipt = provision_tracked(&fixture.config, "games-1").unwrap();
        rollback(&fixture.config, "games-1", &receipt).unwrap();
        remove(&fixture.config, "games-1").unwrap();
        assert_eq!(
            fs::read_to_string(directory.join("world/level.dat")).unwrap(),
            "player builds"
        );
        assert_eq!(
            fs::read_to_string(directory.join("server.jar")).unwrap(),
            "upgraded server"
        );
        assert_eq!(
            fs::read_to_string(directory.join("plugins/plugin.jar")).unwrap(),
            "upgraded plugin"
        );
        assert_eq!(
            fs::read_to_string(directory.join("nested/settings.yml")).unwrap(),
            "custom settings"
        );
        assert!(
            fs::read_to_string(directory.join("server.properties"))
                .unwrap()
                .contains("server-port=25609\n")
        );
    }

    #[test]
    fn disposable_directory_requires_explicit_removal_before_reprovisioning() {
        let fixture = Fixture::new("disposable");
        provision(&fixture.config, "games-1").unwrap();
        fs::write(
            fixture.directory("games-1").join("world/level.dat"),
            "played",
        )
        .unwrap();
        assert!(provision(&fixture.config, "games-1").is_err());
        remove(&fixture.config, "games-1").unwrap();
        provision(&fixture.config, "games-1").unwrap();
        assert_eq!(
            fs::read_to_string(fixture.directory("games-1").join("world/level.dat")).unwrap(),
            "map seed"
        );
    }

    #[test]
    fn failed_copy_leaves_no_target_or_staging_files() {
        let fixture = Fixture::new("disposable");
        // A directory/file collision fails after staging and some successful copies.
        fs::create_dir(fixture.template("configs/server.jar")).unwrap();
        assert!(provision(&fixture.config, "games-1").is_err());
        assert!(!fixture.directory("games-1").exists());
        assert!(fixture.stages().is_empty());
        fs::remove_dir(fixture.template("configs/server.jar")).unwrap();
        provision(&fixture.config, "games-1").unwrap();
    }

    #[test]
    fn missing_sources_duplicate_plugins_and_supplied_markers_fail_safely() {
        let mut fixture = Fixture::new("disposable");
        let template = fixture.config.templates.get_mut("game").unwrap();
        template.plugins.push(template.plugins[0].clone());
        assert!(provision(&fixture.config, "games-1").is_err());
        fixture
            .config
            .templates
            .get_mut("game")
            .unwrap()
            .plugins
            .pop();
        fs::write(fixture.template(&format!("configs/{MARKER}")), "{} ").unwrap();
        assert!(provision(&fixture.config, "games-1").is_err());
        fs::remove_file(fixture.template(&format!("configs/{MARKER}"))).unwrap();
        fs::remove_file(fixture.template("server.jar")).unwrap();
        assert!(provision(&fixture.config, "games-1").is_err());
        assert!(!fixture.directory("games-1").exists());
    }

    #[test]
    fn unowned_directories_and_changed_markers_cannot_be_reused_or_deleted() {
        for storage in ["disposable", "persistent"] {
            let fixture = Fixture::new(storage);
            let directory = fixture.directory("games-1");
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("precious-world"), "keep").unwrap();
            assert!(provision(&fixture.config, "games-1").is_err());
            if storage == "disposable" {
                assert!(remove(&fixture.config, "games-1").is_err());
            }
            assert_eq!(
                fs::read_to_string(directory.join("precious-world")).unwrap(),
                "keep"
            );
            fs::remove_dir_all(&directory).unwrap();
            let receipt = provision_tracked(&fixture.config, "games-1").unwrap();
            let mut marker = read_marker(&directory).unwrap();
            marker["storage"] = json!("tampered");
            fs::write(directory.join(MARKER), serde_json::to_vec(&marker).unwrap()).unwrap();
            assert!(rollback(&fixture.config, "games-1", &receipt).is_err());
            if storage == "disposable" {
                assert!(validate_remove(&fixture.config, "games-1").is_err());
            } else {
                assert!(provision(&fixture.config, "games-1").is_err());
            }
            assert!(directory.exists());
        }
    }

    #[test]
    fn rollback_removes_only_new_generation_and_preserves_templates() {
        let fixture = Fixture::new("persistent");
        let receipt = provision_tracked(&fixture.config, "games-1").unwrap();
        rollback(&fixture.config, "games-1", &receipt).unwrap();
        assert!(!fixture.directory("games-1").exists());
        assert!(fixture.template("map/level.dat").exists());
        provision(&fixture.config, "games-1").unwrap();
        assert!(rollback(&fixture.config, "games-1", &receipt).is_err());
        assert!(fixture.directory("games-1").exists());
    }

    #[test]
    fn legacy_persistent_groups_do_not_claim_or_destroy_existing_data() {
        let mut fixture = Fixture::new("persistent");
        fixture
            .config
            .service_groups
            .get_mut("games")
            .unwrap()
            .template = None;
        fixture.config.instances.clear();
        fixture.config.backends.clear();
        fixture.config.managed_servers.clear();
        fixture
            .config
            .add_instance("games", "games-1", 25600)
            .unwrap();
        let receipt = provision_tracked(&fixture.config, "games-1").unwrap();
        let directory = fixture.directory("games-1");
        fs::write(directory.join("existing"), "keep").unwrap();
        remove(&fixture.config, "games-1").unwrap();
        rollback(&fixture.config, "games-1", &receipt).unwrap();
        assert_eq!(
            fs::read_to_string(directory.join("existing")).unwrap(),
            "keep"
        );
        assert!(!directory.join(MARKER).exists());
        assert!(!directory.join("server.properties").exists());
    }

    #[test]
    fn refuses_source_destination_overlap_and_nested_server_directories() {
        let mut fixture = Fixture::new("disposable");
        fixture.config.templates.get_mut("game").unwrap().configs = Some(fixture.root.clone());
        assert!(provision(&fixture.config, "games-1").is_err());
        fixture.config.templates.get_mut("game").unwrap().configs =
            Some(fixture.template("configs"));
        fixture
            .config
            .managed_servers
            .get_mut("games-2")
            .unwrap()
            .directory = fixture.directory("games-1").join("nested-instance");
        assert!(provision(&fixture.config, "games-1").is_err());
        assert!(!fixture.directory("games-1").exists());
    }

    #[test]
    fn properties_continuations_and_comments_preserve_values_and_endpoint_settings() {
        let fixture = Fixture::new("persistent");
        fs::write(fixture.template("configs/server.properties"), "# comment with slash\\\nserver-port=wrong\\\n   123\nserver-ip:wrong\nlevel-name world\nmotd=continued\\\nserver-port=literal motd text\nother=dangling\\").unwrap();
        provision(&fixture.config, "games-1").unwrap();
        let path = fixture.directory("games-1").join("server.properties");
        let first = fs::read_to_string(&path).unwrap();
        assert!(first.contains("motd=continued\\\nserver-port=literal motd text\n"));
        assert!(first.contains("other=dangling\\\n\nserver-port=25600\n"));
        assert!(!first.contains("server-port=wrong"));
        assert!(!first.contains("server-ip:wrong"));
        assert!(!first.contains("level-name world"));
        provision(&fixture.config, "games-1").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), first);
        fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(provision(&fixture.config, "games-1").is_err());
        assert_eq!(fs::read(&path).unwrap(), [0xff, 0xfe]);
    }

    #[cfg(unix)]
    #[test]
    fn special_files_are_rejected_in_templates_and_owned_properties() {
        use std::os::unix::net::UnixListener;
        let fixture = Fixture::new("persistent");
        let special = fixture.template("configs/socket");
        let _socket = UnixListener::bind(&special).unwrap();
        assert!(provision(&fixture.config, "games-1").is_err());
        assert!(!fixture.directory("games-1").exists());
        fs::remove_file(special).unwrap();
        provision(&fixture.config, "games-1").unwrap();
        let properties = fixture.directory("games-1").join("server.properties");
        fs::remove_file(&properties).unwrap();
        let _properties_socket = UnixListener::bind(&properties).unwrap();
        assert!(provision(&fixture.config, "games-1").is_err());
        assert!(
            fixture
                .directory("games-1")
                .join("world/level.dat")
                .is_file()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inputs_ancestors_properties_and_cleanup_cannot_reach_external_data() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new("disposable");
        let external = fixture.root.join("external");
        fs::create_dir(&external).unwrap();
        fs::write(external.join("precious"), "keep").unwrap();
        symlink(&external, fixture.template("map/link")).unwrap();
        assert!(provision(&fixture.config, "games-1").is_err());
        fs::remove_file(fixture.template("map/link")).unwrap();
        symlink(&external, fixture.root.join("instances")).unwrap();
        assert!(provision(&fixture.config, "games-1").is_err());
        fs::remove_file(fixture.root.join("instances")).unwrap();
        provision(&fixture.config, "games-1").unwrap();
        let directory = fixture.directory("games-1");
        symlink(&external, directory.join("world/link")).unwrap();
        assert!(validate_remove(&fixture.config, "games-1").is_err());
        assert!(remove(&fixture.config, "games-1").is_err());
        fs::remove_file(directory.join("world/link")).unwrap();
        fs::remove_file(directory.join(MARKER)).unwrap();
        symlink(external.join("precious"), directory.join(MARKER)).unwrap();
        assert!(remove(&fixture.config, "games-1").is_err());
        assert_eq!(
            fs::read_to_string(external.join("precious")).unwrap(),
            "keep"
        );
        let persistent = Fixture::new("persistent");
        provision(&persistent.config, "games-1").unwrap();
        fs::remove_file(persistent.directory("games-1").join("server.properties")).unwrap();
        symlink(
            external.join("precious"),
            persistent.directory("games-1").join("server.properties"),
        )
        .unwrap();
        assert!(provision(&persistent.config, "games-1").is_err());
        assert_eq!(
            fs::read_to_string(external.join("precious")).unwrap(),
            "keep"
        );
    }
}
