use super::*;
use crate::text::catalog::Catalog;
use std::sync::Arc;

/// `text = { theme = { name = markup }, messages = { key = markup } }`: the
/// theme's named styles and replacements for Rift's own messages, checked
/// here so a mistake fails the load instead of a player's login.
pub(super) fn parse(root: &Table) -> Result<Arc<Catalog>, String> {
    let Some(values) = options(root, "text", &["theme", "messages"])? else {
        return Ok(Arc::new(Catalog::default()));
    };
    let pairs = |key: &str| -> Result<Vec<(String, String)>, String> {
        let Some(table) = options_map(&values, key)? else {
            return Ok(Vec::new());
        };
        let path = format!("text.{key}");
        let mut pairs = Vec::new();
        for pair in table.pairs::<Value, Value>() {
            let (name, value) = pair.map_err(|error| format!("{path}: {error}"))?;
            let name = string(name, &format!("{path} key"))?;
            let value = string(value, &format!("{path}[{name:?}]"))?;
            if pairs.len() >= 256 {
                return Err(format!("{path}: at most 256 entries"));
            }
            pairs.push((name, value));
        }
        pairs.sort();
        Ok(pairs)
    };
    let theme = pairs("theme")?;
    let messages = pairs("messages")?;
    Catalog::new(
        theme
            .iter()
            .map(|(name, markup)| (name.as_str(), markup.as_str())),
        messages
            .iter()
            .map(|(key, markup)| (key.as_str(), markup.as_str())),
    )
    .map(Arc::new)
}
