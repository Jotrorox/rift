use super::*;
use crate::text::catalog::Catalog;
use std::sync::Arc;

/// `text = { theme = { name = markup }, messages = { key = markup },
/// languages = { tag = { key = markup } } }`: the theme's named styles,
/// replacements for Rift's own messages and their translations, checked
/// here so a mistake fails the load instead of a player's login.
pub(super) fn parse(root: &Table) -> Result<Arc<Catalog>, String> {
    let Some(values) = options(root, "text", &["theme", "messages", "languages"])? else {
        return Ok(Arc::new(Catalog::default()));
    };
    let entries = |table: Table, path: &str| -> Result<Vec<(String, String)>, String> {
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
    let pairs = |key: &str| -> Result<Vec<(String, String)>, String> {
        match options_map(&values, key)? {
            Some(table) => entries(table, &format!("text.{key}")),
            None => Ok(Vec::new()),
        }
    };
    let theme = pairs("theme")?;
    let messages = pairs("messages")?;
    let mut catalog = Catalog::new(borrowed(&theme), borrowed(&messages))?;
    if let Some(languages) = options_map(&values, "languages")? {
        let mut tags = Vec::new();
        for pair in languages.pairs::<Value, Value>() {
            let (tag, table) = pair.map_err(|error| format!("text.languages: {error}"))?;
            let tag = string(tag, "text.languages key")?;
            let Value::Table(table) = table else {
                return Err(format!(
                    "text.languages.{tag} must be a table of messages by key"
                ));
            };
            if tags.len() >= 64 {
                return Err("text.languages: at most 64 languages".into());
            }
            let messages = entries(table, &format!("text.languages.{tag}"))?;
            tags.push((tag, messages));
        }
        tags.sort();
        for (tag, messages) in &tags {
            catalog = catalog.with_language(tag, borrowed(messages))?;
        }
    }
    Ok(Arc::new(catalog))
}

fn borrowed(pairs: &[(String, String)]) -> Vec<(&str, &str)> {
    pairs
        .iter()
        .map(|(key, markup)| (key.as_str(), markup.as_str()))
        .collect()
}
