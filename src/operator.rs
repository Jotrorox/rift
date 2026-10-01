//! Local operator records. Deployment sources stay private; APIs expose metadata.
use crate::control;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

const HISTORY: usize = 64;
const AUDIT: usize = 1024;
const AUDIT_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Default)]
struct Records {
    deployments: VecDeque<Value>,
    audit: VecDeque<Value>,
    sequence: u64,
}
pub struct Store {
    directory: Option<PathBuf>,
    records: Mutex<Records>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn private_file(path: &Path, append: bool, truncate: bool) -> io::Result<std::fs::File> {
    if fs::symlink_metadata(path).is_ok_and(|m| !m.is_file()) {
        return Err(io::Error::other(
            "operator record must be a regular file, without symlinks",
        ));
    }
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create(true)
        .append(append)
        .truncate(truncate);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}
fn bounded_push(values: &mut VecDeque<Value>, value: Value, limit: usize) {
    values.push_back(value);
    while values.len() > limit {
        values.pop_front();
    }
}
impl Store {
    pub fn open(directory: Option<PathBuf>) -> io::Result<Self> {
        let mut records = Records::default();
        if let Some(directory) = &directory {
            if fs::symlink_metadata(directory).is_ok_and(|m| !m.is_dir()) {
                return Err(io::Error::other(
                    "operator records path must be a directory, without symlinks",
                ));
            }
            fs::create_dir_all(directory)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
            }
            let history = directory.join("deployments.json");
            if history.exists() {
                let mut data = Vec::new();
                fs::File::open(history)?
                    .take((HISTORY * (control::MAX_SOURCE * 6 + 4096) + 1) as u64)
                    .read_to_end(&mut data)?;
                let values: Vec<Value> = serde_json::from_slice(&data).map_err(io::Error::other)?;
                for value in values {
                    records.sequence = records.sequence.max(
                        value["id"]
                            .as_u64()
                            .ok_or_else(|| io::Error::other("invalid deployment id"))?,
                    );
                    if value["source"]
                        .as_str()
                        .is_none_or(|s| s.len() > control::MAX_SOURCE)
                    {
                        return Err(io::Error::other("invalid deployment source"));
                    }
                    bounded_push(&mut records.deployments, value, HISTORY);
                }
            }
            let audit = directory.join("audit.jsonl");
            if audit.exists() {
                let mut file = fs::File::open(&audit)?;
                if file.metadata()?.len() > AUDIT_BYTES + 8192 {
                    return Err(io::Error::other("operator audit file exceeds size limit"));
                }
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                let mut complete = 0;
                for line in bytes.split_inclusive(|byte| *byte == b'\n') {
                    if !line.ends_with(b"\n") {
                        break;
                    }
                    let value = serde_json::from_slice(line).map_err(io::Error::other)?;
                    bounded_push(&mut records.audit, value, AUDIT);
                    complete += line.len();
                }
                // A process interrupted during append may leave a partial final
                // record. Remove only that tail before the next append.
                if complete < bytes.len() {
                    // Windows append handles cannot resize files; recovery needs
                    // ordinary write access without truncating complete records.
                    private_file(&audit, false, false)?.set_len(complete as u64)?;
                }
            }
        }
        Ok(Self {
            directory,
            records: Mutex::new(records),
        })
    }
    pub fn deployments(&self) -> Value {
        let records = self.records.lock().unwrap();
        json!({"deployments":records.deployments.iter().rev().map(|value| {
            let mut value = value.clone(); value.as_object_mut().unwrap().remove("source"); value
        }).collect::<Vec<_>>(), "retained":HISTORY, "durable":self.directory.is_some()})
    }
    pub fn source(&self, id: u64) -> Option<String> {
        self.records
            .lock()
            .unwrap()
            .deployments
            .iter()
            .find(|value| value["id"] == id)
            .and_then(|value| value["source"].as_str().map(str::to_owned))
    }
    pub fn deploy(&self, source: &str, actor: &str, action: &str) -> io::Result<()> {
        let mut records = self.records.lock().unwrap();
        records.sequence += 1;
        let value = json!({"id":records.sequence,"timestamp_unix_ms":now(),"actor":actor,"action":action,"revision":control::revision(source),"source":source});
        let mut deployments = records.deployments.clone();
        bounded_push(&mut deployments, value, HISTORY);
        if let Some(directory) = &self.directory {
            let temporary = directory.join("deployments.tmp");
            let mut file = private_file(&temporary, false, true)?;
            serde_json::to_writer(&mut file, &deployments).map_err(io::Error::other)?;
            file.sync_all()?;
            drop(file);
            fs::rename(temporary, directory.join("deployments.json"))?;
        }
        records.deployments = deployments;
        Ok(())
    }
    pub fn audit(
        &self,
        actor: &str,
        action: &str,
        target: &str,
        outcome: &str,
        status: u16,
    ) -> io::Result<()> {
        let mut records = self.records.lock().unwrap();
        // Only caller-selected identity, operation and target. No bodies, tokens,
        // arbitrary diagnostics, queries or console commands are persisted.
        let value = json!({"timestamp_unix_ms":now(), "actor":actor,"action":action.chars().take(128).collect::<String>(),"target":target.chars().take(512).collect::<String>(),"outcome":outcome,"status":status});
        if let Some(directory) = &self.directory {
            let path = directory.join("audit.jsonl");
            if fs::metadata(&path).is_ok_and(|m| m.len() >= AUDIT_BYTES) {
                let mut file = private_file(&directory.join("audit.tmp"), false, true)?;
                for value in &records.audit {
                    writeln!(file, "{value}")?;
                }
                file.sync_all()?;
                drop(file);
                fs::rename(directory.join("audit.tmp"), &path)?;
            }
            let mut file = private_file(&path, true, false)?;
            writeln!(file, "{value}")?;
            file.sync_data()?;
        }
        bounded_push(&mut records.audit, value, AUDIT);
        Ok(())
    }
    /// Append an audit record without blocking the async runtime on fsync.
    pub async fn audit_async(
        self: &Arc<Self>,
        actor: &str,
        action: &str,
        target: &str,
        outcome: &str,
        status: u16,
    ) -> io::Result<()> {
        let store = self.clone();
        let (actor, action, target, outcome) = (
            actor.to_owned(),
            action.to_owned(),
            target.to_owned(),
            outcome.to_owned(),
        );
        tokio::task::spawn_blocking(move || store.audit(&actor, &action, &target, &outcome, status))
            .await
            .map_err(io::Error::other)?
    }
    /// Record a deployment without blocking the async runtime on fsync.
    pub async fn deploy_async(
        self: &Arc<Self>,
        source: &str,
        actor: &str,
        action: &str,
    ) -> io::Result<()> {
        let store = self.clone();
        let (source, actor, action) = (source.to_owned(), actor.to_owned(), action.to_owned());
        tokio::task::spawn_blocking(move || store.deploy(&source, &actor, &action))
            .await
            .map_err(io::Error::other)?
    }
    pub fn audits(&self) -> Value {
        json!({"records":self.records.lock().unwrap().audit.iter().rev().cloned().collect::<Vec<_>>(),"durable":self.directory.is_some()})
    }
}

/// Apply JSON definitions without parsing or rewriting arbitrary user Lua.
/// A single generated wrapper preserves the original script and hook closures.
pub fn edited_source(
    source: &str,
    section: &str,
    name: &str,
    definition: &Value,
) -> Result<String, control::Error> {
    let maximum_name = if section == "templates" { 128 } else { 107 };
    if !["templates", "service_groups"].contains(&section)
        || name.is_empty()
        || name.len() > maximum_name
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(control::Error::invalid(
            "invalid definition name or section",
        ));
    }
    if !definition.is_object() && !definition.is_null() {
        return Err(control::Error::invalid(
            "definition must be an object or null",
        ));
    }
    fn lua(value: &Value) -> Result<String, control::Error> {
        Ok(match value {
            Value::Null => "nil".into(),
            Value::Bool(value) => value.to_string(),
            Value::Number(value) => value.to_string(),
            Value::String(value) => {
                // Decimal escapes have exactly three digits; JSON's Unicode
                // escapes are not Lua syntax. Work bytewise to retain UTF-8.
                let mut result = String::from("\"");
                for byte in value.bytes() {
                    result.push_str(&format!("\\{byte:03}"));
                }
                result.push('"');
                result
            }
            Value::Array(values) => format!(
                "{{{}}}",
                values
                    .iter()
                    .map(lua)
                    .collect::<Result<Vec<_>, _>>()?
                    .join(",")
            ),
            Value::Object(values) => format!(
                "{{{}}}",
                values
                    .iter()
                    .map(|(k, v)| Ok(format!("[{}]={}", lua(&json!(k))?, lua(v)?)))
                    .collect::<Result<Vec<_>, control::Error>>()?
                    .join(",")
            ),
        })
    }
    let definition = lua(definition)?;
    // Reuse our wrapper on subsequent edits rather than nesting the user file.
    const PREFIX: &str =
        "-- Rift structured definitions v1\nlocal __rift_operator_config = (function()\n";
    const SUFFIX: &str = "\nend)() or rift.config\n-- Rift structured overrides\n";
    const END: &str = "return __rift_operator_config\n";
    let (base, overrides) = if let Some(wrapped) = source
        .strip_prefix(PREFIX)
        .and_then(|s| s.strip_suffix(END))
    {
        wrapped.rsplit_once(SUFFIX).ok_or_else(|| {
            control::Error::invalid(
                "structured definition wrapper was edited; refresh or use the Lua editor",
            )
        })?
    } else {
        (source, "")
    };
    let prefix = format!("__rift_operator_config.{section}[\"{name}\"] = ");
    let overrides: String = overrides
        .lines()
        .filter(|line| !line.starts_with(&prefix))
        .map(|line| format!("{line}\n"))
        .collect();
    let initializer =
        format!("__rift_operator_config.{section} = __rift_operator_config.{section} or {{}}\n");
    let initializer = if overrides.lines().any(|line| line == initializer.trim_end()) {
        String::new()
    } else {
        initializer
    };
    let edited =
        format!("{PREFIX}{base}{SUFFIX}{initializer}{overrides}{prefix}{definition}\n{END}");
    if edited.len() > control::MAX_SOURCE {
        return Err(control::Error::invalid(
            "structured source exceeds 256 KiB limit",
        ));
    }
    Ok(edited)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_history_retains_sources_privately_and_audit_omits_bodies() {
        let store = Store::open(None).unwrap();
        for id in 0..70 {
            store
                .deploy(&format!("-- private-source-{id}"), "tester", "save")
                .unwrap();
        }
        assert!(store.source(1).is_none());
        assert_eq!(store.source(70).as_deref(), Some("-- private-source-69"));
        let listed = store.deployments();
        assert_eq!(listed["deployments"].as_array().unwrap().len(), HISTORY);
        assert!(!listed.to_string().contains("private-source"));
        for _ in 0..1100 {
            store
                .audit("tester", "POST console", "lobby-1", "accepted", 200)
                .unwrap();
        }
        assert_eq!(store.audits()["records"].as_array().unwrap().len(), AUDIT);
    }
    #[test]
    fn interrupted_audit_append_is_recovered_before_subsequent_records() {
        let directory =
            std::env::temp_dir().join(format!("rift-operator-recovery-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let store = Store::open(Some(directory.clone())).unwrap();
        store.audit("tester", "save", "", "accepted", 200).unwrap();
        drop(store);
        private_file(&directory.join("audit.jsonl"), true, false)
            .unwrap()
            .write_all(b"{\"partial\"")
            .unwrap();
        let recovered = Store::open(Some(directory.clone())).unwrap();
        assert_eq!(recovered.audits()["records"].as_array().unwrap().len(), 1);
        recovered
            .audit("tester", "save", "", "accepted", 200)
            .unwrap();
        drop(recovered);
        assert_eq!(
            Store::open(Some(directory.clone())).unwrap().audits()["records"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn structured_edits_preserve_script_style_hooks_without_growing_wrappers() {
        let original = "rift.setup { listeners={main='127.0.0.1:0'}, backends={lobby='127.0.0.1:1'}, routes={main='lobby'} }\nrift.on('http', function() return {body='kept'} end)\n";
        let mut source = original.to_owned();
        for _ in 0..20 {
            source = edited_source(
                &source,
                "templates",
                "game",
                &json!({"server_jar":"assets/server.jar"}),
            )
            .unwrap();
        }
        assert_eq!(source.matches("local __rift_operator_config").count(), 1);
        assert_eq!(source.matches("or {}").count(), 1);
        assert!(source.contains(original));
        let config = rift::config::Config::from_lua(&source, "structured.lua").unwrap();
        assert!(config.on_http.is_some());
        assert!(config.templates.contains_key("game"));
    }
}
