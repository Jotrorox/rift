use super::*;
use crate::{server_list, tab_list::TabList};

/// A name format is legacy text around one name, far below this.
const MAX_FORMAT_BYTES: usize = 256;

pub(super) fn parse(root: &Table) -> Result<Option<TabList>, String> {
    let Some(values) = options_map(root, "tab_list")? else {
        return Ok(None);
    };
    fields(
        &values,
        &["header", "footer", "players", "names", "format"],
        "tab_list",
    )?;
    let text = |key: &str| -> Result<Option<serde_json::Value>, String> {
        match values.raw_get::<Value>(key).map_err(|e| e.to_string())? {
            Value::Nil => Ok(None),
            value => server_list::motd(value, &format!("tab_list.{key}")).map(Some),
        }
    };
    let network = match values
        .raw_get::<Value>("players")
        .map_err(|e| e.to_string())?
    {
        Value::Nil => false,
        value => match string(value, "tab_list.players")?.as_str() {
            "server" => false,
            "network" => true,
            _ => return Err("tab_list.players: expected \"server\" or \"network\"".into()),
        },
    };
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
        network,
        backend_names,
        format,
    };
    if !list.network && !list.backend_names && list.header.is_none() && list.footer.is_none() {
        return Err(
            "tab_list: set header, footer, players = \"network\" or names = \"all\"".into(),
        );
    }
    Ok(Some(list))
}
