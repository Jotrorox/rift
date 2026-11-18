use super::*;
use serde_json::json;

fn switchable() -> impl Iterator<Item = ProtocolVersion> {
    (0..=777)
        .filter_map(|number| ProtocolVersion::new(number).ok())
        .filter(|version| version.supports_switching())
}

fn entry(uuid: u8, name: &str) -> TabEntry {
    TabEntry {
        uuid: [uuid; 16],
        name: name.into(),
        properties: Arc::new([]),
        display: None,
        order: 0,
    }
}

fn ranked(uuid: u8, name: &str, display: Json, order: i32) -> TabEntry {
    TabEntry {
        display: Some(Arc::new(display)),
        order,
        ..entry(uuid, name)
    }
}

/// A backend Player Info packet adding `uuids` with every field the version has.
fn backend_add(version: ProtocolVersion, uuids: &[u8]) -> Packet {
    let number = version.number();
    let ids = ids(version).unwrap();
    let mut data = Vec::new();
    if number >= 761 {
        data.push(match number {
            ..768 => 63,
            768 => 127,
            _ => 255,
        });
    } else {
        write_varint(0, &mut data);
    }
    write_varint(uuids.len() as i32, &mut data);
    for &uuid in uuids {
        data.extend([uuid; 16]);
        write_string("Backend", &mut data);
        write_varint(1, &mut data);
        write_string("textures", &mut data);
        write_string("value", &mut data);
        data.push(1);
        write_string("signature", &mut data);
        if number >= 761 {
            data.push(1); // Chat session.
            data.extend([9; 24]);
            for _ in 0..2 {
                write_varint(3, &mut data);
                data.extend([7; 3]);
            }
            data.extend([1, 1, 40]); // Game mode, listed, latency.
        } else {
            data.extend([1, 40]);
        }
        data.push(1); // Display name.
        if version.nbt_components() {
            crate::protocol::nbt::component(&json!({"text": "Shown", "bold": true}), &mut data)
                .unwrap();
        } else {
            write_string("{\"text\":\"Shown\"}", &mut data);
        }
        if number >= 768 {
            write_varint(5, &mut data); // List order.
        }
        if number >= 769 {
            data.push(1); // Hat.
        }
        if (759..761).contains(&number) {
            data.push(0); // No profile key.
        }
    }
    Packet::new(ids.players, data)
}

fn backend_remove(version: ProtocolVersion, uuids: &[u8]) -> Packet {
    removal(
        version,
        uuids
            .iter()
            .map(|uuid| [*uuid; 16])
            .collect::<Vec<_>>()
            .iter(),
    )
    .unwrap()
    .remove(0)
}

#[test]
fn every_switchable_version_has_tab_list_ids() {
    for version in switchable() {
        let ids = ids(version).unwrap();
        assert_eq!(ids.remove_players >= 0, version.number() >= 761);
        if let Some(legacy) = super::super::legacy::ids(version.number()) {
            assert_eq!(
                (ids.players, ids.remove_players, ids.header),
                (legacy.players, legacy.remove_players, legacy.header)
            );
        }
    }
    let modern = |number| ids(ProtocolVersion::new(number).unwrap()).unwrap();
    assert_eq!(
        modern(764),
        TabIds {
            players: 0x3c,
            remove_players: 0x3b,
            header: 0x68
        }
    );
    assert_eq!(
        modern(777),
        TabIds {
            players: 0x47,
            remove_players: 0x46,
            header: 0x7d
        }
    );
}

#[test]
fn backend_entries_are_tracked_for_every_action_layout() {
    for version in switchable() {
        let mut tabs = TabList::default();
        assert!(
            tabs.observe(version, &backend_add(version, &[1, 2]))
                .unwrap()
        );
        assert!(
            tabs.observe(version, &backend_remove(version, &[1]))
                .unwrap()
        );
        assert_eq!(
            tabs.backend,
            BTreeSet::from([[2; 16]]),
            "protocol {}",
            version.number()
        );
        let mut truncated = backend_add(version, &[3]);
        truncated.data.pop();
        assert!(tabs.observe(version, &truncated).is_err());
    }
}

#[test]
fn unknown_player_info_actions_are_rejected() {
    for (number, action) in [(761, 64), (767, 64), (768, 128)] {
        let version = ProtocolVersion::new(number).unwrap();
        let packet = Packet::new(ids(version).unwrap().players, vec![action, 0]);
        assert!(TabList::default().observe(version, &packet).is_err());
    }
}

#[test]
fn rift_lists_only_players_the_backend_does_not() {
    for version in switchable() {
        let ids = ids(version).unwrap();
        let mut tabs = TabList::default();
        tabs.observe(version, &backend_add(version, &[2])).unwrap();
        let view = TabView {
            header: None,
            players: Some(vec![entry(1, "Own"), entry(2, "Local"), entry(3, "Remote")]),
            names: None,
        };
        let packets = tabs.sync(version, Some([1; 16]), &view).unwrap();
        assert_eq!(
            packets,
            [addition(version, ids, &[&entry(3, "Remote")]).unwrap()]
        );
        let mut expected = if version.number() >= 761 {
            vec![9, 1]
        } else {
            vec![0, 1]
        };
        expected.extend([3; 16]);
        write_string("Remote", &mut expected);
        expected.push(0);
        if version.number() >= 761 {
            expected.push(1);
        } else {
            expected.extend([0, 0, 0]);
            if version.number() >= 759 {
                expected.push(0);
            }
        }
        assert_eq!(packets[0].data, expected, "protocol {}", version.number());
        // Unchanged views send nothing; a departed player is removed.
        assert!(tabs.sync(version, Some([1; 16]), &view).unwrap().is_empty());
        let packets = tabs
            .sync(version, Some([1; 16]), &TabView::default())
            .unwrap();
        assert_eq!(packets, [backend_remove(version, &[3])]);
        assert!(!tabs.modified());
    }
}

#[test]
fn backend_entries_displace_rift_entries_without_removal() {
    let version = ProtocolVersion::new(774).unwrap();
    let mut tabs = TabList::default();
    let view = TabView {
        header: None,
        players: Some(vec![entry(3, "Mover")]),
        names: None,
    };
    tabs.sync(version, None, &view).unwrap();
    let revision = tabs.revision();
    // The player joins this backend, which lists them itself.
    tabs.observe(version, &backend_add(version, &[3])).unwrap();
    assert!(tabs.sync(version, None, &view).unwrap().is_empty());
    assert!(!tabs.modified());
    // They leave again: the backend removes its entry, so Rift lists them.
    tabs.observe(version, &backend_remove(version, &[3]))
        .unwrap();
    assert!(tabs.revision() > revision);
    assert_eq!(
        tabs.sync(version, None, &view).unwrap(),
        [addition(version, ids(version).unwrap(), &[&entry(3, "Mover")]).unwrap()]
    );
    // A backend removal of a Rift entry deletes it on the client too.
    tabs.observe(version, &backend_remove(version, &[3]))
        .unwrap();
    assert_eq!(tabs.sync(version, None, &view).unwrap().len(), 1);
}

#[test]
fn additions_are_batched() {
    let version = ProtocolVersion::new(777).unwrap();
    let players: Vec<_> = (0..130).map(|uuid| entry(uuid, "Player")).collect();
    let view = TabView {
        header: None,
        players: Some(players),
        names: None,
    };
    let packets = TabList::default().sync(version, None, &view).unwrap();
    let counts: Vec<_> = packets.iter().map(|packet| packet.data[1]).collect();
    assert_eq!(counts, [64, 64, 2]);
}

#[test]
fn rift_owns_the_header_until_it_hands_it_back() {
    for version in switchable() {
        let ids = ids(version).unwrap();
        let mut tabs = TabList::default();
        let backend_header = Packet::new(ids.header, vec![1, 2]);
        assert!(tabs.observe(version, &backend_header).unwrap());
        let view = TabView {
            header: Some((json!("Top"), json!({"text": "Bottom", "color": "gray"}))),
            players: None,
            names: None,
        };
        let packets = tabs.sync(version, None, &view).unwrap();
        let mut expected = Vec::new();
        Text::Component(&json!("Top"))
            .write(version.nbt_components(), &mut expected)
            .unwrap();
        Text::Component(&json!({"text": "Bottom", "color": "gray"}))
            .write(version.nbt_components(), &mut expected)
            .unwrap();
        assert_eq!(packets, [Packet::new(ids.header, expected)]);
        assert!(!tabs.observe(version, &backend_header).unwrap());
        assert!(tabs.sync(version, None, &view).unwrap().is_empty());
        // The client resets its header on reconfiguration; Rift sends it again.
        tabs.reset();
        assert!(!tabs.observe(version, &backend_header).unwrap());
        assert_eq!(tabs.sync(version, None, &view).unwrap().len(), 1);
        let blank = tabs.sync(version, None, &TabView::default()).unwrap();
        assert_eq!(blank.len(), 1);
        assert!(tabs.observe(version, &backend_header).unwrap());
    }
}

#[test]
fn legacy_switches_remove_rift_entries() {
    let version = ProtocolVersion::new(340).unwrap();
    let mut tabs = TabList::default();
    let view = TabView {
        header: None,
        players: Some(vec![entry(3, "Remote")]),
        names: None,
    };
    tabs.sync(version, None, &view).unwrap();
    assert_eq!(
        tabs.remove_remote(version).unwrap(),
        [backend_remove(version, &[3])]
    );
    assert!(tabs.remove_remote(version).unwrap().is_empty());
}

/// The display name as each layout carries it: a JSON string before 1.20.3
/// and NBT from it, with the list order from 1.21.2.
#[test]
fn display_names_and_list_order_are_encoded_for_every_layout() {
    let display = json!({"text": "", "extra": [
        {"text": "[VIP] ", "color": "gold"},
        {"text": "Remote", "color": "#12ab34"},
    ]});
    let remote = ranked(3, "Remote", display.clone(), 10);
    for version in switchable() {
        let number = version.number();
        let ids = ids(version).unwrap();
        let mut tabs = TabList::default();
        let view = TabView {
            header: None,
            players: Some(vec![remote.clone(), entry(4, "Plain")]),
            names: None,
        };
        let packets = tabs.sync(version, None, &view).unwrap();
        // What the client reads: hex colours only from 1.16.
        let rendered = crate::text::for_client(&display, Some(version)).unwrap_or(display.clone());
        assert_eq!(rendered != display, number < 735, "protocol {number}");
        let mut text = Vec::new();
        Text::Component(&rendered)
            .write(version.nbt_components(), &mut text)
            .unwrap();
        if version.nbt_components() {
            assert_eq!(text[0], 10, "protocol {number}"); // TAG_Compound.
        } else {
            let mut json_string = Vec::new();
            write_string(&rendered.to_string(), &mut json_string);
            assert_eq!(text, json_string);
        }
        let mut expected = match number {
            ..761 => vec![0, 2],
            761..768 => vec![1 | 8 | 32, 2],
            _ => vec![1 | 8 | 32 | 64, 2],
        };
        for (uuid, name, shown) in [(3, "Remote", true), (4, "Plain", false)] {
            expected.extend([uuid; 16]);
            write_string(name, &mut expected);
            expected.push(0); // No properties.
            if number >= 761 {
                expected.push(1); // Listed.
            } else {
                expected.extend([0, 0]); // Survival, no latency.
            }
            expected.push(u8::from(shown));
            if shown {
                expected.extend(&text);
            }
            if number >= 768 {
                expected.push(if shown { 10 } else { 0 }); // List order.
            }
            if (759..761).contains(&number) {
                expected.push(0); // No profile key.
            }
        }
        assert_eq!(packets, [Packet::new(ids.players, expected)]);
        // Rift's own packets parse like a backend's.
        let mut seen = Vec::new();
        let listed = changes(version, ids, &packets[0], |uuid, change| {
            seen.push((uuid[0], change));
        });
        assert!(listed.unwrap());
        assert_eq!(
            seen,
            [(3, Change::Added), (4, Change::Added)],
            "protocol {number}"
        );
        assert!(tabs.sync(version, None, &view).unwrap().is_empty());
    }
}

#[test]
fn exact_display_name_bytes_for_one_version_of_each_layout() {
    let remote = ranked(3, "Bob", json!({"text": "[A] Bob", "color": "red"}), 7);
    let packet = |number| {
        let version = ProtocolVersion::new(number).unwrap();
        addition(version, ids(version).unwrap(), &[&remote])
            .unwrap()
            .data
    };
    let json_text = "{\"color\":\"red\",\"text\":\"[A] Bob\"}";
    let head = |action: u8| {
        let mut data = vec![action, 1];
        data.extend([3; 16]);
        data.extend(b"\x03Bob\x00");
        data
    };
    // 1.12.2: action 0, game mode, latency, display name as a JSON string.
    let mut legacy = head(0);
    legacy.extend([0, 0, 1, json_text.len() as u8]);
    legacy.extend(json_text.bytes());
    assert_eq!(packet(340), legacy);
    // 1.20.2: actions add, listed and display name; still a JSON string.
    let mut modern = head(1 | 8 | 32);
    modern.extend([1, 1, json_text.len() as u8]);
    modern.extend(json_text.bytes());
    assert_eq!(packet(764), modern);
    // 1.20.3: the display name is an anonymous NBT compound.
    let mut nbt = head(1 | 8 | 32);
    nbt.extend([1, 1, 10]);
    nbt.extend(b"\x08\x00\x05color\x00\x03red");
    nbt.extend(b"\x08\x00\x04text\x00\x07[A] Bob");
    nbt.push(0);
    assert_eq!(packet(765), nbt);
    // 1.21.2: the list order follows the display name.
    let mut ordered = head(1 | 8 | 32 | 64);
    ordered.extend(&nbt[23..]);
    ordered.push(7);
    assert_eq!(packet(768), ordered);
}

#[test]
fn changed_display_names_update_listed_entries_in_place() {
    for version in switchable() {
        let number = version.number();
        let ids = ids(version).unwrap();
        let mut tabs = TabList::default();
        let view = |players| TabView {
            header: None,
            players: Some(players),
            names: None,
        };
        let plain = view(vec![entry(3, "Remote"), entry(4, "Other")]);
        assert_eq!(tabs.sync(version, None, &plain).unwrap().len(), 1);
        // A prefix arrives: one update for that entry, no removal.
        let display = json!({"text": "[VIP] Remote", "color": "gold"});
        let named = view(vec![
            ranked(3, "Remote", display.clone(), 10),
            entry(4, "Other"),
        ]);
        let action = match number {
            ..761 => 3,
            761..768 => 32,
            _ => 32 | 64,
        };
        let mut expected = vec![action, 1];
        expected.extend([3; 16]);
        expected.push(1);
        Text::Component(&display)
            .write(version.nbt_components(), &mut expected)
            .unwrap();
        if number >= 768 {
            expected.push(10);
        }
        let packets = tabs.sync(version, None, &named).unwrap();
        assert_eq!(
            packets,
            [Packet::new(ids.players, expected)],
            "protocol {number}"
        );
        let mut seen = Vec::new();
        let listed = changes(version, ids, &packets[0], |uuid, change| {
            seen.push((uuid[0], change));
        });
        assert!(listed.unwrap());
        assert_eq!(seen, [(3, Change::Renamed)], "protocol {number}");
        assert!(tabs.sync(version, None, &named).unwrap().is_empty());
        // Only the rank changes: clients without a list order have nothing new.
        let promoted = view(vec![
            ranked(3, "Remote", display.clone(), 20),
            entry(4, "Other"),
        ]);
        let packets = tabs.sync(version, None, &promoted).unwrap();
        assert_eq!(
            packets.len(),
            usize::from(number >= 768),
            "protocol {number}"
        );
        // The prefix goes away again: the display name is cleared.
        let mut expected = vec![action, 1];
        expected.extend([3; 16]);
        expected.push(0);
        if number >= 768 {
            expected.push(0);
        }
        let packets = tabs.sync(version, None, &plain).unwrap();
        assert_eq!(
            packets,
            [Packet::new(ids.players, expected)],
            "protocol {number}"
        );
        assert!(tabs.sync(version, None, &plain).unwrap().is_empty());
    }
}

fn name(display: Json, order: i32) -> TabName {
    TabName {
        display: Some(Arc::new(display)),
        order,
    }
}

fn names(entries: &[(u8, TabName)]) -> TabView {
    TabView {
        names: Some(
            entries
                .iter()
                .map(|(uuid, name)| ([*uuid; 16], name.clone()))
                .collect(),
        ),
        ..TabView::default()
    }
}

/// With `names`, the backend's own entries, the player's included, take
/// Rift's display name and list order; players without one keep the
/// backend's, and players Rift does not know are left alone.
#[test]
fn backend_entries_take_rift_names_for_every_layout() {
    let display = json!({"text": "[VIP] Local", "color": "gold"});
    for version in switchable() {
        let number = version.number();
        let ids = ids(version).unwrap();
        let mut tabs = TabList::default();
        tabs.observe(version, &backend_add(version, &[1, 2, 5]))
            .unwrap();
        let view = names(&[
            (1, name(json!("Own"), 3)),
            (2, name(display.clone(), 10)),
            (9, name(json!("Elsewhere"), 1)),
        ]);
        let packets = tabs.sync(version, Some([1; 16]), &view).unwrap();
        let shown = |order: i32| if number >= 768 { order } else { 0 };
        assert_eq!(
            packets,
            [update(
                version,
                ids,
                &[
                    ([1; 16], (Some(Arc::new(json!("Own"))), shown(3))),
                    ([2; 16], (Some(Arc::new(display.clone())), shown(10))),
                ]
            )
            .unwrap()],
            "protocol {number}"
        );
        // Rift's update parses as a rename, never as an addition.
        let mut seen = Vec::new();
        changes(version, ids, &packets[0], |uuid, change| {
            seen.push((uuid[0], change))
        })
        .unwrap();
        assert_eq!(seen, [(1, Change::Renamed), (2, Change::Renamed)]);
        assert!(tabs.modified());
        assert!(tabs.sync(version, Some([1; 16]), &view).unwrap().is_empty());
    }
}

#[test]
fn backend_renames_and_re_adds_are_named_again() {
    let version = ProtocolVersion::new(774).unwrap();
    let ids = ids(version).unwrap();
    let mut tabs = TabList::default();
    tabs.observe(version, &backend_add(version, &[2])).unwrap();
    let view = names(&[(2, name(json!("[VIP] Local"), 10))]);
    assert_eq!(tabs.sync(version, None, &view).unwrap().len(), 1);
    // The backend sets its own display name: Rift sets its name again.
    let revision = tabs.revision();
    let mut rename = vec![32, 1];
    rename.extend([2; 16]);
    rename.push(0);
    assert!(
        tabs.observe(version, &Packet::new(ids.players, rename))
            .unwrap()
    );
    assert!(tabs.revision() > revision);
    assert_eq!(tabs.sync(version, None, &view).unwrap().len(), 1);
    // A list order update counts too, on its own.
    let revision = tabs.revision();
    let mut order = vec![64, 1];
    order.extend([2; 16]);
    order.push(1);
    tabs.observe(version, &Packet::new(ids.players, order))
        .unwrap();
    assert!(tabs.revision() > revision);
    assert_eq!(tabs.sync(version, None, &view).unwrap().len(), 1);
    // The backend adds the entry again, as on a respawn.
    tabs.observe(version, &backend_add(version, &[2])).unwrap();
    assert_eq!(tabs.sync(version, None, &view).unwrap().len(), 1);
    // A removed entry is forgotten and not renamed.
    tabs.observe(version, &backend_remove(version, &[2]))
        .unwrap();
    assert!(tabs.sync(version, None, &view).unwrap().is_empty());
    assert!(!tabs.modified());
    // A reconfiguration forgets the names with the entries.
    tabs.observe(version, &backend_add(version, &[2])).unwrap();
    tabs.sync(version, None, &view).unwrap();
    tabs.reset();
    assert!(!tabs.modified());
}

#[test]
fn names_rift_set_are_cleared_when_they_go_away() {
    let version = ProtocolVersion::new(774).unwrap();
    let ids = ids(version).unwrap();
    let mut tabs = TabList::default();
    tabs.observe(version, &backend_add(version, &[2, 3]))
        .unwrap();
    let named = names(&[
        (2, name(json!("[VIP] Local"), 10)),
        (3, name(json!("[Mod] Other"), 50)),
    ]);
    tabs.sync(version, None, &named).unwrap();
    // Bob loses his prefix: his entry is cleared, Carol's is kept.
    let fewer = names(&[(3, name(json!("[Mod] Other"), 50))]);
    let plain = (None, 0);
    assert_eq!(
        tabs.sync(version, None, &fewer).unwrap(),
        [update(version, ids, &[([2; 16], plain.clone())]).unwrap()]
    );
    assert!(tabs.sync(version, None, &fewer).unwrap().is_empty());
    // Turning names off clears every name Rift set.
    assert_eq!(
        tabs.sync(version, None, &TabView::default()).unwrap(),
        [update(version, ids, &[([3; 16], plain)]).unwrap()]
    );
    assert!(!tabs.modified());
    assert!(
        tabs.sync(version, None, &TabView::default())
            .unwrap()
            .is_empty()
    );
}
