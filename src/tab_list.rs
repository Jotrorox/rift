//! `rift.config.tab_list`: a proxy-owned tab list header and footer, and
//! optionally every player on the network in each player's tab list.
//!
//! Header and footer text may use `{player}`, `{server}`, `{online}` and
//! `{server_online}`. Sessions rebuild their view only when the registry,
//! configuration, per-player override or client tab list has changed.
//! Entries and counts are what the viewing player may see (see
//! [`Viewer`]), so a hidden player is neither listed nor counted.

use crate::{
    players::{PlayerRegistry, Viewer},
    protocol::TabView,
};
use serde_json::Value as Json;

/// Rendered text that would exceed this many bytes keeps its placeholders.
const MAX_RENDERED_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TabList {
    /// A plain string or a chat component.
    pub header: Option<Json>,
    pub footer: Option<Json>,
    /// List players on other servers as well as the backend's own entries.
    pub network: bool,
}

/// A header and footer for one player, set through `rift.players.tab_list`.
/// Unset fields use the configured text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Override {
    pub header: Option<Json>,
    pub footer: Option<Json>,
}

/// The tab list one player should see on `server`.
pub fn view(
    registry: &PlayerRegistry,
    viewer: Viewer<'_>,
    config: Option<&TabList>,
    custom: Option<&Override>,
    player: &str,
    server: &str,
) -> TabView {
    let pick = |custom: Option<&Json>, configured: Option<&Json>| custom.or(configured).cloned();
    let header = pick(
        custom.and_then(|text| text.header.as_ref()),
        config.and_then(|config| config.header.as_ref()),
    );
    let footer = pick(
        custom.and_then(|text| text.footer.as_ref()),
        config.and_then(|config| config.footer.as_ref()),
    );
    let header = (header.is_some() || footer.is_some()).then(|| {
        let online = registry.count_for(viewer, None).to_string();
        let server_online = registry.count_for(viewer, Some(server)).to_string();
        let values = [
            ("{player}", player),
            ("{server}", server),
            ("{online}", online.as_str()),
            ("{server_online}", server_online.as_str()),
        ];
        let blank = || Json::String(String::new());
        (
            header.map_or_else(blank, |text| render(text, &values)),
            footer.map_or_else(blank, |text| render(text, &values)),
        )
    });
    TabView {
        header,
        players: config
            .is_some_and(|config| config.network)
            .then(|| registry.tab_entries(server, viewer)),
    }
}

/// Replace placeholders in every string value, leaving object keys alone.
fn render(text: Json, values: &[(&str, &str)]) -> Json {
    fn walk(text: &mut Json, values: &[(&str, &str)]) {
        match text {
            Json::String(value) if value.contains('{') => {
                for (name, replacement) in values {
                    if value.contains(name) {
                        *value = value.replace(name, replacement);
                    }
                }
            }
            Json::Array(items) => items.iter_mut().for_each(|item| walk(item, values)),
            Json::Object(fields) => fields.values_mut().for_each(|item| walk(item, values)),
            _ => {}
        }
    }
    let mut rendered = text.clone();
    walk(&mut rendered, values);
    if rendered.to_string().len() > MAX_RENDERED_BYTES {
        return text;
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::TabEntry;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn placeholders_render_in_strings_and_components() {
        let registry = Arc::new(PlayerRegistry::default());
        let _alice = registry.register([1; 16], "Alice", "lobby").unwrap();
        let _bob = registry.register([2; 16], "Bob", "survival").unwrap();
        let config = TabList {
            header: Some(json!("Hi {player} on {server}")),
            footer: Some(json!({
                "text": "{online} online",
                "extra": [{"text": "{server_online} here", "color": "gray"}],
                "{player}": "{unknown}",
            })),
            network: false,
        };
        let view = view(
            &registry,
            Viewer::System,
            Some(&config),
            None,
            "Alice",
            "lobby",
        );
        assert_eq!(
            view.header,
            Some((
                json!("Hi Alice on lobby"),
                json!({
                    "text": "2 online",
                    "extra": [{"text": "1 here", "color": "gray"}],
                    "{player}": "{unknown}",
                })
            ))
        );
        assert_eq!(view.players, None);
    }

    #[test]
    fn overrides_replace_single_fields_and_missing_text_is_blank() {
        let registry = PlayerRegistry::default();
        let config = TabList {
            header: Some(json!("network")),
            ..TabList::default()
        };
        let custom = Override {
            footer: Some(json!("mine")),
            ..Override::default()
        };
        assert_eq!(
            view(
                &registry,
                Viewer::System,
                Some(&config),
                None,
                "Alice",
                "lobby"
            )
            .header,
            Some((json!("network"), json!("")))
        );
        assert_eq!(
            view(
                &registry,
                Viewer::System,
                Some(&config),
                Some(&custom),
                "Alice",
                "lobby"
            )
            .header,
            Some((json!("network"), json!("mine")))
        );
        assert_eq!(
            view(
                &registry,
                Viewer::System,
                None,
                Some(&custom),
                "Alice",
                "lobby"
            )
            .header,
            Some((json!(""), json!("mine")))
        );
        assert_eq!(
            view(&registry, Viewer::System, None, None, "Alice", "lobby"),
            TabView::default()
        );
    }

    #[test]
    fn network_lists_players_on_other_servers() {
        let registry = Arc::new(PlayerRegistry::default());
        let _alice = registry.register([1; 16], "Alice", "lobby").unwrap();
        let bob = registry.register([2; 16], "Bob", "survival").unwrap();
        let skin = crate::auth::ProfileProperty {
            name: "textures".into(),
            value: "skin".into(),
            signature: Some("signed".into()),
        };
        let before = registry.generation();
        bob.set_properties(std::slice::from_ref(&skin));
        assert_ne!(registry.generation(), before);
        let config = TabList {
            network: true,
            ..TabList::default()
        };
        let view = view(
            &registry,
            Viewer::System,
            Some(&config),
            None,
            "Alice",
            "lobby",
        );
        assert_eq!(view.header, None);
        assert_eq!(
            view.players,
            Some(vec![TabEntry {
                uuid: [2; 16],
                name: "Bob".into(),
                properties: Arc::new([skin]),
            }])
        );
    }

    #[test]
    fn hidden_players_are_neither_listed_nor_counted_for_viewers_without_the_node() {
        let registry = Arc::new(PlayerRegistry::default());
        let _alice = registry.register([1; 16], "Alice", "lobby-2").unwrap();
        let _staff = registry
            .register_visible([2; 16], "Staff", "lobby-1", Some("vanish.see".into()))
            .unwrap();
        let _bob = registry.register([3; 16], "Bob", "lobby-1").unwrap();
        let config = TabList {
            header: Some(json!("{online} online, {server_online} here")),
            network: true,
            ..TabList::default()
        };
        let names = |view: TabView| -> Vec<String> {
            view.players
                .unwrap()
                .into_iter()
                .map(|entry| entry.name)
                .collect()
        };
        let none = |_: &str| false;
        let alice = Viewer::Player {
            uuid: [1; 16],
            holds: &none,
        };
        let plain = view(&registry, alice, Some(&config), None, "Alice", "lobby-2");
        assert_eq!(plain.header.as_ref().unwrap().0, json!("2 online, 1 here"));
        assert_eq!(names(plain), ["Bob"]);
        let see = |node: &str| node == "vanish.see";
        let moderator = Viewer::Player {
            uuid: [1; 16],
            holds: &see,
        };
        let seen = view(
            &registry,
            moderator,
            Some(&config),
            None,
            "Alice",
            "lobby-2",
        );
        assert_eq!(seen.header.as_ref().unwrap().0, json!("3 online, 1 here"));
        assert_eq!(names(seen), ["Staff", "Bob"]);
        // Players always count themselves.
        let staff = Viewer::Player {
            uuid: [2; 16],
            holds: &none,
        };
        let own = view(&registry, staff, Some(&config), None, "Staff", "lobby-1");
        assert_eq!(own.header.unwrap().0, json!("3 online, 2 here"));
        // Every viewer kind, and the system view the capacity checks use.
        assert_eq!(registry.count_for(Viewer::Public, None), 2);
        assert_eq!(registry.count_for(Viewer::Backend, Some("lobby-1")), 1);
        assert_eq!(registry.count_for(Viewer::System, None), 3);
        assert_eq!(registry.count_on_server("lobby-1"), 2);
        assert_eq!(registry.server_counts()["lobby-1"], 2);
        assert_eq!(registry.server_counts_for(Viewer::Public)["lobby-1"], 1);
        // Revealing the player rebuilds every view.
        let before = registry.generation();
        assert!(registry.set_visibility(&[2; 16], None));
        assert!(!registry.set_visibility(&[2; 16], None));
        assert_ne!(registry.generation(), before);
        assert_eq!(registry.count_for(Viewer::Public, None), 3);
    }

    #[test]
    fn oversized_renders_keep_their_placeholders() {
        let template = json!("{server}".repeat(2000));
        let long = "s".repeat(64);
        assert_eq!(render(template.clone(), &[("{server}", &long)]), template);
    }
}
