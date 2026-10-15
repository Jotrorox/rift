use super::*;
use crate::server_list::{self, Entry, ServerList};

const MAX_HOSTS: usize = 256;
const ENTRY_FIELDS: &[&str] = &[
    "motd",
    "maintenance_motd",
    "version",
    "max_players",
    "favicon",
    "sample",
];

pub(super) fn parse(root: &Table, base: &Path) -> Result<Option<ServerList>, String> {
    let Some(values) = options_map(root, "server_list")? else {
        return Ok(None);
    };
    let mut allowed = ENTRY_FIELDS.to_vec();
    allowed.push("hosts");
    fields(&values, &allowed, "server_list")?;
    let mut list = ServerList {
        defaults: entry(&values, "server_list", base)?,
        hosts: BTreeMap::new(),
    };
    if let Some(hosts) = options_map(&values, "hosts")? {
        // Reuse route pattern rules, including duplicate detection after normalization.
        let mut patterns = Routes::<()>::default();
        for pair in hosts.pairs::<Value, Value>() {
            let (pattern, value) = pair.map_err(|e| e.to_string())?;
            let pattern = string(pattern, "server_list.hosts key")?;
            let path = format!("server_list.hosts[{pattern:?}]");
            if pattern == "*" {
                return Err(format!(
                    "{path}: set network-wide defaults directly on server_list"
                ));
            }
            patterns
                .add_pattern(&pattern, ())
                .map_err(|error| format!("{path}: {error}"))?;
            if list.hosts.len() >= MAX_HOSTS {
                return Err(format!("server_list.hosts: at most {MAX_HOSTS} entries"));
            }
            let values = table(value, &path)?;
            fields(&values, ENTRY_FIELDS, &path)?;
            let normalized = pattern
                .strip_suffix('.')
                .unwrap_or(&pattern)
                .to_ascii_lowercase();
            list.hosts.insert(normalized, entry(&values, &path, base)?);
        }
    }
    list.validate()?;
    Ok(Some(list))
}

fn entry(values: &Table, path: &str, base: &Path) -> Result<Entry, String> {
    let field = |key: &str| -> Result<Option<Value>, String> {
        match values.raw_get::<Value>(key).map_err(|e| e.to_string())? {
            Value::Nil => Ok(None),
            value => Ok(Some(value)),
        }
    };
    let at = |key: &str| format!("{path}.{key}");
    Ok(Entry {
        motd: field("motd")?
            .map(|value| server_list::motd(value, &at("motd")))
            .transpose()?,
        maintenance_motd: field("maintenance_motd")?
            .map(|value| server_list::motd(value, &at("maintenance_motd")))
            .transpose()?,
        version: field("version")?
            .map(|value| server_list::text(value, &at("version")))
            .transpose()?,
        max_players: field("max_players")?
            .map(|value| server_list::count(value, &at("max_players")))
            .transpose()?,
        favicon: field("favicon")?
            .map(|value| {
                let file = string(value, &at("favicon"))?;
                if file.trim().is_empty() || file.contains('\0') {
                    return Err(format!("{}: expected a PNG file path", at("favicon")));
                }
                server_list::favicon_file(&base.join(file))
                    .map_err(|error| format!("{}: {error}", at("favicon")))
            })
            .transpose()?,
        sample: field("sample")?
            .map(|value| server_list::sample(value, &at("sample")))
            .transpose()?,
    })
}
