use super::{NEWER, OLDER, down::Down, up::Up};
use crate::{
    Direction, State,
    steps::v1_20_5_to_1_21::testing::{Harness, Out, corpus, hex},
    wire::{Reader, Writer},
};
use State::{Configuration, Login, Play};

fn up() -> Harness {
    Harness::new(super::UP, NEWER, OLDER)
}

fn down() -> Harness {
    Harness::new(super::DOWN, OLDER, NEWER)
}

fn item(protocol: i32, name: &str) -> i32 {
    super::registry_id(protocol, "item", name)
}

fn names(out: &[Out]) -> Vec<&str> {
    out.iter().map(|out| out.name.as_str()).collect()
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(Up::routes().problems(), &[] as &[String]);
    assert_eq!(Down::routes().problems(), &[] as &[String]);
}

#[test]
fn login_success_loses_and_regains_the_strict_flag() {
    // As the official 1.21.1 and 1.21.3 servers sent them.
    let older = hex("888b6130823b387bb012d6dc24e47d5906436f727075730001");
    let newer = hex("888b6130823b387bb012d6dc24e47d5906436f7270757300");
    assert_eq!(
        up().one_clientbound(Login, "login_finished", &older).data,
        newer
    );
    assert_eq!(
        down().one_clientbound(Login, "login_finished", &newer).data,
        older
    );
    assert!(up().clientbound(Login, "login_finished", &newer).is_err());
}

#[test]
fn feature_flags_follow_what_each_client_knows() {
    let flags = |names: &[&str]| {
        let mut writer = Writer::new();
        writer.varint(names.len() as i32);
        for name in names {
            writer.string(name);
        }
        writer.into_inner()
    };
    let older = flags(&["minecraft:vanilla", "minecraft:bundle"]);
    let out = up().one_clientbound(Configuration, "update_enabled_features", &older);
    assert_eq!(out.data, flags(&["minecraft:vanilla"]));
    let newer = flags(&["minecraft:vanilla", "minecraft:winter_drop"]);
    let out = down().one_clientbound(Configuration, "update_enabled_features", &newer);
    assert_eq!(out.data, flags(&["minecraft:vanilla", "minecraft:bundle"]));
}

fn client_information(particles: Option<i32>) -> Vec<u8> {
    let mut writer = Writer::new();
    writer
        .string("en_us")
        .i8(12)
        .varint(0)
        .bool(true)
        .u8(0x7f)
        .varint(1);
    writer.bool(false).bool(true);
    if let Some(particles) = particles {
        writer.varint(particles);
    }
    writer.into_inner()
}

#[test]
fn client_information_gains_the_particle_setting() {
    for state in [Configuration, Play] {
        let out = up().one_serverbound(state, "client_information", &client_information(Some(2)));
        assert_eq!(out.data, client_information(None));
        let out = down().one_serverbound(state, "client_information", &client_information(None));
        assert_eq!(out.data, client_information(Some(0)));
    }
}

fn spawn_info(writer: &mut Writer, sea_level: Option<i32>) {
    writer
        .varint(0)
        .string("minecraft:overworld")
        .i64(-42)
        .u8(0)
        .i8(-1);
    writer.bool(false).bool(true);
    writer.bool(true).string("minecraft:the_nether").i64(0x1234);
    writer.varint(0);
    if let Some(sea_level) = sea_level {
        writer.varint(sea_level);
    }
}

fn login(sea_level: Option<i32>) -> Vec<u8> {
    let mut writer = Writer::new();
    writer
        .i32(7)
        .bool(false)
        .varint(1)
        .string("minecraft:overworld");
    writer
        .varint(20)
        .varint(10)
        .varint(8)
        .bool(false)
        .bool(true)
        .bool(false);
    spawn_info(&mut writer, sea_level);
    writer.bool(true);
    writer.into_inner()
}

fn respawn(sea_level: Option<i32>) -> Vec<u8> {
    let mut writer = Writer::new();
    spawn_info(&mut writer, sea_level);
    writer.u8(3);
    writer.into_inner()
}

#[test]
fn login_and_respawn_carry_the_sea_level_on_1_21_2() {
    assert_eq!(
        up().one_clientbound(Play, "login", &login(None)).data,
        login(Some(63))
    );
    assert_eq!(
        down()
            .one_clientbound(Play, "login", &login(Some(-60)))
            .data,
        login(None)
    );
    assert_eq!(
        up().one_clientbound(Play, "respawn", &respawn(None)).data,
        respawn(Some(63))
    );
    assert_eq!(
        down()
            .one_clientbound(Play, "respawn", &respawn(Some(62)))
            .data,
        respawn(None)
    );
    assert!(up().clientbound(Play, "login", &login(Some(63))).is_err());

    // The recorded joins: 1.21.1 up and back down is the original; 1.21.3
    // down and back up has the sea level every 1.21 world reports (the
    // recorded superflat world's is -63).
    for (protocol, there, back) in [(OLDER, up(), down()), (NEWER, down(), up())] {
        let (mut there, mut back) = (there, back);
        let Some(recorded) = corpus(protocol) else {
            continue;
        };
        let login = recorded
            .iter()
            .find(|packet| packet.name(protocol) == "login")
            .expect("the session has a login");
        let converted = there.one_clientbound(Play, "login", &login.data);
        let restored = back.one_clientbound(Play, "login", &converted.data);
        let mut expected = login.data.clone();
        if protocol == NEWER {
            // The sea level is the last VarInt before the secure chat flag.
            let mut end = expected.len() - 2;
            while end > 0 && expected[end - 1] & 0x80 != 0 {
                end -= 1;
            }
            expected.splice(end..expected.len() - 1, [63]);
        }
        assert_eq!(restored.data, expected, "{protocol}");
    }
}

#[test]
fn frozen_day_time_is_a_negative_time_on_1_21_and_a_flag_on_1_21_2() {
    let older = |day: i64| {
        let mut writer = Writer::new();
        writer.i64(1000).i64(day);
        writer.into_inner()
    };
    let newer = |day: i64, ticking: bool| {
        let mut writer = Writer::new();
        writer.i64(1000).i64(day).bool(ticking);
        writer.into_inner()
    };
    assert_eq!(
        up().one_clientbound(Play, "set_time", &older(6000)).data,
        newer(6000, true)
    );
    assert_eq!(
        up().one_clientbound(Play, "set_time", &older(-6000)).data,
        newer(6000, false)
    );
    assert_eq!(
        down()
            .one_clientbound(Play, "set_time", &newer(6000, true))
            .data,
        older(6000)
    );
    assert_eq!(
        down()
            .one_clientbound(Play, "set_time", &newer(6000, false))
            .data,
        older(-6000)
    );
    assert_eq!(
        down()
            .one_clientbound(Play, "set_time", &newer(0, false))
            .data,
        older(-1)
    );
}

fn older_position(flags: u8, teleport: i32) -> Vec<u8> {
    let mut writer = Writer::new();
    writer
        .f64(1.5)
        .f64(-60.0)
        .f64(5.5)
        .f32(90.0)
        .f32(0.0)
        .u8(flags)
        .varint(teleport);
    writer.into_inner()
}

fn newer_position(teleport: i32, delta: [f64; 3], flags: i32) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.varint(teleport).f64(1.5).f64(-60.0).f64(5.5);
    for value in delta {
        writer.f64(value);
    }
    writer.f32(90.0).f32(0.0).i32(flags);
    writer.into_inner()
}

#[test]
fn teleports_keep_the_motion_along_relative_axes() {
    // A 1.21.4 client at the end of the chain answers teleports in order.
    let mut later = up();
    later.ends = Some((769, OLDER));
    let out = later.one_clientbound(Play, "player_position", &older_position(0b11010, 1));
    assert_eq!(out.data, newer_position(1, [0.0; 3], 0b11010 | 1 << 6));

    let mut down = down();
    let out = down.one_clientbound(
        Play,
        "player_position",
        &newer_position(1, [0.0; 3], 0b11010 | 1 << 6),
    );
    assert_eq!(out.data, older_position(0b11010, 1));
}

#[test]
fn a_1_21_2_client_answers_teleports_after_the_ping_that_marks_them() {
    let mut up = up();
    let out = up
        .clientbound(Play, "player_position", &older_position(0, 9))
        .unwrap();
    assert_eq!(
        names(&out),
        [
            "bundle_delimiter",
            "ping",
            "player_position",
            "bundle_delimiter"
        ]
    );
    let ping = out[1].data.clone();
    // The client answers: pong, its position, then the acceptance.
    assert!(up.serverbound(Play, "pong", &ping).unwrap().is_empty());
    let mut pos_rot = Writer::new();
    pos_rot
        .f64(1.5)
        .f64(-60.0)
        .f64(5.5)
        .f32(90.0)
        .f32(0.0)
        .u8(1);
    assert!(
        up.serverbound(Play, "move_player_pos_rot", &pos_rot.buf)
            .unwrap()
            .is_empty()
    );
    let mut accept = Writer::new();
    accept.varint(9);
    let out = up
        .serverbound(Play, "accept_teleportation", &accept.buf)
        .unwrap();
    assert_eq!(names(&out), ["accept_teleportation", "move_player_pos_rot"]);
    let mut older = Writer::new();
    older
        .f64(1.5)
        .f64(-60.0)
        .f64(5.5)
        .f32(90.0)
        .f32(0.0)
        .bool(true);
    assert_eq!(out[1].data, older.buf);
    // Other pongs are the backend's.
    let out = up.serverbound(Play, "pong", &[0, 0, 0, 5]).unwrap();
    assert_eq!(names(&out), ["pong"]);
    // Inside a backend bundle no second bundle is opened.
    up.clientbound(Play, "bundle_delimiter", &[]).unwrap();
    let out = up
        .clientbound(Play, "player_position", &older_position(0, 10))
        .unwrap();
    assert_eq!(names(&out), ["ping", "player_position"]);
}

fn join(harness: &mut Harness, sea_level: Option<i32>) {
    harness
        .clientbound(Play, "login", &login(sea_level))
        .unwrap();
}

#[test]
fn motion_1_21_cannot_express_follows_the_teleport() {
    let mut down = down();
    join(&mut down, Some(63));
    // Absolute position with a new motion: set it.
    let out = down
        .clientbound(
            Play,
            "player_position",
            &newer_position(2, [0.5, 0.0, -0.25], 0),
        )
        .unwrap();
    assert_eq!(names(&out), ["player_position", "set_entity_motion"]);
    let mut motion = Writer::new();
    motion.varint(7).i16(4000).i16(0).i16(-2000);
    assert_eq!(out[1].data, motion.buf);
    // Relative everything with an added motion: push.
    let out = down
        .clientbound(
            Play,
            "player_position",
            &newer_position(3, [0.0, 1.0, 0.0], 0b11100111),
        )
        .unwrap();
    assert_eq!(names(&out), ["player_position", "explode"]);
    // Relative position, kept motion: what 1.21 does by itself.
    let out = down
        .clientbound(
            Play,
            "player_position",
            &newer_position(4, [0.0; 3], 0b11100111),
        )
        .unwrap();
    assert_eq!(names(&out), ["player_position"]);
}

#[test]
fn entity_teleports_become_position_syncs_and_back() {
    let mut older = Writer::new();
    older
        .varint(19)
        .f64(-0.5)
        .f64(-60.0)
        .f64(5.5)
        .i8(64)
        .i8(-32)
        .bool(true);
    let out = up().one_clientbound(Play, "teleport_entity", &older.buf);
    assert_eq!(out.name, "entity_position_sync");
    let mut newer = Writer::new();
    newer
        .varint(19)
        .f64(-0.5)
        .f64(-60.0)
        .f64(5.5)
        .f64(0.0)
        .f64(0.0)
        .f64(0.0);
    newer.f32(90.0).f32(-45.0).bool(true);
    assert_eq!(out.data, newer.buf);
    let out = down().one_clientbound(Play, "entity_position_sync", &newer.buf);
    assert_eq!(out.name, "teleport_entity");
    assert_eq!(out.data, older.buf);
}

fn newer_teleport(entity: i32, flags: i32) -> Vec<u8> {
    let mut writer = Writer::new();
    writer
        .varint(entity)
        .f64(1.0)
        .f64(0.0)
        .f64(-1.0)
        .f64(0.0)
        .f64(0.0)
        .f64(0.0);
    writer.f32(10.0).f32(0.0).i32(flags).bool(false);
    writer.into_inner()
}

#[test]
fn a_teleport_of_the_players_vehicle_moves_it_on_1_21() {
    let mut down = down();
    join(&mut down, Some(63));
    // Absolute teleports of anything are 1.21 entity teleports.
    let out = down.one_clientbound(Play, "teleport_entity", &newer_teleport(30, 0));
    assert_eq!(out.name, "teleport_entity");
    // Relative ones are only meaningful for the vehicle.
    assert!(
        down.clientbound(Play, "teleport_entity", &newer_teleport(30, 0b111))
            .unwrap()
            .is_empty()
    );
    let mut passengers = Writer::new();
    passengers.varint(30).varint(1).varint(7);
    down.clientbound(Play, "set_passengers", &passengers.buf)
        .unwrap();
    let mut moved = Writer::new();
    moved.f64(100.0).f64(64.0).f64(-100.0).f32(45.0).f32(0.0);
    down.serverbound(Play, "move_vehicle", &moved.buf).unwrap();
    let out = down.one_clientbound(Play, "teleport_entity", &newer_teleport(30, 0b1111));
    assert_eq!(out.name, "move_vehicle");
    let mut vehicle = Writer::new();
    vehicle.f64(101.0).f64(64.0).f64(-101.0).f32(55.0).f32(0.0);
    assert_eq!(out.data, vehicle.buf);
}

#[test]
fn turning_the_player_looks_far_ahead_in_that_direction() {
    let mut down = down();
    let mut position = Writer::new();
    position.f64(10.0).f64(64.0).f64(-3.0).bool(true);
    down.serverbound(Play, "move_player_pos", &position.buf)
        .unwrap();
    let mut rotation = Writer::new();
    rotation.f32(90.0).f32(0.0);
    let out = down.one_clientbound(Play, "player_rotation", &rotation.buf);
    assert_eq!(out.name, "player_look_at");
    let mut reader = Reader::new(&out.data);
    assert_eq!(reader.varint().unwrap(), 1);
    let target = [
        reader.f64().unwrap(),
        reader.f64().unwrap(),
        reader.f64().unwrap(),
    ];
    assert!(!reader.bool().unwrap());
    // Yaw 90 faces negative X.
    assert!(target[0] < -9.0e4);
    assert!((target[1] - (64.0 + super::down::EYE_HEIGHT)).abs() < 1e-6);
    assert!((target[2] + 3.0).abs() < 1e-6);
}

#[test]
fn minecarts_moving_along_tracks_jump_to_the_last_step() {
    let mut body = Writer::new();
    body.varint(40).varint(2);
    for x in [1.0, 2.0] {
        body.f64(x)
            .f64(0.0)
            .f64(0.0)
            .f64(0.1)
            .f64(0.0)
            .f64(0.0)
            .i8(0)
            .i8(0)
            .f32(0.5);
    }
    let out = down()
        .clientbound(Play, "move_minecart_along_track", &body.buf)
        .unwrap();
    assert_eq!(names(&out), ["teleport_entity", "set_entity_motion"]);
    let mut motion = Writer::new();
    motion.varint(40).i16(800).i16(0).i16(0);
    assert_eq!(out[1].data, motion.buf);
    let empty = [40, 0];
    assert!(
        down()
            .clientbound(Play, "move_minecart_along_track", &empty)
            .unwrap()
            .is_empty()
    );
}

fn plain(count: i32, item: i32) -> Vec<u8> {
    let mut writer = Writer::new();
    super::stacks::write_plain(&mut writer, count, item);
    writer.into_inner()
}

#[test]
fn the_cursor_and_the_inventory_have_packets_of_their_own_on_1_21_2() {
    let (old_stone, new_stone) = (item(OLDER, "stone"), item(NEWER, "stone"));
    let slot = |window: i8, slot: i16| {
        let mut writer = Writer::new();
        writer
            .i8(window)
            .varint(5)
            .i16(slot)
            .bytes(&plain(3, old_stone));
        writer.into_inner()
    };
    let mut up = up();
    let out = up.one_clientbound(Play, "container_set_slot", &slot(-1, -1));
    assert_eq!(
        (out.name.as_str(), out.data),
        ("set_cursor_item", plain(3, new_stone))
    );
    let out = up.one_clientbound(Play, "container_set_slot", &slot(-2, 9));
    let mut inventory = vec![9];
    inventory.extend(plain(3, new_stone));
    assert_eq!(
        (out.name.as_str(), out.data),
        ("set_player_inventory", inventory.clone())
    );
    let out = up.one_clientbound(Play, "container_set_slot", &slot(3, 0));
    let mut set_slot = vec![3, 5, 0, 0];
    set_slot.extend(plain(3, new_stone));
    assert_eq!(out.data, set_slot);

    let mut down = down();
    join(&mut down, Some(63));
    let out = down.one_clientbound(Play, "set_cursor_item", &plain(3, new_stone));
    assert_eq!(out.name, "container_set_slot");
    let mut cursor = vec![0xff, 0, 0xff, 0xff];
    cursor.extend(plain(3, old_stone));
    assert_eq!(out.data, cursor);
    let out = down.one_clientbound(Play, "set_player_inventory", &inventory);
    let mut player = vec![0xfe, 0, 0, 9];
    player.extend(plain(3, old_stone));
    assert_eq!(out.data, player);
    let empty = [0];
    let out = down.one_clientbound(Play, "set_cursor_item", &empty);
    assert_eq!(out.data, [0xff, 0, 0xff, 0xff, 0]);
}

#[test]
fn armour_changes_behind_an_open_window_reach_a_1_21_client_as_equipment() {
    let new_helmet = item(NEWER, "iron_helmet");
    let slot = |slot: i16| {
        let mut writer = Writer::new();
        writer
            .varint(0)
            .varint(8)
            .i16(slot)
            .bytes(&plain(1, new_helmet));
        writer.into_inner()
    };
    let mut down = down();
    join(&mut down, Some(63));
    assert_eq!(
        down.clientbound(Play, "container_set_slot", &slot(5))
            .unwrap()
            .len(),
        1
    );
    let mut screen = Writer::new();
    screen.varint(4).varint(2).bytes(&[8, 0, 0]);
    down.clientbound(Play, "open_screen", &screen.buf).unwrap();
    let out = down
        .clientbound(Play, "container_set_slot", &slot(5))
        .unwrap();
    assert_eq!(names(&out), ["container_set_slot", "set_equipment"]);
    let mut equipment = vec![7, 5];
    equipment.extend(plain(1, item(OLDER, "iron_helmet")));
    assert_eq!(out[1].data, equipment);
    down.clientbound(Play, "container_close", &[4]).unwrap();
    assert_eq!(
        down.clientbound(Play, "container_set_slot", &slot(5))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn window_ids_are_varints_on_1_21_2() {
    let mut up = up();
    assert_eq!(
        up.one_clientbound(Play, "container_close", &[200]).data,
        [0xc8, 1]
    );
    assert_eq!(
        up.one_clientbound(Play, "container_set_data", &[7, 0, 1, 0, 2])
            .data,
        [7, 0, 1, 0, 2]
    );
    assert_eq!(
        up.one_clientbound(Play, "mount_screen_open", &[7, 5, 0, 0, 0, 9])
            .data,
        [7, 5, 0, 0, 0, 9]
    );
    assert_eq!(up.one_serverbound(Play, "container_close", &[7]).data, [7]);
    assert!(up.serverbound(Play, "container_close", &[0x80, 1]).is_err());
    let mut down = down();
    assert_eq!(
        down.one_clientbound(Play, "container_close", &[0xc8, 1])
            .data,
        [200]
    );
    assert!(
        down.clientbound(Play, "container_close", &[0x80, 2])
            .is_err()
    );
    assert_eq!(
        down.one_serverbound(Play, "container_close", &[0xff]).data,
        [0xff, 0xff, 0xff, 0xff, 0x0f]
    );
}

#[test]
fn movement_carries_flags_and_old_clients_end_their_ticks() {
    let mut up = up();
    let out = up.one_serverbound(Play, "move_player_rot", &hex("42b40000 00000000 03"));
    assert_eq!(out.data, hex("42b40000 00000000 01"));
    // Only landing or leaving the ground matters to 1.21.
    assert_eq!(
        up.serverbound(Play, "move_player_status_only", &[3])
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        up.one_serverbound(Play, "move_player_status_only", &[0])
            .data,
        [0]
    );
    assert!(
        up.serverbound(Play, "client_tick_end", &[])
            .unwrap()
            .is_empty()
    );
    assert!(
        up.serverbound(Play, "bundle_item_selected", &[3, 1])
            .unwrap()
            .is_empty()
    );

    let mut down = down();
    let out = down
        .serverbound(Play, "move_player_status_only", &[1])
        .unwrap();
    assert_eq!(names(&out), ["move_player_status_only", "client_tick_end"]);
    assert_eq!(out[0].data, [1]);
    assert!(
        down.serverbound(Play, "move_player_status_only", &[2])
            .is_err()
    );
}

#[test]
fn vehicle_input_is_a_key_set_on_1_21_2() {
    let mut down = down();
    let mut sprint = Writer::new();
    sprint.varint(7).varint(3).varint(0);
    down.serverbound(Play, "player_command", &sprint.buf)
        .unwrap();
    let mut steer = Writer::new();
    steer.f32(-0.98).f32(0.98).u8(3);
    let out = down.one_serverbound(Play, "player_input", &steer.buf);
    assert_eq!(out.data, [0x01 | 0x08 | 0x10 | 0x20 | 0x40]);

    let mut up = up();
    join(&mut up, None);
    // Not riding: a 1.21 server would ignore it.
    assert!(
        up.serverbound(Play, "player_input", &[0x01 | 0x04])
            .unwrap()
            .is_empty()
    );
    let mut passengers = Writer::new();
    passengers.varint(30).varint(1).varint(7);
    up.clientbound(Play, "set_passengers", &passengers.buf)
        .unwrap();
    let out = up.one_serverbound(Play, "player_input", &[0x01 | 0x04 | 0x20]);
    let mut steer = Writer::new();
    steer.f32(0.98).f32(0.98).u8(2);
    assert_eq!(out.data, steer.buf);
    // Riding, every turn repeats the keys.
    let out = up
        .serverbound(Play, "move_player_rot", &hex("00000000 00000000 00"))
        .unwrap();
    assert_eq!(names(&out), ["move_player_rot", "player_input"]);
    assert_eq!(out[1].data, steer.buf);
    up.clientbound(Play, "remove_entities", &[1, 30]).unwrap();
    let out = up
        .serverbound(Play, "move_player_rot", &hex("00000000 00000000 00"))
        .unwrap();
    assert_eq!(names(&out), ["move_player_rot"]);
}

#[test]
fn clicks_on_the_world_border_are_a_1_21_2_field() {
    let body = |border: Option<bool>| {
        let mut writer = Writer::new();
        writer
            .varint(0)
            .i64(0x1234)
            .varint(1)
            .f32(0.5)
            .f32(1.0)
            .f32(0.5)
            .bool(false);
        if let Some(border) = border {
            writer.bool(border);
        }
        writer.varint(33);
        writer.into_inner()
    };
    assert_eq!(
        up().one_serverbound(Play, "use_item_on", &body(Some(true)))
            .data,
        body(None)
    );
    assert_eq!(
        down()
            .one_serverbound(Play, "use_item_on", &body(None))
            .data,
        body(Some(false))
    );
}

#[test]
fn creative_destroy_all_is_echoed_to_a_1_21_client() {
    let mut down = down();
    let clear = |slot: i16| {
        let mut writer = Writer::new();
        writer.i16(slot).varint(0);
        writer.into_inner()
    };
    for slot in 0..45 {
        let out = down
            .serverbound(Play, "set_creative_mode_slot", &clear(slot))
            .unwrap();
        assert_eq!(out.len(), 1);
    }
    let out = down
        .serverbound(Play, "set_creative_mode_slot", &clear(45))
        .unwrap();
    assert_eq!(out.len(), 46);
    assert!(out[1..].iter().all(|out| out.name == "container_set_slot"));
    // A single cleared slot is not a sweep.
    assert_eq!(
        down.serverbound(Play, "set_creative_mode_slot", &clear(45))
            .unwrap()
            .len(),
        1
    );
}

fn recipe_packet(state: i32, names: &[&str], highlight: &[&str]) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.varint(state);
    for flag in [true, false, false, false, false, false, false, true] {
        writer.bool(flag);
    }
    for list in [names, highlight]
        .iter()
        .take(if state == 0 { 2 } else { 1 })
    {
        writer.varint(list.len() as i32);
        for name in *list {
            writer.string(name);
        }
    }
    writer.into_inner()
}

/// A tiny 1.21 recipe list: shaped sticks, smelted glass, a stonecutter
/// slab and a special recipe.
fn older_recipes() -> Vec<u8> {
    let serializer = |name| super::registry_id(OLDER, "recipe_serializer", name);
    let ingredient = |writer: &mut Writer, items: &[&str]| {
        writer.varint(items.len() as i32);
        for name in items {
            writer.bytes(&plain(1, item(OLDER, name)));
        }
    };
    let mut writer = Writer::new();
    writer.varint(4);
    writer
        .string("minecraft:stick")
        .varint(serializer("crafting_shaped"));
    writer.string("sticks").varint(3).varint(1).varint(2);
    ingredient(&mut writer, &["oak_planks", "birch_planks"]);
    ingredient(&mut writer, &["oak_planks", "birch_planks"]);
    writer.bytes(&plain(4, item(OLDER, "stick"))).bool(true);
    writer
        .string("minecraft:glass")
        .varint(serializer("smelting"));
    writer.string("").varint(1);
    ingredient(&mut writer, &["sand"]);
    writer
        .bytes(&plain(1, item(OLDER, "glass")))
        .f32(0.1)
        .varint(200);
    writer
        .string("minecraft:stone_slab_from_stone_stonecutting")
        .varint(serializer("stonecutting"));
    writer.string("");
    ingredient(&mut writer, &["stone"]);
    writer.bytes(&plain(2, item(OLDER, "stone_slab")));
    writer
        .string("minecraft:armor_dye")
        .varint(serializer("crafting_special_armordye"))
        .varint(3);
    writer.into_inner()
}

#[test]
fn the_recipe_book_goes_up_by_display_id() {
    let mut up = up();
    let out = up.one_clientbound(Play, "update_recipes", &older_recipes());
    let mut reader = Reader::new(&out.data);
    let mut sets = std::collections::HashMap::new();
    for _ in 0..reader.varint().unwrap() {
        let key = reader.str().unwrap().to_owned();
        let items: Vec<i32> = (0..reader.varint().unwrap())
            .map(|_| reader.varint().unwrap())
            .collect();
        sets.insert(key, items);
    }
    assert_eq!(sets["minecraft:furnace_input"], [item(NEWER, "sand")]);
    assert!(sets["minecraft:smithing_base"].is_empty());
    assert_eq!(reader.varint().unwrap(), 1); // One stonecutter recipe.
    assert_eq!(reader.varint().unwrap(), 2); // Holder set of one item.
    assert_eq!(reader.varint().unwrap(), item(NEWER, "stone"));
    reader.varint().unwrap(); // Item stack display.
    assert_eq!(reader.rest(), plain(2, item(NEWER, "stone_slab")));

    let init = recipe_packet(
        0,
        &[
            "minecraft:stick",
            "glass",
            "minecraft:armor_dye",
            "minecraft:unknown",
        ],
        &["minecraft:glass"],
    );
    let out = up.clientbound(Play, "recipe", &init).unwrap();
    assert_eq!(names(&out), ["recipe_book_settings", "recipe_book_add"]);
    assert_eq!(out[0].data, [1, 0, 0, 0, 0, 0, 0, 1]);
    let mut reader = Reader::new(&out[1].data);
    assert_eq!(reader.varint().unwrap(), 2);
    assert_eq!(reader.varint().unwrap(), 0); // The sticks' display ID.
    assert_eq!(*out[1].data.last().unwrap(), 1); // Replace.

    let place = [3, 1, 0]; // Window 3, display 1 (glass), not all.
    let out = up.one_serverbound(Play, "place_recipe", &place);
    let mut glass = Writer::new();
    glass.i8(3).string("minecraft:glass").bool(false);
    assert_eq!(out.data, glass.buf);
    assert!(
        up.serverbound(Play, "place_recipe", &[3, 9, 0])
            .unwrap()
            .is_empty()
    );
    let out = up.one_serverbound(Play, "recipe_book_seen_recipe", &[0]);
    let mut stick = Writer::new();
    stick.string("minecraft:stick");
    assert_eq!(out.data, stick.buf);
    let mut ghost = Writer::new();
    ghost.i8(3).string("minecraft:stick");
    let out = up.one_clientbound(Play, "place_ghost_recipe", &ghost.buf);
    assert_eq!(out.data[0], 3);

    let remove = recipe_packet(2, &["minecraft:glass"], &[]);
    let out = up.clientbound(Play, "recipe", &remove).unwrap();
    assert_eq!(names(&out), ["recipe_book_settings", "recipe_book_remove"]);
    assert_eq!(out[1].data, [1, 1]);
}

#[test]
fn the_recipe_book_comes_down_as_recipes_the_client_knows() {
    // What the up direction makes of a 1.21 book is a valid 1.21.2 book:
    // send it down again and the recipes come back under display IDs.
    let mut up = up();
    let sets = up.one_clientbound(Play, "update_recipes", &older_recipes());
    let init = recipe_packet(
        0,
        &["minecraft:stick", "minecraft:glass"],
        &["minecraft:glass"],
    );
    let book = up.clientbound(Play, "recipe", &init).unwrap();

    let mut down = down();
    let out = down
        .clientbound(Play, "update_recipes", &sets.data)
        .unwrap();
    assert_eq!(names(&out), ["update_recipes"]);
    assert!(
        down.clientbound(Play, "recipe_book_settings", &book[0].data)
            .unwrap()
            .is_empty()
    );
    let out = down
        .clientbound(Play, "recipe_book_add", &book[1].data)
        .unwrap();
    assert_eq!(names(&out), ["update_recipes", "recipe"]);
    let mut reader = Reader::new(&out[0].data);
    assert_eq!(reader.varint().unwrap(), 3);
    assert_eq!(reader.str().unwrap(), "rift:recipe/0");
    assert_eq!(
        reader.varint().unwrap(),
        super::registry_id(OLDER, "recipe_serializer", "crafting_shaped")
    );
    let expected = recipe_packet(0, &["rift:recipe/0", "rift:recipe/1"], &["rift:recipe/1"]);
    assert_eq!(out[1].data, expected);

    let mut place = Writer::new();
    place.i8(2).string("rift:recipe/1").bool(true);
    assert_eq!(
        down.one_serverbound(Play, "place_recipe", &place.buf).data,
        [2, 1, 1]
    );
    let mut foreign = Writer::new();
    foreign.i8(2).string("minecraft:glass").bool(true);
    assert!(
        down.serverbound(Play, "place_recipe", &foreign.buf)
            .unwrap()
            .is_empty()
    );

    let out = down.one_clientbound(Play, "recipe_book_remove", &[1, 0]);
    assert_eq!(out.name, "recipe");
    assert_eq!(out.data, recipe_packet(2, &["rift:recipe/0"], &[]));
}

/// Errors that are the item codec's: stacks with components.
fn codec_pending(error: &std::io::Error) -> bool {
    error.to_string().contains("not implemented")
}

#[test]
fn recorded_sessions_convert_through_both_steps() {
    let own = [
        "login_finished",
        "login",
        "respawn",
        "set_time",
        "player_position",
        "teleport_entity",
        "entity_position_sync",
        "update_recipes",
        "recipe",
        "recipe_book_add",
        "recipe_book_remove",
        "recipe_book_settings",
        "container_set_slot",
        "container_close",
        "update_enabled_features",
    ];
    for (protocol, mut harness) in [(OLDER, up()), (NEWER, down())] {
        let Some(recorded) = corpus(protocol) else {
            continue;
        };
        let (mut converted, mut pending) = (0, 0);
        for packet in recorded
            .iter()
            .filter(|packet| packet.direction == Direction::Clientbound)
        {
            let name = packet.name(protocol);
            let result = harness.send_id(packet.direction, packet.state, packet.id, &packet.data);
            if !own.contains(&name) {
                continue;
            }
            match result {
                Ok(_) => converted += 1,
                Err(error) if codec_pending(&error) => pending += 1,
                Err(error) => panic!("{protocol} {name}: {error}"),
            }
        }
        eprintln!("{protocol}: {converted} converted, {pending} wait for the item codec");
        assert!(converted > 30, "{protocol}: only {converted}");
    }
}

#[test]
fn the_recorded_1_21_recipes_go_up_and_down_again() {
    let Some(recorded) = corpus(OLDER) else {
        return;
    };
    let find = |name: &str| {
        recorded
            .iter()
            .find(|packet| packet.name(OLDER) == name)
            .unwrap_or_else(|| panic!("the session has {name}"))
    };
    let mut up = up();
    let sets = up.one_clientbound(Play, "update_recipes", &find("update_recipes").data);
    let book = up
        .clientbound(Play, "recipe", &find("recipe").data)
        .unwrap();
    let mut down = down();
    let out = down
        .clientbound(Play, "update_recipes", &sets.data)
        .unwrap();
    let mut reader = Reader::new(&out[0].data);
    let stonecutter = reader.varint().unwrap();
    assert!(stonecutter > 200, "{stonecutter} stonecutter recipes");
    down.clientbound(Play, "recipe_book_settings", &book[0].data)
        .unwrap();
    let out = down
        .clientbound(Play, "recipe_book_add", &book[1].data)
        .unwrap();
    assert_eq!(names(&out), ["update_recipes", "recipe"]);
}

#[test]
fn the_recorded_1_21_2_book_comes_down() {
    let Some(recorded) = corpus(NEWER) else {
        return;
    };
    let mut down = down();
    let mut unlocked = 0;
    for packet in &recorded {
        let name = packet.name(NEWER);
        if !matches!(
            name,
            "update_tags" | "update_recipes" | "recipe_book_settings" | "recipe_book_add"
        ) {
            continue;
        }
        match down.send_id(packet.direction, packet.state, packet.id, &packet.data) {
            Ok(out) => unlocked += out.iter().filter(|out| out.name == "recipe").count(),
            Err(error) if codec_pending(&error) => {}
            Err(error) => panic!("{name}: {error}"),
        }
    }
    assert!(unlocked > 20, "{unlocked}");
}

#[test]
fn malformed_packets_fail_without_panicking() {
    let mut up = up();
    let mut down = down();
    for name in [
        "login",
        "respawn",
        "set_time",
        "player_position",
        "teleport_entity",
        "update_recipes",
        "recipe",
    ] {
        for body in [
            &[][..],
            &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff][..],
            &[5, 1, 2][..],
        ] {
            assert!(
                up.clientbound(Play, name, body).is_err(),
                "up {name} {body:?}"
            );
        }
    }
    for name in [
        "login",
        "respawn",
        "set_time",
        "player_position",
        "teleport_entity",
        "entity_position_sync",
        "player_rotation",
        "move_minecart_along_track",
        "recipe_book_add",
        "recipe_book_remove",
        "update_recipes",
        "place_ghost_recipe",
        "set_player_inventory",
    ] {
        for body in [&[][..], &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff][..]] {
            assert!(
                down.clientbound(Play, name, body).is_err(),
                "down {name} {body:?}"
            );
        }
    }
    // A slot display nested too deep is refused, not followed.
    let composite = super::registry_id(NEWER, "slot_display", "composite") as u8;
    let mut deep = vec![
        1,
        0,
        super::registry_id(NEWER, "recipe_display", "stonecutter") as u8,
    ];
    deep.extend(std::iter::repeat_n([composite, 1], 200).flatten());
    assert!(down.clientbound(Play, "recipe_book_add", &deep).is_err());
    assert!(up.serverbound(Play, "use_item_on", &[0, 1]).is_err());
    assert!(down.serverbound(Play, "player_input", &[1, 2, 3]).is_err());
}

#[test]
fn recipes_unlocked_later_reach_a_1_21_client_with_the_whole_list() {
    let mut up = up();
    let sets = up.one_clientbound(Play, "update_recipes", &older_recipes());
    let init = up
        .clientbound(Play, "recipe", &recipe_packet(0, &["minecraft:stick"], &[]))
        .unwrap();
    let add = up
        .clientbound(Play, "recipe", &recipe_packet(1, &["minecraft:glass"], &[]))
        .unwrap();
    // Unlocked later: highlighted, with a toast.
    assert_eq!(*add[1].data.last().unwrap(), 0); // Not a replacement.
    assert_eq!(add[1].data[add[1].data.len() - 2], 3);

    let mut down = down();
    down.clientbound(Play, "update_recipes", &sets.data)
        .unwrap();
    down.clientbound(Play, "recipe_book_settings", &init[0].data)
        .unwrap();
    down.clientbound(Play, "recipe_book_add", &init[1].data)
        .unwrap();
    let out = down
        .clientbound(Play, "recipe_book_add", &add[1].data)
        .unwrap();
    assert_eq!(names(&out), ["update_recipes", "recipe"]);
    // The client keeps both book recipes and the stonecutter's.
    assert_eq!(Reader::new(&out[0].data).varint().unwrap(), 3);
    assert_eq!(out[1].data, recipe_packet(1, &["rift:recipe/1"], &[]));

    // A ghost recipe is recognised by its display.
    let ghost = up.one_clientbound(Play, "place_ghost_recipe", &{
        let mut writer = Writer::new();
        writer.i8(2).string("minecraft:glass");
        writer.into_inner()
    });
    let out = down.one_clientbound(Play, "place_ghost_recipe", &ghost.data);
    let mut expected = Writer::new();
    expected.i8(2).string("rift:recipe/1");
    assert_eq!(out.data, expected.buf);
    let mut unknown = ghost.data.clone();
    unknown.push(0);
    assert!(
        down.clientbound(Play, "place_ghost_recipe", &unknown)
            .unwrap()
            .is_empty()
    );
}
