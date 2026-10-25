use super::*;
use crate::protocol::{read_string, read_varint};
use serde_json::json;

fn switchable() -> impl Iterator<Item = ProtocolVersion> {
    (0..=777)
        .filter_map(|number| ProtocolVersion::new(number).ok())
        .filter(|version| version.supports_switching())
}

fn version(number: i32) -> ProtocolVersion {
    ProtocolVersion::new(number).unwrap()
}

fn bar(text: &str) -> BossBar {
    BossBar {
        text: json!(text),
        progress: 1.0,
        color: 0,
        style: 0,
        flags: 0,
    }
}

/// UUID and action of a Boss Event packet, and the bytes after them.
fn split(packet: &Packet) -> ([u8; 16], i32, &[u8]) {
    let (uuid, mut rest) = packet.data.split_at(16);
    let action = read_varint(&mut rest).unwrap();
    (uuid.try_into().unwrap(), action, rest)
}

#[test]
fn every_switchable_version_from_1_9_has_a_boss_event_id() {
    for version in switchable() {
        assert_eq!(
            id(version).is_some(),
            version.number() >= 107,
            "protocol {}",
            version.number()
        );
        assert_eq!(supported(version.number()), version.number() >= 107);
    }
    for (number, expected) in [
        (107, 0x0c),
        (573, 0x0d),
        (759, 0x0a),
        (762, 0x0b),
        (763, 0x0b),
        (764, 0x0a),
        (765, 0x0a),
        (769, 0x0a),
        (770, 0x09),
        (774, 0x09),
        (776, 0x09),
        (777, 0x09),
    ] {
        assert_eq!(id(version(number)), Some(expected), "protocol {number}");
    }
    assert!(!supported(47));
    assert!(!supported(-1));
}

#[test]
fn adds_update_only_changed_fields_and_remove_unwanted_bars() {
    let version = version(763);
    let mut bars = BossBars::default();
    let mut wanted = BTreeMap::new();
    wanted.insert("event".to_owned(), bar("Event"));
    let packets = bars.sync(version, &wanted).unwrap();
    assert_eq!(packets.len(), 1);
    let (event, action, mut rest) = split(&packets[0]);
    assert_eq!(packets[0].id, 0x0b);
    assert_eq!(action, ADD);
    assert_eq!(read_string(&mut rest, 262144).unwrap(), "\"Event\"");
    assert_eq!(rest, [0x3f, 0x80, 0, 0, 0, 0, 0]);
    assert!(bars.sync(version, &wanted).unwrap().is_empty());

    wanted.insert(
        "event".to_owned(),
        BossBar {
            progress: 0.5,
            color: 2,
            style: 1,
            ..bar("Event")
        },
    );
    let packets = bars.sync(version, &wanted).unwrap();
    let actions: Vec<_> = packets.iter().map(|packet| split(packet).1).collect();
    assert_eq!(actions, [PROGRESS, STYLE]);
    assert_eq!(split(&packets[0]).2, 0.5f32.to_be_bytes());
    assert_eq!(split(&packets[1]).2, [2, 1]);

    wanted.insert(
        "event".to_owned(),
        BossBar {
            flags: 5,
            ..bar("Renamed")
        },
    );
    wanted.insert("vote".to_owned(), bar("Vote"));
    let packets = bars.sync(version, &wanted).unwrap();
    let summary: Vec<_> = packets
        .iter()
        .map(|packet| (split(packet).0, split(packet).1))
        .collect();
    let vote = uuid("vote");
    assert_eq!(
        summary,
        [
            (event, NAME),
            (event, PROGRESS),
            (event, STYLE),
            (event, FLAGS),
            (vote, ADD)
        ]
    );
    assert_eq!(split(&packets[3]).2, [5]);

    wanted.remove("event");
    let packets = bars.sync(version, &wanted).unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(split(&packets[0]), (event, REMOVE, &[][..]));
}

#[test]
fn a_reset_client_receives_every_bar_again() {
    let version = version(767);
    let mut bars = BossBars::default();
    let wanted = BTreeMap::from([("a".to_owned(), bar("A")), ("b".to_owned(), bar("B"))]);
    assert_eq!(bars.sync(version, &wanted).unwrap().len(), 2);
    let revision = bars.revision();
    bars.reset();
    assert_ne!(bars.revision(), revision);
    let packets = bars.sync(version, &wanted).unwrap();
    assert!(packets.iter().all(|packet| split(packet).1 == ADD));
    assert_eq!(packets.len(), 2);
}

#[test]
fn text_is_network_nbt_from_1_20_3() {
    let wanted = BTreeMap::from([(
        "gold".to_owned(),
        BossBar {
            text: json!({"text": "Gold", "color": "gold"}),
            ..bar("")
        },
    )]);
    let packets = BossBars::default().sync(version(765), &wanted).unwrap();
    let (_, _, rest) = split(&packets[0]);
    let mut nbt = Vec::new();
    crate::protocol::nbt::component(&wanted["gold"].text, &mut nbt).unwrap();
    assert_eq!(&rest[..nbt.len()], nbt);
    assert_eq!(rest.len(), nbt.len() + 7);
}

#[test]
fn versions_without_boss_bars_send_nothing() {
    let wanted = BTreeMap::from([("a".to_owned(), bar("A"))]);
    assert!(
        BossBars::default()
            .sync(version(47), &wanted)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn bar_uuids_are_stable_distinct_and_marked() {
    assert_eq!(uuid("event"), uuid("event"));
    assert_ne!(uuid("event"), uuid("events"));
    assert_eq!(&uuid("event")[..4], b"rift");
    assert_eq!(uuid("event")[6] >> 4, 8);
    assert_eq!(uuid("event")[8] >> 6, 2);
}
