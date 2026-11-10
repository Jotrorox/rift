use super::{NEWER, OLDER, Text, down, up};
use crate::{
    Direction, State,
    nbt::Nbt,
    steps::v1_21_2_to_1_21_4::harness::{Harness, hex, recorded, single},
    wire::{Rewriter, Writer},
};
use std::io;

fn up() -> Harness {
    Harness::new(super::UP.unwrap()(), NEWER, OLDER)
}

fn down() -> Harness {
    Harness::new(super::DOWN.unwrap()(), OLDER, NEWER)
}

/// Text that is the same in both protocols: components copied, styles kept.
fn verbatim<T>(body: impl FnOnce(&mut Text<'_>) -> io::Result<T>) -> io::Result<T> {
    let mut component = |rw: &mut Rewriter<'_>| rw.nbt(false).map(drop);
    let mut style = |_: &mut Nbt| Ok(());
    body(&mut Text {
        component: &mut component,
        style: &mut style,
    })
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(up::Up::routes().problems(), &[] as &[String]);
    assert_eq!(down::Down::routes().problems(), &[] as &[String]);
}

// A real orb worth 5 at (-4.5, -60, 5.5) from the 1.21.4 server, and the
// same orb as the 1.21.5 server spawns it (UUID aside) with its value.
const ORB_1_21_4: &str = "1cc012000000000000c04e00000000000040160000000000000005";
const ORB_POSITION: &str = "c012000000000000c04e0000000000004016000000000000";

#[test]
fn orbs_spawn_as_entities_with_their_value_in_entity_data() {
    let out = up()
        .clientbound(State::Play, "add_experience_orb", &hex(ORB_1_21_4))
        .unwrap();
    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!(out[0].name, "add_entity");
    let mut spawn = hex("1c");
    spawn.extend_from_slice(&super::orb_uuid(0x1c).to_be_bytes());
    spawn.extend(hex(&format!("2f{ORB_POSITION}00000000000000000000")));
    assert_eq!(out[0].data, spawn);
    // What the 1.21.5 server sends for an orb worth 5.
    assert_eq!(
        (out[1].name.as_str(), out[1].data.clone()),
        ("set_entity_data", hex("1c080105ff"))
    );
    assert!(
        up().clientbound(State::Play, "add_experience_orb", &hex("1c00"))
            .is_err()
    );
}

#[test]
fn orb_entities_go_back_to_their_own_packet() {
    // A real 1.21.5 orb spawn.
    let spawn = hex(
        "1ca91d545a89e94be487e74da4b90a46cb2f3fe0000000000000c04e000000000000c00400000000000000000000000000000000",
    );
    let out = down()
        .clientbound(State::Play, "add_entity", &spawn)
        .unwrap();
    assert_eq!(
        single(&out, "add_experience_orb"),
        hex("1c3fe0000000000000c04e000000000000c0040000000000000000")
    );
    // A moving orb also gets its motion; a value given as data is kept.
    let mut moving = spawn.clone();
    let length = moving.len();
    moving[length - 7] = 7;
    moving[length - 1] = 9;
    let out = down()
        .clientbound(State::Play, "add_entity", &moving)
        .unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].data[out[0].data.len() - 2..], [0, 7]);
    assert_eq!(
        (out[1].name.as_str(), out[1].data.clone()),
        ("set_entity_motion", hex("1c000000000009"))
    );
    // Other entities are left to the entity codec.
    let mut zombie = spawn.clone();
    zombie[17] = 0x7c;
    let out = down()
        .clientbound(State::Play, "add_entity", &zombie)
        .unwrap();
    assert_eq!(single(&out, "add_entity"), zombie);
    assert!(
        down()
            .clientbound(State::Play, "add_entity", &spawn[..30])
            .is_err()
    );
}

fn chat_message(checksum: Option<u8>) -> Vec<u8> {
    let mut chat = Writer::new();
    chat.string("hello")
        .i64(1_700_000_000_000)
        .i64(42)
        .bool(true);
    chat.bytes(&[0xab; 256]).varint(3).bytes(&[1, 0, 0]);
    if let Some(checksum) = checksum {
        chat.u8(checksum);
    }
    chat.buf
}

fn signed_command(checksum: Option<u8>) -> Vec<u8> {
    let mut command = Writer::new();
    command.string("msg Steve hi").i64(5).i64(6).varint(1);
    command
        .string("message")
        .bytes(&[0xcd; 256])
        .varint(0)
        .bytes(&[0, 0, 0]);
    if let Some(checksum) = checksum {
        command.u8(checksum);
    }
    command.buf
}

#[test]
fn chat_gains_an_unchecked_checksum_and_loses_it_again() {
    let out = down()
        .serverbound(State::Play, "chat", &chat_message(None))
        .unwrap();
    assert_eq!(single(&out, "chat"), chat_message(Some(0)));
    let out = up()
        .serverbound(State::Play, "chat", &chat_message(Some(0x5a)))
        .unwrap();
    assert_eq!(single(&out, "chat"), chat_message(None));
    let out = down()
        .serverbound(State::Play, "chat_command_signed", &signed_command(None))
        .unwrap();
    assert_eq!(single(&out, "chat_command_signed"), signed_command(Some(0)));
    let out = up()
        .serverbound(State::Play, "chat_command_signed", &signed_command(Some(7)))
        .unwrap();
    assert_eq!(single(&out, "chat_command_signed"), signed_command(None));
    // Without the checksum a 1.21.5 message is short; with one a 1.21.4
    // message is too long.
    assert!(
        up().serverbound(State::Play, "chat", &chat_message(None))
            .is_err()
    );
    assert!(
        down()
            .serverbound(State::Play, "chat", &chat_message(Some(0)))
            .is_err()
    );
    assert!(up().serverbound(State::Play, "chat", &[0xff; 4]).is_err());
}

fn player_chat(global_index: Option<i32>, inline_type: bool) -> Vec<u8> {
    let mut chat = Writer::new();
    if let Some(index) = global_index {
        chat.varint(index);
    }
    chat.uuid(0x1234).varint(4).bool(true).bytes(&[0xee; 256]);
    chat.string("hi")
        .i64(1)
        .i64(2)
        .varint(2)
        .varint(5)
        .varint(0)
        .bytes(&[0x11; 256]);
    chat.bool(true)
        .bytes(&Nbt::from("unsigned").to_bytes(false));
    chat.varint(2).varint(1).i64(-1);
    if inline_type {
        let style = Nbt::Compound(crate::nbt::Compound::new()).to_bytes(false);
        chat.varint(0);
        chat.string("chat.type.text")
            .varint(2)
            .varint(0)
            .varint(2)
            .bytes(&style);
        chat.string("chat.type.text.narrate")
            .varint(0)
            .bytes(&style);
    } else {
        chat.varint(1);
    }
    chat.bytes(&Nbt::from("Steve").to_bytes(false)).bool(false);
    chat.buf
}

#[test]
fn player_chat_counts_messages_for_1_21_5_clients() {
    for inline in [false, true] {
        let converted =
            verbatim(|text| super::player_chat(&player_chat(None, inline), Some(9), text));
        assert_eq!(converted.unwrap(), player_chat(Some(9), inline));
        let converted =
            verbatim(|text| super::player_chat(&player_chat(Some(300), inline), None, text));
        assert_eq!(converted.unwrap(), player_chat(None, inline));
    }
    let mut bad = player_chat(None, false);
    bad.push(0);
    assert!(verbatim(|text| super::player_chat(&bad, Some(0), text)).is_err());
    assert!(verbatim(|text| super::player_chat(&bad[..100], Some(0), text)).is_err());
}

#[test]
fn shield_events_become_the_sounds_1_21_4_clients_played() {
    let mut step = up();
    let out = step
        .clientbound(State::Play, "entity_event", &hex("000000051d"))
        .unwrap();
    let body = single(&out, "sound_entity");
    let shield_block = super::registry_id(NEWER, "sound_event", "item.shield.block");
    let mut expected = Writer::new();
    expected
        .varint(shield_block + 1)
        .varint(7)
        .varint(5)
        .f32(1.0);
    assert_eq!(body[..expected.buf.len()], expected.buf);
    let pitch = f32::from_be_bytes(body[expected.buf.len()..][..4].try_into().unwrap());
    assert!((0.8..=1.2).contains(&pitch), "{pitch}");
    let out = step
        .clientbound(State::Play, "entity_event", &hex("000000051e"))
        .unwrap();
    assert_eq!(out[0].name, "sound_entity");
    // A real event (24: operator level) passes untouched.
    let out = step
        .clientbound(State::Play, "entity_event", &hex("0000000118"))
        .unwrap();
    assert_eq!(single(&out, "entity_event"), hex("0000000118"));
    assert!(
        step.clientbound(State::Play, "entity_event", &hex("00000001"))
            .is_err()
    );
}

#[test]
fn test_blocks_do_not_cross_into_1_21_4() {
    assert!(
        up().serverbound(State::Play, "set_test_block", &[0; 10])
            .unwrap()
            .is_empty()
    );
    assert!(
        up().serverbound(State::Play, "test_instance_block_action", &[0; 10])
            .unwrap()
            .is_empty()
    );
    assert!(
        down()
            .clientbound(State::Play, "test_instance_block_status", &[0; 3])
            .unwrap()
            .is_empty()
    );
}

/// Recorded orbs convert to what the other server sends for them.
#[test]
fn recorded_orbs_convert_to_what_the_other_server_sends() {
    let Some(orbs) = recorded(
        OLDER,
        Direction::Clientbound,
        State::Play,
        "add_experience_orb",
    ) else {
        return;
    };
    for orb in &orbs {
        let out = up().replay(orb).unwrap();
        assert_eq!(out[0].name, "add_entity");
        assert_eq!(out[1].data, hex("1c080105ff"));
    }
    let Some(spawns) = recorded(NEWER, Direction::Clientbound, State::Play, "add_entity") else {
        return;
    };
    let mut orbs = 0;
    for spawn in &spawns {
        let out = down().replay(spawn).unwrap();
        if out[0].name == "add_experience_orb" {
            orbs += 1;
        } else {
            assert_eq!(single(&out, "add_entity"), spawn.data);
        }
    }
    assert_eq!(orbs, 1);
}
