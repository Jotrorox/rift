use super::*;
use crate::{server_list, tab_list::TabList};

pub(super) fn parse(root: &Table) -> Result<Option<TabList>, String> {
    let Some(values) = options_map(root, "tab_list")? else {
        return Ok(None);
    };
    fields(&values, &["header", "footer", "players"], "tab_list")?;
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
    let list = TabList {
        header: text("header")?,
        footer: text("footer")?,
        network,
    };
    if list == TabList::default() {
        return Err("tab_list: set header, footer or players = \"network\"".into());
    }
    Ok(Some(list))
}
