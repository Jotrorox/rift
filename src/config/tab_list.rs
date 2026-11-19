use super::*;
use crate::{
    server_list,
    tab_list::{Players, TabList},
};

/// A name format is legacy text around one name, far below this.
const MAX_FORMAT_BYTES: usize = 256;
/// Service groups `tab_list.groups` may name.
const MAX_GROUPS: usize = 64;

pub(super) fn parse(root: &Table) -> Result<Option<TabList>, String> {
    let Some(values) = options_map(root, "tab_list")? else {
        return Ok(None);
    };
    fields(
        &values,
        &["header", "footer", "players", "groups", "names", "format"],
        "tab_list",
    )?;
    let text = |key: &str| -> Result<Option<serde_json::Value>, String> {
        match values.raw_get::<Value>(key).map_err(|e| e.to_string())? {
            Value::Nil => Ok(None),
            value => server_list::motd(value, &format!("tab_list.{key}")).map(Some),
        }
    };
    let groups = match values
        .raw_get::<Value>("groups")
        .map_err(|e| e.to_string())?
    {
        Value::Nil => None,
        value => {
            let names = string_list(value, "tab_list.groups", MAX_GROUPS)?;
            if names.is_empty() {
                return Err("tab_list.groups: name at least one service group".into());
            }
            let count = names.len();
            let names: BTreeSet<String> = names.into_iter().collect();
            if names.len() != count {
                return Err("tab_list.groups: duplicate service group".into());
            }
            Some(names)
        }
    };
    let players = match values
        .raw_get::<Value>("players")
        .map_err(|e| e.to_string())?
    {
        Value::Nil => Players::Server,
        value => match string(value, "tab_list.players")?.as_str() {
            "server" => Players::Server,
            "network" => Players::Network,
            "group" => Players::Groups(groups.clone()),
            _ => {
                return Err(
                    "tab_list.players: expected \"server\", \"group\" or \"network\"".into(),
                );
            }
        },
    };
    if groups.is_some() && !matches!(players, Players::Groups(_)) {
        return Err("tab_list.groups: needs players = \"group\"".into());
    }
    let backend_names = match values
        .raw_get::<Value>("names")
        .map_err(|e| e.to_string())?
    {
        Value::Nil => false,
        value => match string(value, "tab_list.names")?.as_str() {
            "network" => false,
            "all" => true,
            _ => return Err("tab_list.names: expected \"network\" or \"all\"".into()),
        },
    };
    let format = match values
        .raw_get::<Value>("format")
        .map_err(|e| e.to_string())?
    {
        Value::Nil => None,
        value => {
            let format = string(value, "tab_list.format")?;
            if format.len() > MAX_FORMAT_BYTES {
                return Err(format!("tab_list.format: at most {MAX_FORMAT_BYTES} bytes"));
            }
            if !format.contains("{name}") {
                return Err("tab_list.format: must contain {name}".into());
            }
            Some(format)
        }
    };
    let list = TabList {
        header: text("header")?,
        footer: text("footer")?,
        players,
        backend_names,
        format,
    };
    if list.players == Players::Server
        && !list.backend_names
        && list.header.is_none()
        && list.footer.is_none()
    {
        return Err(
            "tab_list: set header, footer, players = \"group\" or \"network\", or names = \"all\""
                .into(),
        );
    }
    Ok(Some(list))
}
