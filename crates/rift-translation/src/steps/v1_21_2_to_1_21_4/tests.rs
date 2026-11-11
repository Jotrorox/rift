use super::{
    NEWER, OLDER, down,
    harness::{Harness, hex, recorded, single},
    up,
};
use crate::{Direction, State, wire::Writer};

fn up() -> Harness {
    Harness::new(super::UP.unwrap()(), NEWER, OLDER)
}

fn down() -> Harness {
    Harness::new(super::DOWN.unwrap()(), OLDER, NEWER)
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(up::Up::routes().problems(), &[] as &[String]);
    assert_eq!(down::Down::routes().problems(), &[] as &[String]);
}

#[test]
fn held_slot_is_a_byte_before_1_21_4_and_a_varint_after() {
    // Real `set_held_slot` bodies: slot 0 on both servers.
    assert_eq!(
        single(
            &up()
                .clientbound(State::Play, "set_held_slot", &[0])
                .unwrap(),
            "set_held_slot"
        ),
        [0]
    );
    assert_eq!(
        single(
            &up()
                .clientbound(State::Play, "set_held_slot", &[0xff])
                .unwrap(),
            "set_held_slot"
        ),
        hex("ffffffff0f")
    );
    let mut step = down();
    assert_eq!(
        single(
            &step
                .clientbound(State::Play, "set_held_slot", &[8])
                .unwrap(),
            "set_held_slot"
        ),
        [8]
    );
    assert!(
        step.clientbound(State::Play, "set_held_slot", &hex("8001"))
            .is_err()
    );
    assert!(step.clientbound(State::Play, "set_held_slot", &[]).is_err());
    assert!(
        step.clientbound(State::Play, "set_held_slot", &[1, 2])
            .is_err()
    );
}

#[test]
fn winter_drop_is_enabled_for_1_21_2_and_stripped_for_1_21_4() {
    // What both servers send: only `minecraft:vanilla`.
    let vanilla = hex("01116d696e6563726166743a76616e696c6c61");
    let mut both = Writer::new();
    both.varint(2)
        .string("minecraft:vanilla")
        .string("minecraft:winter_drop");
    let out = down()
        .clientbound(State::Configuration, "update_enabled_features", &vanilla)
        .unwrap();
    assert_eq!(single(&out, "update_enabled_features"), both.buf);
    let out = up()
        .clientbound(State::Configuration, "update_enabled_features", &both.buf)
        .unwrap();
    assert_eq!(single(&out, "update_enabled_features"), vanilla);
    assert!(
        up().clientbound(State::Configuration, "update_enabled_features", &hex("05"))
            .is_err()
    );
}

#[test]
fn vehicle_moves_gain_and_lose_the_on_ground_flag() {
    let mut old = Writer::new();
    old.f64(1.5).f64(64.0).f64(-3.25).f32(90.0).f32(10.0);
    let mut new = old.clone();
    new.bool(true);
    let out = down()
        .serverbound(State::Play, "move_vehicle", &old.buf)
        .unwrap();
    assert_eq!(single(&out, "move_vehicle"), new.buf);
    let out = up()
        .serverbound(State::Play, "move_vehicle", &new.buf)
        .unwrap();
    assert_eq!(single(&out, "move_vehicle"), old.buf);
    assert!(
        up().serverbound(State::Play, "move_vehicle", &old.buf)
            .is_err()
    );
    let mut bad = old.clone();
    bad.u8(2);
    assert!(
        up().serverbound(State::Play, "move_vehicle", &bad.buf)
            .is_err()
    );
}

#[test]
fn a_1_21_2_client_reports_loading_after_join_and_respawn() {
    let mut step = down();
    // The real 1.21.4 server's `login`.
    let login = hex(concat!(
        "000000010003136d696e6563726166743a6f766572776f726c64146d696e6563726166743a7468655f6e6574",
        "686572116d696e6563726166743a7468655f656e6414020200010000136d696e6563726166743a6f76657277",
        "6f726c64f6785071858e05aa01ff00010000c1ffffff0f00",
    ));
    let out = step.clientbound(State::Play, "login", &login).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!((out[0].name.as_str(), &out[0].data), ("login", &login));
    assert_eq!(out[1].direction, Direction::Serverbound);
    assert_eq!(
        (out[1].name.as_str(), out[1].data.len()),
        ("player_loaded", 0)
    );
    // Back to the overworld with the login's spawn details, keeping nothing.
    let respawn = hex(concat!(
        "00136d696e6563726166743a6f766572776f726c64f6785071858e05aa01ff00010000c1ffffff0f",
        "00"
    ));
    let out = step.clientbound(State::Play, "respawn", &respawn).unwrap();
    assert_eq!((out[0].name.as_str(), &out[0].data), ("respawn", &respawn));
    assert_eq!(out[1].name, "player_loaded");
    // A 1.21.4 client says so itself; a 1.21.2 server has no such packet.
    assert!(
        up().serverbound(State::Play, "player_loaded", &[])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn picking_from_the_inventory_swaps_into_the_selected_hotbar_slot() {
    let mut step = down();
    // Inventory menu at state 7 (container 0), then another menu at state 9.
    step.clientbound(State::Play, "container_set_slot", &hex("0007000a00"))
        .unwrap();
    step.clientbound(State::Play, "container_set_slot", &hex("0209000100"))
        .unwrap();
    step.serverbound(State::Play, "set_carried_item", &hex("0003"))
        .unwrap();
    let out = step.serverbound(State::Play, "pick_item", &[20]).unwrap();
    // Container 0, state 7, slot 20, button 3, swap, no changes, empty cursor.
    assert_eq!(single(&out, "container_click"), hex("0007001403020000"));
    // The server moving the selection is followed too.
    step.clientbound(State::Play, "set_held_slot", &[5])
        .unwrap();
    let out = step.serverbound(State::Play, "pick_item", &[35]).unwrap();
    assert_eq!(single(&out, "container_click"), hex("0007002305020000"));
    // Hotbar slots are the client's to select; nonsense is ignored.
    assert!(
        step.serverbound(State::Play, "pick_item", &[4])
            .unwrap()
            .is_empty()
    );
    assert!(
        step.serverbound(State::Play, "pick_item", &[36])
            .unwrap()
            .is_empty()
    );
    assert!(
        step.serverbound(State::Play, "pick_item", &[20, 0])
            .is_err()
    );
    // A 1.21.2 server cannot resolve a block or entity to pick.
    assert!(
        up().serverbound(State::Play, "pick_item_from_block", &[0; 9])
            .unwrap()
            .is_empty()
    );
}

/// Every recorded packet the step handles converts, and a converted held
/// slot or feature list is what the other version's own server sends.
#[test]
fn recorded_packets_convert_to_what_the_other_server_sends() {
    for (protocol, mut step, other) in [(OLDER, up(), NEWER), (NEWER, down(), OLDER)] {
        for (state, name) in [
            (State::Play, "set_held_slot"),
            (State::Configuration, "update_enabled_features"),
        ] {
            let (Some(sent), Some(expected)) = (
                recorded(protocol, Direction::Clientbound, state, name),
                recorded(other, Direction::Clientbound, state, name),
            ) else {
                return;
            };
            assert_eq!(sent.len(), 1, "{protocol} {name}");
            let out = step.replay(&sent[0]).unwrap();
            let converted = single(&out, name);
            if name == "update_enabled_features" && other == OLDER {
                // The 1.21.2 server's list plus the flag the step adds.
                let mut flagged = expected[0].data.clone();
                flagged[0] += 1;
                let mut writer = Writer { buf: flagged };
                writer.string("minecraft:winter_drop");
                assert_eq!(converted, writer.buf);
            } else {
                assert_eq!(converted, expected[0].data, "{protocol} {name}");
            }
        }
        // Recorded logins pass unchanged (and gain `player_loaded` going down).
        let Some(logins) = recorded(protocol, Direction::Clientbound, State::Play, "login") else {
            return;
        };
        let out = step.replay(&logins[0]).unwrap();
        assert_eq!(out[0].data, logins[0].data);
        assert_eq!(out.len(), if protocol == NEWER { 2 } else { 1 });
    }
}

/// Whole recorded sessions of both versions cross the step in both
/// orientations. Packets the shared codecs own pass through unconverted
/// until the codecs convert them; nothing may fail.
#[test]
fn recorded_sessions_cross_the_step_both_ways() {
    use crate::steps::v1_21_2_to_1_21_4::harness::cross_sessions;
    for harness in [up(), down()] {
        if let Some(count) = cross_sessions(harness) {
            assert!(count > 500, "{count} packets");
        }
    }
}
