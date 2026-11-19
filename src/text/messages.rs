//! `messages.toml`: a plugin's message catalogue, beside its `plugin.toml`.
//!
//! ```toml
//! [messages]
//! created = "<good>Party created."
//! invited = { text = "<accent><from></accent> invited you to a party.", args = ["from"] }
//! tagged  = { text = "<team>[<tag>]</team> <name>", args = ["tag", "name"], styles = ["team"] }
//!
//! [languages.de]
//! created = "<good>Party erstellt."
//! ```
//!
//! The plugin folder's name is the catalogue's namespace. Parsing is strict,
//! like `plugin.toml`: unknown keys, wrong types and out-of-range values are
//! errors. Templates are checked against the theme later, when the
//! configuration that enables the plugin assembles its
//! [`Catalog`](super::catalog::Catalog).
use super::markup;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read},
    path::Path,
};

pub const FILE: &str = "messages.toml";
/// The namespace of Rift's own messages; no plugin catalogue may use it.
pub const RESERVED: &str = "rift";
pub const MAX_BYTES: usize = 256 * 1024;
pub const MAX_MESSAGES: usize = 256;
pub const MAX_LANGUAGES: usize = 64;
pub const MAX_TEMPLATE_BYTES: usize = 4096;
/// Arguments and style arguments of one message together.
pub const MAX_ARGUMENTS: usize = 16;

/// One message: its default template, the arguments and styles every one of
/// its templates may use, and the plugin's translations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Message {
    pub text: String,
    /// Content arguments, such as `<from>`.
    pub args: Vec<String>,
    /// Style arguments, such as `<team>...</team>`.
    pub styles: Vec<String>,
    /// `[languages.<tag>]` templates by normalised tag.
    pub languages: BTreeMap<String, String>,
}

/// A parsed `messages.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Messages {
    messages: BTreeMap<String, Message>,
    languages: BTreeSet<String>,
}

impl Messages {
    /// Parse `messages.toml` text for the folder `plugins/<namespace>/`.
    pub fn parse(text: &str, namespace: &str) -> Result<Self, String> {
        if namespace == RESERVED {
            return Err(format!(
                "the namespace \"{RESERVED}\" is reserved for Rift's own messages; rename the plugin folder"
            ));
        }
        if text.len() > MAX_BYTES {
            return Err(format!("{FILE} exceeds {} KiB", MAX_BYTES / 1024));
        }
        let root: toml::Table = toml::from_str(text).map_err(|error| {
            // toml's messages span several lines with a source excerpt.
            let message = error.message().trim().to_owned();
            match error.span() {
                Some(span) => {
                    let line = text[..span.start.min(text.len())].lines().count().max(1);
                    format!("line {line}: {message}")
                }
                None => message,
            }
        })?;
        if let Some(key) = root
            .keys()
            .find(|key| !matches!(key.as_str(), "messages" | "languages"))
        {
            return Err(format!(
                "unknown key {key:?}; expected [messages] and [languages.<tag>]"
            ));
        }
        let mut catalog = Self::default();
        if let Some(value) = root.get("messages") {
            let table = value
                .as_table()
                .ok_or_else(|| "[messages] must be a table of messages by key".to_owned())?;
            if table.len() > MAX_MESSAGES {
                return Err(format!("[messages]: at most {MAX_MESSAGES} messages"));
            }
            for (key, value) in table {
                catalog.messages.insert(key.clone(), message(key, value)?);
            }
        }
        if let Some(value) = root.get("languages") {
            let table = value.as_table().ok_or_else(|| {
                "[languages] must hold a table per language, such as [languages.de]".to_owned()
            })?;
            if table.len() > MAX_LANGUAGES {
                return Err(format!("[languages]: at most {MAX_LANGUAGES} languages"));
            }
            for (tag, value) in table {
                let language = crate::directory::language_tag(tag)
                    .map_err(|error| format!("[languages.{tag}]: {error}"))?;
                if !catalog.languages.insert(language.clone()) {
                    return Err(format!(
                        "[languages.{tag}]: the language {language} appears twice"
                    ));
                }
                let templates = value.as_table().ok_or_else(|| {
                    format!("[languages.{tag}] must be a table of templates by key")
                })?;
                for (key, value) in templates {
                    let label = format!("[languages.{tag}] {key:?}");
                    let message = catalog.messages.get_mut(key).ok_or_else(|| {
                        format!("{label}: there is no message {key:?} in [messages]")
                    })?;
                    message
                        .languages
                        .insert(language.clone(), template(&label, value)?);
                }
            }
        }
        Ok(catalog)
    }

    /// The message `key`.
    pub fn get(&self, key: &str) -> Option<&Message> {
        self.messages.get(key)
    }

    /// Every message by key, sorted.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Message)> {
        self.messages
            .iter()
            .map(|(key, message)| (key.as_str(), message))
    }

    /// How many messages the catalogue has.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// The tags of the file's `[languages.<tag>]` tables, normalised.
    pub fn languages(&self) -> impl Iterator<Item = &str> {
        self.languages.iter().map(String::as_str)
    }
}

/// `folder/messages.toml` for the plugin `namespace`, or `None` when the
/// folder has none. Errors name the file.
pub fn read(folder: &Path, namespace: &str) -> Option<Result<Messages, String>> {
    let path = folder.join(FILE);
    let fail = |message: &dyn std::fmt::Display| format!("{}: {message}", path.display());
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => return Some(Err(fail(&error))),
        Ok(metadata) if !metadata.is_file() => return Some(Err(fail(&"must be a regular file"))),
        Ok(_) => {}
    }
    let mut text = String::new();
    let read = fs::File::open(&path)
        .and_then(|file| file.take(MAX_BYTES as u64 + 1).read_to_string(&mut text))
        .map_err(|error| fail(&error));
    Some(read.and_then(|_| Messages::parse(&text, namespace).map_err(|error| fail(&error))))
}

/// A message key: 1..=64 characters of `a-z`, `0-9`, `_`, `-` and `.`.
pub fn valid_key(key: &str) -> bool {
    (1..=64).contains(&key.len())
        && key.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.')
        })
}

/// A `[messages]` entry: a template, or a table with `text`, `args` and
/// `styles`.
fn message(key: &str, value: &toml::Value) -> Result<Message, String> {
    let label = format!("[messages] {key:?}");
    if !valid_key(key) {
        return Err(format!(
            "{label}: a key is 1..=64 characters of a-z, 0-9, '_', '-' and '.'"
        ));
    }
    let fields = match value {
        toml::Value::String(_) => {
            return Ok(Message {
                text: template(&label, value)?,
                ..Message::default()
            });
        }
        toml::Value::Table(fields) => fields,
        other => {
            return Err(format!(
                "{label} must be a template or a table with text, args and styles, got {}",
                other.type_str()
            ));
        }
    };
    if let Some(field) = fields
        .keys()
        .find(|field| !matches!(field.as_str(), "text" | "args" | "styles"))
    {
        return Err(format!(
            "{label}: unknown key {field:?}; a message takes text, args and styles (quote a key that contains '.')"
        ));
    }
    let text = fields
        .get("text")
        .ok_or_else(|| format!("{label}: text is required"))?;
    let text = template(&format!("{label}.text"), text)?;
    let args = names(&label, fields.get("args"), "args")?;
    let styles = names(&label, fields.get("styles"), "styles")?;
    if let Some(name) = args.iter().find(|name| styles.contains(name)) {
        return Err(format!("{label}: {name:?} is in both args and styles"));
    }
    if args.len() + styles.len() > MAX_ARGUMENTS {
        return Err(format!(
            "{label}: at most {MAX_ARGUMENTS} arguments and styles together"
        ));
    }
    Ok(Message {
        text,
        args,
        styles,
        languages: BTreeMap::new(),
    })
}

/// `args` or `styles`: a list of distinct argument names.
fn names(label: &str, value: Option<&toml::Value>, field: &str) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let label = format!("{label}.{field}");
    let items = value
        .as_array()
        .ok_or_else(|| format!("{label} must be a list of names"))?;
    if items.len() > MAX_ARGUMENTS {
        return Err(format!(
            "{label}: at most {MAX_ARGUMENTS} arguments and styles together"
        ));
    }
    let mut names = Vec::new();
    for item in items {
        let name = item
            .as_str()
            .ok_or_else(|| format!("{label} must be a list of names"))?;
        markup::custom_name(name, &label)?;
        if names.iter().any(|existing| existing == name) {
            return Err(format!("{label}: {name:?} appears twice"));
        }
        names.push(name.to_owned());
    }
    Ok(names)
}

fn template(label: &str, value: &toml::Value) -> Result<String, String> {
    let text = value.as_str().ok_or_else(|| {
        format!(
            "{label} must be a template string, got {}",
            value.type_str()
        )
    })?;
    if text.is_empty() || text.len() > MAX_TEMPLATE_BYTES {
        return Err(format!(
            "{label} must contain 1..={MAX_TEMPLATE_BYTES} bytes"
        ));
    }
    Ok(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARTY: &str = r#"
[messages]
created = "<good>Party created."
invited = { text = "<accent><from></accent> invited you to a party.", args = ["from"] }
tagged  = { text = "<team>[<tag>]</team> <name>", args = ["tag", "name"], styles = ["team"] }
"queue.full" = "Full."

[languages.de-AT]
created = "<good>Party erstellt."

[languages.de]
invited = "<accent><from></accent> hat dich in eine Party eingeladen."
"#;

    #[test]
    fn a_catalogue_parses_with_arguments_styles_and_languages() {
        let messages = Messages::parse(PARTY, "party").unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages.languages().collect::<Vec<_>>(), ["de", "de_at"]);
        let tagged = messages.get("tagged").unwrap();
        assert_eq!(tagged.args, ["tag", "name"]);
        assert_eq!(tagged.styles, ["team"]);
        assert!(tagged.languages.is_empty());
        let created = messages.get("created").unwrap();
        assert_eq!(created.text, "<good>Party created.");
        assert!(created.args.is_empty());
        assert_eq!(created.languages["de_at"], "<good>Party erstellt.");
        assert!(messages.get("queue.full").is_some());
        assert_eq!(
            messages.iter().map(|(key, _)| key).collect::<Vec<_>>(),
            ["created", "invited", "queue.full", "tagged"]
        );
        assert!(Messages::parse("", "party").unwrap().is_empty());
    }

    #[test]
    fn parsing_is_strict() {
        for (text, expected) in [
            ("typo = 1", "unknown key \"typo\""),
            ("[message]\na = \"x\"", "unknown key \"message\""),
            ("messages = 1", "[messages] must be a table"),
            (
                "languages = 1",
                "[languages] must hold a table per language",
            ),
            ("[languages]\nde = 1", "[languages.de] must be a table"),
            (
                "[messages]\na = 1",
                "[messages] \"a\" must be a template or a table",
            ),
            (
                "[messages]\na = \"\"",
                "[messages] \"a\" must contain 1..=4096 bytes",
            ),
            ("[messages]\nA = \"x\"", "[messages] \"A\": a key is"),
            ("[messages]\n\"a:b\" = \"x\"", "a key is 1..=64"),
            (
                "[messages]\na = { args = [] }",
                "[messages] \"a\": text is required",
            ),
            (
                "[messages]\na = { text = \"x\", arg = [] }",
                "[messages] \"a\": unknown key \"arg\"",
            ),
            ("[messages]\na.b = \"x\"", "unknown key \"b\""),
            (
                "[messages]\na = { text = 1 }",
                "[messages] \"a\".text must be a template string",
            ),
            (
                "[messages]\na = { text = \"x\", args = \"b\" }",
                ".args must be a list",
            ),
            (
                "[messages]\na = { text = \"x\", args = [1] }",
                ".args must be a list",
            ),
            (
                "[messages]\na = { text = \"x\", args = [\"B\"] }",
                "must be 1..=64 characters",
            ),
            (
                "[messages]\na = { text = \"x\", styles = [\"bold\"] }",
                "<bold> is a built-in tag",
            ),
            (
                "[messages]\na = { text = \"x\", args = [\"b\", \"b\"] }",
                "\"b\" appears twice",
            ),
            (
                "[messages]\na = { text = \"x\", args = [\"b\"], styles = [\"b\"] }",
                "\"b\" is in both args and styles",
            ),
            (
                "[messages]\na = \"x\"\n[languages.d]\na = \"y\"",
                "[languages.d]: a language is",
            ),
            (
                "[messages]\na = \"x\"\n[languages.de]\nb = \"y\"",
                "[languages.de] \"b\": there is no message \"b\" in [messages]",
            ),
            (
                "[messages]\na = \"x\"\n[languages.de]\na = { text = \"y\" }",
                "[languages.de] \"a\" must be a template string",
            ),
            (
                "[messages]\na = \"x\"\n[languages.de]\na = \"y\"\n[languages.DE]\na = \"z\"",
                "the language de appears twice",
            ),
            ("[messages]\na = \"x\"\na = \"y\"", "duplicate key"),
        ] {
            let error = Messages::parse(text, "party").unwrap_err();
            assert!(error.contains(expected), "{text}: {error}");
        }
        assert!(
            Messages::parse("", "rift")
                .unwrap_err()
                .contains("reserved for Rift's own messages")
        );
    }

    #[test]
    fn limits_are_enforced() {
        let too_large = format!("# {}", "x".repeat(MAX_BYTES));
        assert!(
            Messages::parse(&too_large, "party")
                .unwrap_err()
                .contains("exceeds 256 KiB")
        );
        let many = |count: usize| {
            let mut text = "[messages]\n".to_owned();
            for index in 0..count {
                text.push_str(&format!("m{index} = \"x\"\n"));
            }
            text
        };
        Messages::parse(&many(MAX_MESSAGES), "party").unwrap();
        assert!(
            Messages::parse(&many(MAX_MESSAGES + 1), "party")
                .unwrap_err()
                .contains("at most 256 messages")
        );
        let languages = |count: usize| {
            let mut text = "[messages]\na = \"x\"\n".to_owned();
            for index in 0..count {
                text.push_str(&format!("[languages.l{index}]\na = \"y\"\n"));
            }
            text
        };
        Messages::parse(&languages(MAX_LANGUAGES), "party").unwrap();
        assert!(
            Messages::parse(&languages(MAX_LANGUAGES + 1), "party")
                .unwrap_err()
                .contains("at most 64 languages")
        );
        let key = |length: usize| format!("[messages]\n{} = \"x\"", "k".repeat(length));
        Messages::parse(&key(64), "party").unwrap();
        assert!(Messages::parse(&key(65), "party").is_err());
        let template = |length: usize| format!("[messages]\na = \"{}\"", "x".repeat(length));
        Messages::parse(&template(MAX_TEMPLATE_BYTES), "party").unwrap();
        assert!(
            Messages::parse(&template(MAX_TEMPLATE_BYTES + 1), "party")
                .unwrap_err()
                .contains("1..=4096 bytes")
        );
        let arguments = |args: usize, styles: usize| {
            let list = |prefix: &str, count: usize| {
                (0..count)
                    .map(|index| format!("\"{prefix}{index}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!(
                "[messages]\na = {{ text = \"x\", args = [{}], styles = [{}] }}",
                list("a", args),
                list("s", styles)
            )
        };
        Messages::parse(&arguments(10, 6), "party").unwrap();
        assert!(
            Messages::parse(&arguments(10, 7), "party")
                .unwrap_err()
                .contains("at most 16 arguments and styles")
        );
        assert!(Messages::parse(&arguments(17, 0), "party").is_err());
    }

    #[test]
    fn the_file_is_read_from_the_plugin_folder() {
        let folder = std::env::temp_dir().join(format!("rift-messages-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        assert_eq!(read(&folder, "party"), None);
        std::fs::write(folder.join(FILE), PARTY).unwrap();
        assert_eq!(read(&folder, "party").unwrap().unwrap().len(), 4);
        std::fs::write(folder.join(FILE), "typo = 1").unwrap();
        let error = read(&folder, "party").unwrap().unwrap_err();
        assert!(
            error.contains("messages.toml: unknown key \"typo\""),
            "{error}"
        );
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
