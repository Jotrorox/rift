use super::{NEWER, OLDER, down::Down, testing::Harness, up::Up};
use crate::{
    Direction, State,
    wire::{Reader, Writer},
};
use State::{Configuration, Play};

fn up() -> Harness {
    Harness::new(super::UP, NEWER, OLDER)
}

fn down() -> Harness {
    Harness::new(super::DOWN, OLDER, NEWER)
}

fn features(names: &[&str]) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.varint(names.len() as i32);
    for name in names {
        writer.string(name);
    }
    writer.into_inner()
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(Up::routes().problems(), &[] as &[String]);
    assert_eq!(Down::routes().problems(), &[] as &[String]);
}

#[test]
fn feature_flags_follow_what_each_client_knows() {
    // The flags the official 1.20.6 and 1.21.1 servers send by default.
    let vanilla = super::testing::hex("01116d696e6563726166743a76616e696c6c61");
    let out = up().one_clientbound(Configuration, "update_enabled_features", &vanilla);
    assert_eq!(out.data, vanilla);
    let out = down().one_clientbound(Configuration, "update_enabled_features", &vanilla);
    assert_eq!(
        out.data,
        features(&["minecraft:vanilla", "minecraft:update_1_21"])
    );

    let experimental = features(&[
        "minecraft:vanilla",
        "minecraft:update_1_21",
        "minecraft:bundle",
    ]);
    let out = up().one_clientbound(Configuration, "update_enabled_features", &experimental);
    assert_eq!(
        out.data,
        features(&["minecraft:vanilla", "minecraft:bundle"])
    );
    let out = down().one_clientbound(Configuration, "update_enabled_features", &experimental);
    assert_eq!(out.data, experimental);
}

#[test]
fn custom_report_details_and_server_links_never_reach_a_1_20_5_client() {
    let mut down = down();
    let links = super::testing::hex("01 00 00 12 68747470733a2f2f6578616d706c652e6f7267");
    assert!(
        down.clientbound(Configuration, "server_links", &links)
            .unwrap()
            .is_empty()
    );
    assert!(
        down.clientbound(Play, "server_links", &links)
            .unwrap()
            .is_empty()
    );
    let details = super::testing::hex("01 03 6b6579 05 76616c7565");
    assert!(
        down.clientbound(Play, "custom_report_details", &details)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn projectile_power_becomes_a_length_and_back_a_vector_along_the_motion() {
    let mut body = Writer::new();
    body.varint(42).f64(0.06).f64(0.0).f64(-0.08);
    let out = up().one_clientbound(Play, "projectile_power", &body.buf);
    let mut reader = Reader::new(&out.data);
    assert_eq!(reader.varint().unwrap(), 42);
    assert!((reader.f64().unwrap() - 0.1).abs() < 1e-12);
    reader.finish().unwrap();

    let mut down = down();
    let mut power = Writer::new();
    power.varint(42).f64(0.1);
    // Without the motion it follows, the vector cannot be rebuilt.
    assert!(
        down.clientbound(Play, "projectile_power", &power.buf)
            .unwrap()
            .is_empty()
    );
    let mut motion = Writer::new();
    motion.varint(42).i16(2400).i16(0).i16(-3200);
    let out = down.one_clientbound(Play, "set_entity_motion", &motion.buf);
    assert_eq!(out.data, motion.buf);
    let out = down.one_clientbound(Play, "projectile_power", &power.buf);
    let mut reader = Reader::new(&out.data);
    assert_eq!(reader.varint().unwrap(), 42);
    let vector = [
        reader.f64().unwrap(),
        reader.f64().unwrap(),
        reader.f64().unwrap(),
    ];
    reader.finish().unwrap();
    for (got, want) in vector.iter().zip([0.06, 0.0, -0.08]) {
        assert!((got - want).abs() < 1e-12, "{vector:?}");
    }
    // Motion of another entity does not count.
    motion.buf[0] = 43;
    down.clientbound(Play, "set_entity_motion", &motion.buf)
        .unwrap();
    assert!(
        down.clientbound(Play, "projectile_power", &power.buf)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn credits_roll_on_a_1_21_client_and_are_answered_for_it() {
    // "Win game" with value 0: the player has seen the credits.
    let skip = super::testing::hex("04 00000000");
    let out = up().clientbound(Play, "game_event", &skip).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].direction, Direction::Serverbound);
    assert_eq!(out[0].name, "client_command");
    assert_eq!(out[0].data, [0]);
    let roll = super::testing::hex("04 3f800000");
    assert_eq!(up().one_clientbound(Play, "game_event", &roll).data, roll);
    let out = down().one_clientbound(Play, "game_event", &skip);
    assert_eq!(out.data, roll);
    // Other events pass untouched, as the official servers recorded them.
    let rain = super::testing::hex("07 3e4cccce");
    assert_eq!(up().one_clientbound(Play, "game_event", &rain).data, rain);
    assert_eq!(down().one_clientbound(Play, "game_event", &rain).data, rain);
}

#[test]
fn mount_screens_count_columns_on_1_21_and_slots_before() {
    let mut body = Writer::new();
    body.u8(3).varint(16).i32(77); // A donkey with a chest: saddle + 15.
    let out = up().one_clientbound(Play, "mount_screen_open", &body.buf);
    let mut columns = Writer::new();
    columns.u8(3).varint(5).i32(77);
    assert_eq!(out.data, columns.buf);
    let out = down().one_clientbound(Play, "mount_screen_open", &columns.buf);
    assert_eq!(out.data, body.buf);
    let mut horse = Writer::new();
    horse.u8(3).varint(1).i32(77);
    let out = up().one_clientbound(Play, "mount_screen_open", &horse.buf);
    assert_eq!(out.data[1], 0);
}

fn open_enchanting_table(harness: &mut Harness) {
    let menu = super::enchantment_menu();
    let mut body = Writer::new();
    body.varint(5).varint(menu);
    // An empty plain-text title, as NBT in both protocols.
    body.bytes(&[8, 0, 0]);
    harness.clientbound(Play, "open_screen", &body.buf).unwrap();
}

fn set_data(property: i16, value: i16) -> Vec<u8> {
    let mut body = Writer::new();
    body.u8(5).i16(property).i16(value);
    body.into_inner()
}

#[test]
fn enchanting_table_hints_follow_the_synchronised_registry() {
    let older = crate::data::version(OLDER).expect_registry("enchantment");
    let sharpness = older.id("sharpness").unwrap();
    let synced = super::vanilla_synced_enchantments();
    let synced_sharpness = synced.iter().position(|name| name == "sharpness").unwrap() as i16;

    let mut up = up();
    // Outside the enchanting table the same property is something else.
    let other = set_data(4, sharpness as i16);
    assert_eq!(
        up.one_clientbound(Play, "container_set_data", &other).data,
        other
    );
    open_enchanting_table(&mut up);
    let out = up.one_clientbound(Play, "container_set_data", &set_data(4, sharpness as i16));
    assert_eq!(out.data, set_data(4, synced_sharpness));
    // Levels and "no hint" stay as they are.
    assert_eq!(
        up.one_clientbound(Play, "container_set_data", &set_data(7, 3))
            .data,
        set_data(7, 3)
    );
    assert_eq!(
        up.one_clientbound(Play, "container_set_data", &set_data(5, -1))
            .data,
        set_data(5, -1)
    );
    up.serverbound(Play, "container_close", &[5]).unwrap();
    assert_eq!(
        up.one_clientbound(Play, "container_set_data", &other).data,
        other
    );

    let mut down = down();
    open_enchanting_table(&mut down);
    let out = down.one_clientbound(Play, "container_set_data", &set_data(6, synced_sharpness));
    assert_eq!(out.data, set_data(6, sharpness as i16));
    // A registry the backend reordered is followed; a custom entry has no
    // 1.20.5 ID and shows no hint.
    let mut registry = Writer::new();
    registry.string("minecraft:enchantment").varint(2);
    registry.string("mypack:zap").bool(false);
    registry.string("minecraft:sharpness").bool(false);
    down.clientbound(Configuration, "registry_data", &registry.buf)
        .unwrap();
    let out = down.one_clientbound(Play, "container_set_data", &set_data(6, 1));
    assert_eq!(out.data, set_data(6, sharpness as i16));
    let out = down.one_clientbound(Play, "container_set_data", &set_data(6, 0));
    assert_eq!(out.data, set_data(6, -1));
}

fn use_item(rotation: Option<(f32, f32)>) -> Vec<u8> {
    let mut body = Writer::new();
    body.varint(0).varint(17);
    if let Some((yaw, pitch)) = rotation {
        body.f32(yaw).f32(pitch);
    }
    body.into_inner()
}

#[test]
fn use_item_carries_the_rotation_on_1_21() {
    let mut up = up();
    let mut status = Writer::new();
    status.bool(true);
    up.serverbound(Play, "move_player_status_only", &status.buf)
        .unwrap();
    let out = up
        .serverbound(Play, "use_item", &use_item(Some((90.0, -10.0))))
        .unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].name, "move_player_rot");
    let mut rotation = Writer::new();
    rotation.f32(90.0).f32(-10.0).bool(true);
    assert_eq!(out[0].data, rotation.buf);
    assert_eq!(out[1].name, "use_item");
    assert_eq!(out[1].data, use_item(None));
    // The backend already has this rotation.
    let out = up
        .serverbound(Play, "use_item", &use_item(Some((90.0, -10.0))))
        .unwrap();
    assert_eq!(out.len(), 1);
    let mut rot = Writer::new();
    rot.f32(45.0).f32(5.0).bool(false);
    up.serverbound(Play, "move_player_rot", &rot.buf).unwrap();
    let out = up
        .serverbound(Play, "use_item", &use_item(Some((45.0, 5.0))))
        .unwrap();
    assert_eq!(out.len(), 1);
    assert!(
        up.serverbound(Play, "use_item", &use_item(Some((f32::NAN, 0.0))))
            .is_err()
    );

    let mut down = down();
    let mut pos_rot = Writer::new();
    pos_rot
        .f64(1.0)
        .f64(64.0)
        .f64(-3.0)
        .f32(-30.0)
        .f32(12.5)
        .bool(true);
    down.serverbound(Play, "move_player_pos_rot", &pos_rot.buf)
        .unwrap();
    let out = down.one_serverbound(Play, "use_item", &use_item(None));
    assert_eq!(out.data, use_item(Some((-30.0, 12.5))));
}

#[test]
fn malformed_packets_fail_without_panicking() {
    let mut up = up();
    let mut down = down();
    for (name, body) in [
        ("projectile_power", &[1u8, 0, 0][..]),
        ("game_event", &[4, 0][..]),
        ("mount_screen_open", &[1, 0x80][..]),
        ("container_set_data", &[1][..]),
    ] {
        assert!(up.clientbound(Play, name, body).is_err(), "{name}");
        assert!(down.clientbound(Play, name, body).is_err(), "{name}");
    }
    assert!(
        up.clientbound(Configuration, "update_enabled_features", &[5, 1])
            .is_err()
    );
    assert!(
        down.clientbound(Configuration, "update_enabled_features", &[1, 0xff])
            .is_err()
    );
    assert!(up.serverbound(Play, "use_item", &[0, 0, 0]).is_err());
    assert!(down.serverbound(Play, "use_item", &[0, 0, 0]).is_err());
    assert!(
        down.clientbound(Configuration, "registry_data", &[3, b'a'])
            .is_err()
    );
    // Trailing bytes mean the layout did not match.
    assert!(
        up.clientbound(Play, "game_event", &[4, 0, 0, 0, 0, 9])
            .is_err()
    );
}

/// Every packet of the recorded sessions that this pair's own handlers
/// convert goes through, and comes out in the other version's layout.
#[test]
fn recorded_sessions_convert_through_both_steps() {
    let own = [
        "update_enabled_features",
        "game_event",
        "projectile_power",
        "mount_screen_open",
        "container_set_data",
        "use_item",
    ];
    for (protocol, mut harness) in [(OLDER, up()), (NEWER, down())] {
        let Some(recorded) = super::testing::corpus(protocol) else {
            continue;
        };
        let mut converted = 0;
        for packet in &recorded {
            let from = match packet.direction {
                Direction::Clientbound => harness.backend,
                Direction::Serverbound => harness.client,
            };
            if packet.direction == Direction::Serverbound && from != protocol {
                continue;
            }
            if !own.contains(&packet.name(protocol)) {
                continue;
            }
            harness
                .send_id(packet.direction, packet.state, packet.id, &packet.data)
                .unwrap_or_else(|error| panic!("{}: {error}", packet.name(protocol)));
            converted += 1;
        }
        assert!(converted > 100, "{protocol}: only {converted} packets");
    }
}
