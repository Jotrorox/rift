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
        };
        let packets = tabs.sync(version, Some([1; 16]), &view).unwrap();
        assert_eq!(packets, [addition(version, ids, &[&entry(3, "Remote")])]);
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
        [addition(
            version,
            ids(version).unwrap(),
            &[&entry(3, "Mover")]
        )]
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
    };
    tabs.sync(version, None, &view).unwrap();
    assert_eq!(
        tabs.remove_remote(version).unwrap(),
        [backend_remove(version, &[3])]
    );
    assert!(tabs.remove_remote(version).unwrap().is_empty());
}
