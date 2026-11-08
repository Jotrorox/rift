//! The read-only status service: what `/status` reveals, how the bundled page
//! looks, and which other sites may read the JSON.
use super::*;

/// Optional read-only status service, independent of the administration service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusConfig {
    pub listen: SocketAddr,
    pub ui: bool,
    pub metrics: bool,
    pub exposure: StatusExposure,
    pub page: StatusPage,
    /// Display names and hidden entries, keyed by backend or service group.
    pub backends: BTreeMap<String, StatusBackend>,
    /// Origins allowed to read `/status` from a browser; `["*"]` allows any.
    pub cors: Vec<String>,
}

impl Default for StatusConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:9090".parse().unwrap(),
            ui: true,
            metrics: true,
            exposure: StatusExposure::Operator,
            page: StatusPage::default(),
            backends: BTreeMap::new(),
            cors: Vec::new(),
        }
    }
}

impl StatusConfig {
    /// The configured entry for a backend: its own, else its service group's.
    pub fn backend(&self, name: &str, group: Option<&str>) -> Option<&StatusBackend> {
        self.backends
            .get(name)
            .or_else(|| group.and_then(|group| self.backends.get(group)))
    }

    pub fn allows_origin(&self, origin: &str) -> bool {
        self.cors
            .iter()
            .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(origin))
    }
}

/// How much `/status` reveals to anyone who can reach the status server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusExposure {
    /// Addresses, listeners, counters and service settings, for monitoring networks.
    #[default]
    Operator,
    /// Names, health and player counts only, for a page linked to players.
    Public,
}

impl StatusExposure {
    pub fn name(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Public => "public",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusBackend {
    Hidden,
    Display(String),
}

/// Presentation of the bundled status page, delivered to it through `/status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusPage {
    pub title: Option<String>,
    pub notice: Option<String>,
    pub accent: Option<String>,
    pub refresh: Duration,
    pub panels: StatusPanels,
    pub links: Vec<StatusLink>,
}

impl Default for StatusPage {
    fn default() -> Self {
        Self {
            title: None,
            notice: None,
            accent: None,
            refresh: Duration::from_secs(5),
            panels: StatusPanels::default(),
            links: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusPanels {
    pub summary: bool,
    pub backends: bool,
    pub listeners: bool,
    pub metrics: bool,
}

impl Default for StatusPanels {
    fn default() -> Self {
        Self {
            summary: true,
            backends: true,
            listeners: true,
            metrics: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLink {
    pub label: String,
    pub href: String,
}

pub const MAX_LINKS: usize = 8;
pub const MIN_REFRESH_MS: usize = 1_000;
pub const MAX_REFRESH_MS: usize = 300_000;
const MAX_ORIGINS: usize = 32;

pub(super) fn parse(root: &Table) -> Result<Option<StatusConfig>, String> {
    let Some(table) = service_options(
        root,
        "status",
        &[
            "enabled", "listen", "ui", "metrics", "exposure", "backends", "cors",
        ],
    )?
    else {
        return Ok(None);
    };
    let enabled = boolean(&table, "status", "enabled", true)?;
    let exposure = match table
        .raw_get::<Value>("exposure")
        .map_err(|e| e.to_string())?
    {
        Value::Nil => StatusExposure::Operator,
        value => match string(value, "status.exposure")?.as_str() {
            "operator" => StatusExposure::Operator,
            "public" => StatusExposure::Public,
            _ => return Err("status.exposure: expected \"operator\" or \"public\"".into()),
        },
    };
    let (ui, page) = page(table.raw_get::<Value>("ui").map_err(|e| e.to_string())?)?;
    let config = StatusConfig {
        listen: service_listen(&table, "status", StatusConfig::default().listen)?,
        ui,
        // Prometheus text carries every counter, so a public page opts in to it.
        metrics: boolean(
            &table,
            "status",
            "metrics",
            exposure == StatusExposure::Operator,
        )?,
        exposure,
        page,
        backends: backends(
            table
                .raw_get::<Value>("backends")
                .map_err(|e| e.to_string())?,
        )?,
        cors: cors(table.raw_get::<Value>("cors").map_err(|e| e.to_string())?)?,
    };
    Ok(enabled.then_some(config))
}

/// `ui` is `true`/`false`, or a table that enables the page and styles it.
fn page(value: Value) -> Result<(bool, StatusPage), String> {
    let table = match value {
        Value::Nil => return Ok((true, StatusPage::default())),
        Value::Boolean(enabled) => return Ok((enabled, StatusPage::default())),
        Value::Table(table) => table,
        _ => return Err("status.ui: expected a boolean or a table".into()),
    };
    fields(
        &table,
        &[
            "enabled",
            "title",
            "notice",
            "accent",
            "refresh_ms",
            "panels",
            "links",
        ],
        "status.ui",
    )?;
    let enabled = boolean(&table, "status.ui", "enabled", true)?;
    let optional_text = |key: &str, maximum: usize| -> Result<Option<String>, String> {
        match table.raw_get::<Value>(key).map_err(|e| e.to_string())? {
            Value::Nil => Ok(None),
            value => text(value, &format!("status.ui.{key}"), maximum).map(Some),
        }
    };
    let title = optional_text("title", 80)?;
    let notice = optional_text("notice", 500)?;
    let accent = match table
        .raw_get::<Value>("accent")
        .map_err(|e| e.to_string())?
    {
        Value::Nil => None,
        value => {
            let accent = string(value, "status.ui.accent")?;
            let digits = accent.strip_prefix('#').unwrap_or("");
            if !matches!(digits.len(), 3 | 6) || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("status.ui.accent: expected a hex color such as \"#ff6a00\"".into());
            }
            // Six digits, so the page can derive translucent variants.
            let digits = if digits.len() == 3 {
                digits.chars().flat_map(|c| [c, c]).collect()
            } else {
                digits.to_owned()
            };
            Some(format!("#{}", digits.to_ascii_lowercase()))
        }
    };
    let refresh = integer(&table, "status.ui", "refresh_ms", 5_000, MAX_REFRESH_MS)?;
    if refresh < MIN_REFRESH_MS {
        return Err(format!(
            "status.ui.refresh_ms: expected an integer in {MIN_REFRESH_MS}..={MAX_REFRESH_MS}"
        ));
    }
    let panels = match table
        .raw_get::<Value>("panels")
        .map_err(|e| e.to_string())?
    {
        Value::Nil => StatusPanels::default(),
        value => {
            let panels = super::table(value, "status.ui.panels")?;
            let path = "status.ui.panels";
            fields(
                &panels,
                &["summary", "backends", "listeners", "metrics"],
                path,
            )?;
            StatusPanels {
                summary: boolean(&panels, path, "summary", true)?,
                backends: boolean(&panels, path, "backends", true)?,
                listeners: boolean(&panels, path, "listeners", true)?,
                metrics: boolean(&panels, path, "metrics", true)?,
            }
        }
    };
    let links = match table.raw_get::<Value>("links").map_err(|e| e.to_string())? {
        Value::Nil => Vec::new(),
        value => links(value)?,
    };
    Ok((
        enabled,
        StatusPage {
            title,
            notice,
            accent,
            refresh: Duration::from_millis(refresh as u64),
            panels,
            links,
        },
    ))
}

fn links(value: Value) -> Result<Vec<StatusLink>, String> {
    let mut ordered = BTreeMap::new();
    for pair in super::table(value, "status.ui.links")?.pairs::<Value, Value>() {
        let (index, value) = pair.map_err(|e| e.to_string())?;
        let Value::Integer(index) = index else {
            return Err("status.ui.links: expected a dense array".into());
        };
        let path = format!("status.ui.links[{index}]");
        let link = super::table(value, &path)?;
        fields(&link, &["label", "href"], &path)?;
        let label = text(
            link.raw_get("label").map_err(|e| e.to_string())?,
            &format!("{path}.label"),
            40,
        )?;
        let href = string(
            link.raw_get("href").map_err(|e| e.to_string())?,
            &format!("{path}.href"),
        )?;
        if !link_target(&href) {
            return Err(format!(
                "{path}.href: expected an http(s) URL or a path starting with \"/\""
            ));
        }
        ordered.insert(index, StatusLink { label, href });
    }
    if ordered.len() > MAX_LINKS || ordered.keys().copied().ne(1..=ordered.len() as i64) {
        return Err(format!(
            "status.ui.links: expected a dense array of at most {MAX_LINKS} links"
        ));
    }
    Ok(ordered.into_values().collect())
}

/// Only navigations: no `javascript:`/`data:` URLs, and no protocol-relative paths.
fn link_target(href: &str) -> bool {
    let rest = if let Some(rest) = href
        .strip_prefix("https://")
        .or_else(|| href.strip_prefix("http://"))
    {
        if rest.is_empty() || rest.starts_with('/') {
            return false;
        }
        rest
    } else if href.starts_with('/') && !href.starts_with("//") && !href.starts_with("/\\") {
        href
    } else {
        return false;
    };
    href.len() <= 512 && !rest.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn backends(value: Value) -> Result<BTreeMap<String, StatusBackend>, String> {
    let mut result = BTreeMap::new();
    if matches!(value, Value::Nil) {
        return Ok(result);
    }
    for pair in super::table(value, "status.backends")?.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|e| e.to_string())?;
        let name = string(key, "status.backends key")?;
        let path = format!("status.backends.{name}");
        let entry = match value {
            Value::Boolean(false) => StatusBackend::Hidden,
            Value::String(_) => StatusBackend::Display(text(value, &path, 64)?),
            _ => return Err(format!("{path}: expected a display name or false")),
        };
        result.insert(name, entry);
    }
    Ok(result)
}

fn cors(value: Value) -> Result<Vec<String>, String> {
    let origins = match value {
        Value::Nil => return Ok(Vec::new()),
        Value::String(_) => vec![string(value, "status.cors")?],
        value => string_list(value, "status.cors", MAX_ORIGINS)?,
    };
    let mut result = Vec::with_capacity(origins.len());
    for origin in origins {
        if origin == "*" || result.iter().any(|value| value == "*") {
            if !result.is_empty() {
                return Err("status.cors: \"*\" must be the only origin".into());
            }
            result.push(origin);
            continue;
        }
        let normalized = origin.to_ascii_lowercase();
        if !valid_origin(&normalized) {
            return Err(format!(
                "status.cors: invalid origin {origin:?} (expected scheme://host[:port] without a path)"
            ));
        }
        if !result.contains(&normalized) {
            result.push(normalized);
        }
    }
    Ok(result)
}

/// A serialized browser origin, as sent in the `Origin` request header.
fn valid_origin(origin: &str) -> bool {
    let Some(authority) = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
    else {
        return false;
    };
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let Some((address, rest)) = rest.split_once(']') else {
            return false;
        };
        if address.parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        match rest {
            "" => (address, None),
            rest => match rest.strip_prefix(':') {
                Some(port) => (address, Some(port)),
                None => return false,
            },
        }
    } else {
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        let labels_valid = !host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            });
        if !labels_valid {
            return false;
        }
        (host, port)
    };
    !host.is_empty()
        && port.is_none_or(|port| {
            !port.is_empty()
                && port.len() <= 5
                && port.bytes().all(|b| b.is_ascii_digit())
                && port.parse::<u16>().is_ok_and(|port| port != 0)
        })
}

/// Human-facing text: trimmed, bounded and free of control characters.
fn text(value: Value, path: &str, maximum: usize) -> Result<String, String> {
    let value = string(value, path)?;
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.chars().count() > maximum
        || trimmed.chars().any(char::is_control)
    {
        return Err(format!(
            "{path}: expected 1..={maximum} characters without control characters"
        ));
    }
    Ok(trimmed.to_owned())
}

impl Config {
    /// Entries in `status.backends` name a backend or a service group. Runtime
    /// instances come and go, so they are configured through their group.
    pub(super) fn validate_status(&self) -> Result<(), String> {
        let Some(status) = &self.status else {
            return Ok(());
        };
        for name in status.backends.keys() {
            if let Some(instance) = self.instances.get(name) {
                return Err(format!(
                    "status.backends.{name}: instances are configured through their service group {:?}",
                    instance.group
                ));
            }
            if !self.backends.contains_key(name) && !self.service_groups.contains_key(name) {
                return Err(format!(
                    "status.backends.{name}: unknown backend or service group"
                ));
            }
        }
        Ok(())
    }
}
