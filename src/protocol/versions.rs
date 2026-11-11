//! Which players' Minecraft versions a backend takes through translation.
//!
//! A backend always takes players on the version it runs. `ClientVersions`
//! says who else may join it translated: everyone, nobody, newer or older
//! clients only, or the versions a list of patterns names.
use super::{ProtocolVersion, packets::RELEASES};

/// The oldest and newest protocol translation reaches (1.12 and 26.3).
fn translated() -> (i32, i32) {
    let protocols = rift_translation::version::PROTOCOLS;
    (protocols[0].0, protocols[protocols.len() - 1].0)
}

/// Every release translation reaches, oldest first, with its protocol.
pub fn translated_releases() -> impl Iterator<Item = (&'static str, i32)> {
    let (oldest, newest) = translated();
    RELEASES
        .iter()
        .copied()
        .filter(move |(_, protocol)| (oldest..=newest).contains(protocol))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Rule {
    /// Every version translation reaches.
    All,
    /// Clients newer than the backend, as ViaVersion serves.
    Newer,
    /// Clients older than the backend, as ViaBackwards serves.
    Older,
    /// Clients whose protocol lies in `low..=high`, as written in `text`.
    Span { low: i32, high: i32, text: String },
}

/// The client versions one backend, or the whole proxy, translates for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientVersions(Vec<Rule>);

impl Default for ClientVersions {
    fn default() -> Self {
        Self::all()
    }
}

/// The protocols a release name or a wildcard such as `1.20.*` covers.
fn named(text: &str) -> Result<(i32, i32), String> {
    let family = text.strip_suffix(".*").or_else(|| text.strip_suffix(".x"));
    let matches = |release: &str| match family {
        Some(family) => {
            release == family
                || release
                    .strip_prefix(family)
                    .is_some_and(|rest| rest.starts_with('.'))
        }
        None => release == text,
    };
    let mut protocols = RELEASES
        .iter()
        .filter(|(release, _)| matches(release))
        .map(|(_, protocol)| *protocol);
    let Some(first) = protocols.next() else {
        return Err(format!(
            "{text:?} names no Minecraft version Rift knows (examples: \"1.20.4\", \"1.20.*\", \"1.16-1.18.2\", \">=1.19\")"
        ));
    };
    Ok(protocols.fold((first, first), |(low, high), protocol| {
        (low.min(protocol), high.max(protocol))
    }))
}

fn rule(text: &str) -> Result<Rule, String> {
    let text = text.trim().to_ascii_lowercase();
    let (oldest, newest) = translated();
    let (low, high) = match text.as_str() {
        "" => return Err("a version pattern must not be empty".into()),
        "all" | "*" => return Ok(Rule::All),
        "newer" => return Ok(Rule::Newer),
        "older" => return Ok(Rule::Older),
        "none" => {
            return Err("\"none\" stands alone: use translation = \"none\" or false".into());
        }
        _ => {
            if let Some(from) = text.strip_prefix(">=") {
                (named(from.trim())?.0, newest)
            } else if let Some(from) = text.strip_suffix('+') {
                (named(from.trim())?.0, newest)
            } else if let Some(to) = text.strip_prefix("<=") {
                (oldest, named(to.trim())?.1)
            } else if let Some((from, to)) = text.split_once('-') {
                (named(from.trim())?.0, named(to.trim())?.1)
            } else {
                named(&text)?
            }
        }
    };
    if low > high {
        return Err(format!(
            "{text:?} is an empty range: the first version is the newer one"
        ));
    }
    if high < oldest || low > newest {
        return Err(format!(
            "{text:?} lies outside the versions Rift translates (1.12 to 26.3)"
        ));
    }
    Ok(Rule::Span { low, high, text })
}

impl ClientVersions {
    /// Every version translation reaches: the default.
    pub fn all() -> Self {
        Self(vec![Rule::All])
    }

    /// No translation: only players on the backend's own version.
    pub fn none() -> Self {
        Self(Vec::new())
    }

    /// Patterns as written in configuration: `all`, `newer`, `older`, a
    /// release (`1.20.4`), a family (`1.20.*`), a range (`1.16-1.18.2`) or an
    /// open range (`>=1.19`, `1.19+`, `<=1.16.5`). A single `none` is no
    /// translation.
    pub fn parse<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Result<Self, String> {
        let patterns: Vec<_> = patterns.into_iter().collect();
        if let [single] = patterns.as_slice()
            && single.trim().eq_ignore_ascii_case("none")
        {
            return Ok(Self::none());
        }
        if patterns.is_empty() {
            return Err("expected at least one version pattern, or false for none".into());
        }
        let mut rules = Vec::new();
        for pattern in patterns {
            let rule = rule(pattern)?;
            if !rules.contains(&rule) {
                rules.push(rule);
            }
        }
        if rules.contains(&Rule::All) {
            return Ok(Self::all());
        }
        Ok(Self(rules))
    }

    /// Whether a player on `client` may join a backend on another version,
    /// `backend`, through translation.
    pub fn allows(&self, client: ProtocolVersion, backend: ProtocolVersion) -> bool {
        let (client, backend) = (client.number(), backend.number());
        self.0.iter().any(|rule| match rule {
            Rule::All => true,
            Rule::Newer => client > backend,
            Rule::Older => client < backend,
            Rule::Span { low, high, .. } => (*low..=*high).contains(&client),
        })
    }

    pub fn is_all(&self) -> bool {
        self.0 == [Rule::All]
    }

    pub fn is_none(&self) -> bool {
        self.0.is_empty()
    }

    /// The patterns as configuration writes them; empty for none.
    pub fn patterns(&self) -> Vec<&str> {
        self.0
            .iter()
            .map(|rule| match rule {
                Rule::All => "all",
                Rule::Newer => "newer",
                Rule::Older => "older",
                Rule::Span { text, .. } => text,
            })
            .collect()
    }

    /// The value the web interface and Lua share: `"all"`, `"none"`,
    /// `"newer"`, `"older"`, or a list of patterns.
    pub fn to_json(&self) -> serde_json::Value {
        match self.patterns().as_slice() {
            [] => "none".into(),
            [level @ ("all" | "newer" | "older")] => (*level).into(),
            patterns => patterns.to_vec().into(),
        }
    }

    /// One line for people: what a backend with this setting accepts.
    pub fn summary(&self) -> String {
        match self.patterns().as_slice() {
            [] => "Own version only".into(),
            ["all"] => "All versions".into(),
            ["newer"] => "Own version and newer".into(),
            ["older"] => "Own version and older".into(),
            patterns => patterns
                .iter()
                .map(|pattern| match *pattern {
                    "newer" => "newer",
                    "older" => "older",
                    pattern => pattern,
                })
                .collect::<Vec<_>>()
                .join(", "),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(release: &str) -> ProtocolVersion {
        ProtocolVersion::from_release(release).unwrap()
    }

    fn allows(patterns: &[&str], client: &str, backend: &str) -> bool {
        ClientVersions::parse(patterns.iter().copied())
            .unwrap()
            .allows(version(client), version(backend))
    }

    #[test]
    fn levels_admit_everyone_nobody_or_one_direction() {
        assert!(ClientVersions::default().is_all());
        assert!(allows(&["all"], "1.12.2", "26.3"));
        assert!(allows(&["*"], "26.3", "1.12.2"));
        assert!(!allows(&["none"], "1.20.4", "1.20.2"));
        assert!(ClientVersions::parse(["none"]).unwrap().is_none());
        assert!(allows(&["newer"], "1.21.4", "1.20.1"));
        assert!(!allows(&["newer"], "1.19.4", "1.20.1"));
        assert!(allows(&["older"], "1.19.4", "1.20.1"));
        assert!(!allows(&["older"], "1.21.4", "1.20.1"));
    }

    #[test]
    fn patterns_name_releases_families_and_ranges() {
        // An exact release covers every release that shares its protocol.
        assert!(allows(&["1.20.1"], "1.20", "1.12.2"));
        assert!(!allows(&["1.20.1"], "1.20.2", "1.12.2"));
        for family in ["1.20.*", "1.20.x", " 1.20.X "] {
            assert!(allows(&[family], "1.20", "26.3"));
            assert!(allows(&[family], "1.20.6", "26.3"));
            assert!(!allows(&[family], "1.21", "26.3"));
            assert!(!allows(&[family], "1.19.4", "26.3"));
        }
        // 1.2 is no prefix of 1.20 or 1.21.
        assert!(ClientVersions::parse(["1.2.*"]).is_err());
        assert!(allows(&["26.*"], "26.2", "1.21.4"));
        assert!(allows(&["1.16-1.18.2"], "1.17.1", "26.3"));
        assert!(!allows(&["1.16-1.18.2"], "1.19", "26.3"));
        assert!(allows(&["1.16.*-1.18.*"], "1.18.2", "26.3"));
        assert!(allows(&[">=1.19"], "26.3", "1.12.2"));
        assert!(allows(&["1.19+"], "1.19", "1.12.2"));
        assert!(!allows(&[">=1.19"], "1.18.2", "1.12.2"));
        assert!(allows(&["<=1.16.5"], "1.12", "26.3"));
        assert!(!allows(&["<=1.16.5"], "1.17", "26.3"));
        // Several patterns add up, levels included.
        assert!(allows(&["1.12.*", "newer"], "1.12.2", "1.20.1"));
        assert!(allows(&["1.12.*", "newer"], "1.21", "1.20.1"));
        assert!(!allows(&["1.12.*", "newer"], "1.16.5", "1.20.1"));
    }

    #[test]
    fn mistakes_are_named() {
        for (patterns, expected) in [
            (vec![], "at least one"),
            (vec![""], "must not be empty"),
            (vec!["1.99"], "names no Minecraft version"),
            (vec!["1.20.4-1.16"], "empty range"),
            (vec!["1.8.*"], "outside the versions Rift translates"),
            (vec!["none", "1.20.*"], "stands alone"),
            (vec!["latest"], "names no Minecraft version"),
        ] {
            let error = ClientVersions::parse(patterns.iter().copied()).unwrap_err();
            assert!(error.contains(expected), "{patterns:?}: {error}");
        }
    }

    #[test]
    fn values_round_trip_through_configuration() {
        for (patterns, json, summary) in [
            (vec!["all"], serde_json::json!("all"), "All versions"),
            (
                vec!["1.20.*", "ALL"],
                serde_json::json!("all"),
                "All versions",
            ),
            (vec!["none"], serde_json::json!("none"), "Own version only"),
            (
                vec!["newer"],
                serde_json::json!("newer"),
                "Own version and newer",
            ),
            (
                vec!["older"],
                serde_json::json!("older"),
                "Own version and older",
            ),
            (
                vec!["1.20.X", "1.21.4", "1.20.x"],
                serde_json::json!(["1.20.x", "1.21.4"]),
                "1.20.x, 1.21.4",
            ),
        ] {
            let versions = ClientVersions::parse(patterns.iter().copied()).unwrap();
            assert_eq!(versions.to_json(), json);
            assert_eq!(versions.summary(), summary);
            assert_eq!(
                ClientVersions::parse(versions.patterns()).unwrap_or(ClientVersions::none()),
                versions
            );
        }
        assert_eq!(translated_releases().next(), Some(("1.12", 335)));
        assert_eq!(translated_releases().last(), Some(("26.3", 777)));
    }
}
