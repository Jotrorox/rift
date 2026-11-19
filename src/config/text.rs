use super::*;
use crate::text::{catalog::Catalog, messages::Messages};
use std::sync::Arc;

/// Entries in `text.messages` and in each `text.languages` table: Rift's
/// own messages and replacements for plugin messages by qualified key.
const MAX_ENTRIES: usize = 1024;
/// Styles in `text.theme`.
const MAX_STYLES: usize = 256;
const MAX_LANGUAGES: usize = 64;

/// `text = { theme = { name = markup }, messages = { key = markup },
/// languages = { tag = { key = markup } } }`: the theme's named styles,
/// replacements for Rift's own messages and their translations, checked
/// here so a mistake fails the load instead of a player's login.
///
/// `plugins` are the `messages.toml` catalogues of the enabled plugins by
/// namespace, checked with this theme. `namespace:key` entries in
/// `messages` and `languages` replace their messages.
pub(super) fn parse(
    root: &Table,
    plugins: Vec<(String, Messages)>,
) -> Result<Arc<Catalog>, String> {
    let Some(values) = options(root, "text", &["theme", "messages", "languages"])? else {
        let mut catalog = Catalog::default();
        for (namespace, messages) in plugins {
            catalog = catalog.with_plugin(&namespace, messages)?;
        }
        return Ok(Arc::new(catalog));
    };
    let entries =
        |table: Table, path: &str, limit: usize| -> Result<Vec<(String, String)>, String> {
            let mut pairs = Vec::new();
            for pair in table.pairs::<Value, Value>() {
                let (name, value) = pair.map_err(|error| format!("{path}: {error}"))?;
                let name = string(name, &format!("{path} key"))?;
                let value = string(value, &format!("{path}[{name:?}]"))?;
                if pairs.len() >= limit {
                    return Err(format!("{path}: at most {limit} entries"));
                }
                pairs.push((name, value));
            }
            pairs.sort();
            Ok(pairs)
        };
    let pairs = |key: &str, limit: usize| -> Result<Vec<(String, String)>, String> {
        match options_map(&values, key)? {
            Some(table) => entries(table, &format!("text.{key}"), limit),
            None => Ok(Vec::new()),
        }
    };
    let theme = pairs("theme", MAX_STYLES)?;
    let messages = pairs("messages", MAX_ENTRIES)?;
    let mut catalog = Catalog::themed(borrowed(&theme))?;
    // Plugins first: the operator's replacements are checked against them.
    for (namespace, plugin) in plugins {
        catalog = catalog.with_plugin(&namespace, plugin)?;
    }
    catalog = catalog.with_messages(borrowed(&messages))?;
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
            if tags.len() >= MAX_LANGUAGES {
                return Err(format!("text.languages: at most {MAX_LANGUAGES} languages"));
            }
            let messages = entries(table, &format!("text.languages.{tag}"), MAX_ENTRIES)?;
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
