use super::dimension::RECORDED_CODEC;
use super::{NEWER, OLDER, down, format_uuid, legacy_attribute, legacy_map_color, parse_uuid, up};
use crate::{
    Direction, State, data,
    steps::v1_14_4_to_1_15::support::harness::{
        Harness, body, hex, replay, truncations_fail, unimplemented,
    },
    wire::Writer,
};

fn upgrading() -> Harness {
    Harness::new((up::STEP.unwrap())(), NEWER, OLDER)
}

fn downgrading() -> Harness {
    Harness::new((down::STEP.unwrap())(), OLDER, NEWER)
}

// Recorded from the official servers: Login Success and `login`.
const LOGIN_FINISHED_1_15_2: &str =
    "24 38383862363133302d383233622d333837622d623031322d643664633234653437643539 06436f72707573";
const LOGIN_FINISHED_1_16: &str = "888b6130823b387bb012d6dc24e47d59 06436f72707573";
const LOGIN_1_15_2: &str = "00000001 01 00000000 c7feab42c557da51 14 04666c6174 02 00 01";
const OVERWORLD: &str = "136d696e6563726166743a6f766572776f726c64";
const WORLDS: &str = "03 136d696e6563726166743a6f766572776f726c64 146d696e6563726166743a7468655f6e6574686572 116d696e6563726166743a7468655f656e64";

/// The 1.16 server's `login` with the given seed: everything but the codec
/// is inline, the codec is the recorded one.
fn login_1_16(seed: &str) -> Vec<u8> {
    let mut packet = hex(&format!("00000001 01 ff {WORLDS}"));
    packet.extend_from_slice(RECORDED_CODEC);
    packet.extend(hex(&format!(
        "{OVERWORLD} {OVERWORLD} {seed} 14 02 00 01 00 01"
    )));
    packet
}

fn respawn_1_16(dimension: &str, world: &str, keep: bool) -> Vec<u8> {
    body(|w| {
        w.string(dimension)
            .string(world)
            .i64(7)
            .u8(1)
            .u8(0xff)
            .bool(false)
            .bool(false)
            .bool(keep);
    })
}

fn respawn_1_15(dimension: i32, level_type: &str) -> Vec<u8> {
    body(|w| {
        w.i32(dimension).i64(7).u8(1).string(level_type);
    })
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(up::Up::routes().problems(), &[] as &[String]);
    assert_eq!(down::Down::routes().problems(), &[] as &[String]);
}

#[test]
fn uuids_and_attribute_names_convert() {
    let uuid = 0x888b6130_823b_387b_b012_d6dc24e47d59;
    assert_eq!(format_uuid(uuid), "888b6130-823b-387b-b012-d6dc24e47d59");
    assert_eq!(
        parse_uuid("888b6130-823b-387b-b012-d6dc24e47d59").unwrap(),
        uuid
    );
    assert_eq!(
        parse_uuid("888b6130823b387bb012d6dc24e47d59").unwrap(),
        uuid
    );
    for bad in [
        "",
        "888b6130-823b-387b-b012-d6dc24e47d5",
        "888b6130+823b-387b-b012-d6dc24e47d59",
        "+88b6130823b387bb012d6dc24e47d59",
    ] {
        assert!(parse_uuid(bad).is_err(), "{bad}");
    }
    assert_eq!(
        legacy_attribute("minecraft:generic.max_health"),
        "generic.maxHealth"
    );
    assert_eq!(
        legacy_attribute("zombie.spawn_reinforcements"),
        "zombie.spawnReinforcements"
    );
    assert_eq!(legacy_attribute("generic.luck"), "generic.luck");
}

#[test]
fn login_success_uuid_changes_form() {
    let sent = Harness::new((up::STEP.unwrap())(), NEWER, OLDER)
        .send(
            Direction::Clientbound,
            State::Login,
            "login_finished",
            &hex(LOGIN_FINISHED_1_15_2),
        )
        .unwrap();
    assert_eq!(sent[0].data, hex(LOGIN_FINISHED_1_16));
    let sent = downgrading()
        .send(
            Direction::Clientbound,
            State::Login,
            "login_finished",
            &hex(LOGIN_FINISHED_1_16),
        )
        .unwrap();
    assert_eq!(
        (sent[0].state, &sent[0].data),
        (State::Login, &hex(LOGIN_FINISHED_1_15_2))
    );
}

#[test]
fn the_registries_codec_has_the_recorded_vanilla_codec() {
    let codec = crate::codec::registries::login::default_codec(NEWER).unwrap();
    assert_eq!(codec.to_bytes(true), RECORDED_CODEC);
}

#[test]
fn login_converts_like_the_servers_send_it() {
    // A 1.15.2 login becomes exactly what the 1.16 server sent, but for the
    // seed (the two sessions had different worlds).
    let sent = upgrading().one(Direction::Clientbound, "login", &hex(LOGIN_1_15_2), "login");
    assert_eq!(sent, login_1_16("c7feab42c557da51"));
    let sent = downgrading().one(
        Direction::Clientbound,
        "login",
        &login_1_16("6de9aae76afbfcd0"),
        "login",
    );
    assert_eq!(
        sent,
        hex("00000001 01 00000000 6de9aae76afbfcd0 14 04666c6174 02 00 01")
    );
}

#[test]
fn respawn_converts_dimensions_and_flags() {
    let sent = upgrading().one(
        Direction::Clientbound,
        "respawn",
        &respawn_1_15(-1, "default"),
        "respawn",
    );
    assert_eq!(
        sent,
        body(|w| {
            w.string("minecraft:the_nether")
                .string("minecraft:the_nether")
                .i64(7)
                .u8(1)
                .u8(0xff)
                .bool(false)
                .bool(false)
                .bool(true);
        })
    );
    let sent = upgrading().one(
        Direction::Clientbound,
        "respawn",
        &respawn_1_15(1, "FLAT"),
        "respawn",
    );
    assert_eq!(&sent[sent.len() - 3..], [0, 1, 1]);

    let mut step = downgrading();
    step.clientbound("login", &login_1_16("0000000000000000"))
        .unwrap();
    let sent = step
        .clientbound(
            "respawn",
            &respawn_1_16("minecraft:the_end", "minecraft:the_end", false),
        )
        .unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].data, respawn_1_15(1, "default"));
    // Another world of the same dimension: through the overworld first.
    let sent = step
        .clientbound(
            "respawn",
            &respawn_1_16("minecraft:the_end", "rift:arena", false),
        )
        .unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].data, respawn_1_15(0, "default"));
    assert_eq!(sent[1].data, respawn_1_15(1, "default"));
    // The same world again (a death): no detour.
    let sent = step
        .clientbound(
            "respawn",
            &respawn_1_16("minecraft:the_end", "rift:arena", false),
        )
        .unwrap();
    assert_eq!(sent.len(), 1);
    // Unknown types count as the overworld; caves are the overworld.
    let sent = step
        .clientbound(
            "respawn",
            &respawn_1_16("minecraft:overworld_caves", "minecraft:overworld", false),
        )
        .unwrap();
    assert_eq!(sent.last().unwrap().data, respawn_1_15(0, "default"));
}

#[test]
fn kept_attributes_are_restored_after_respawn() {
    let mut step = downgrading();
    step.clientbound("login", &login_1_16("0000000000000000"))
        .unwrap();
    let attributes = |entity: i32, value: f64| {
        body(|w| {
            w.varint(entity)
                .i32(1)
                .string("minecraft:generic.max_health")
                .f64(value)
                .varint(1)
                .uuid(5)
                .f64(2.0)
                .i8(0);
        })
    };
    // Another entity's attributes are not the player's.
    step.clientbound("update_attributes", &attributes(9, 4.0))
        .unwrap();
    step.clientbound("update_attributes", &attributes(1, 30.0))
        .unwrap();
    let sent = step
        .clientbound(
            "respawn",
            &respawn_1_16("minecraft:overworld", "minecraft:overworld", true),
        )
        .unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].name, "update_attributes");
    assert_eq!(
        sent[1].data,
        body(|w| {
            w.varint(1)
                .i32(1)
                .string("generic.maxHealth")
                .f64(30.0)
                .varint(1)
                .uuid(5)
                .f64(2.0)
                .i8(0);
        })
    );
    // A respawn that resets them forgets them.
    let reset = respawn_1_16("minecraft:overworld", "minecraft:overworld", false);
    assert_eq!(step.clientbound("respawn", &reset).unwrap().len(), 1);
    let keep = respawn_1_16("minecraft:overworld", "minecraft:overworld", true);
    assert_eq!(step.clientbound("respawn", &keep).unwrap().len(), 1);
}

#[test]
fn custom_dimension_types_get_the_closest_old_dimension() {
    use crate::nbt::{Compound, Nbt};
    let kind = |name: &str, skylight: bool, ceiling: bool, warm: bool| {
        let mut entry = Compound::new();
        entry.insert("name", name);
        entry.insert("has_skylight", skylight);
        entry.insert("has_ceiling", ceiling);
        entry.insert("ultrawarm", warm);
        Nbt::Compound(entry)
    };
    let mut codec = Compound::new();
    codec.insert(
        "dimension",
        vec![
            kind("rift:caves", false, true, false),
            kind("rift:void", false, false, false),
            kind("rift:sky", true, false, false),
        ],
    );
    let mut packet = hex("00000001 01 ff 00");
    packet.extend(Nbt::Compound(codec).to_bytes(true));
    let mut tail = Writer::new();
    tail.string("rift:void")
        .string("rift:world")
        .i64(0)
        .u8(20)
        .varint(8)
        .bool(false)
        .bool(true)
        .bool(false)
        .bool(false);
    packet.extend(tail.buf);
    let mut step = downgrading();
    let sent = step.one(Direction::Clientbound, "login", &packet, "login");
    assert_eq!(&sent[5..9], 1i32.to_be_bytes());
    for (name, legacy) in [
        ("rift:caves", -1),
        ("rift:sky", 0),
        ("minecraft:the_nether", -1),
    ] {
        let sent = step
            .clientbound("respawn", &respawn_1_16(name, "rift:world", false))
            .unwrap();
        assert_eq!(sent.last().unwrap().data, respawn_1_15(legacy, "default"));
    }
}

#[test]
fn lightning_moves_between_its_packets() {
    let lightning = data::version(NEWER)
        .expect_registry("entity_type")
        .id("lightning_bolt")
        .unwrap();
    let global = body(|w| {
        w.varint(42).i8(1).f64(1.0).f64(2.0).f64(3.0);
    });
    let sent = upgrading().one(
        Direction::Clientbound,
        "add_global_entity",
        &global,
        "add_entity",
    );
    let mut expected = body(|w| {
        w.varint(42);
    });
    expected.extend(&sent[1..17]);
    expected.extend(body(|w| {
        w.varint(lightning)
            .f64(1.0)
            .f64(2.0)
            .f64(3.0)
            .i8(0)
            .i8(0)
            .i32(0)
            .i16(0)
            .i16(0)
            .i16(0);
    }));
    assert_eq!(sent, expected);
    let sent = downgrading().one(
        Direction::Clientbound,
        "add_entity",
        &expected,
        "add_global_entity",
    );
    assert_eq!(sent, global);
    // Other global entities never existed in vanilla.
    let other = body(|w| {
        w.varint(42).i8(2).f64(1.0).f64(2.0).f64(3.0);
    });
    assert!(
        upgrading()
            .clientbound("add_global_entity", &other)
            .is_err()
    );
}

#[test]
fn other_entities_and_falling_blocks_are_renumbered() {
    let (old, new) = (data::version(OLDER), data::version(NEWER));
    let spawn = |kind: i32, data: i32| {
        body(|w| {
            w.varint(7)
                .uuid(9)
                .varint(kind)
                .f64(0.5)
                .f64(64.0)
                .f64(0.5)
                .i8(0)
                .i8(0)
                .i32(data)
                .i16(0)
                .i16(1)
                .i16(0);
        })
    };
    let block =
        |version: &data::VersionData, state: &str| version.block_states.parse(state).unwrap();
    let falling = |version: &data::VersionData| {
        version
            .expect_registry("entity_type")
            .id("falling_block")
            .unwrap()
    };
    let sent = downgrading().one(
        Direction::Clientbound,
        "add_entity",
        &spawn(falling(new), block(new, "soul_soil")),
        "add_entity",
    );
    assert_eq!(sent, spawn(falling(old), block(old, "soul_sand")));
    let arrow =
        |version: &data::VersionData| version.expect_registry("entity_type").id("arrow").unwrap();
    let sent = downgrading().one(
        Direction::Clientbound,
        "add_entity",
        &spawn(arrow(new), 8),
        "add_entity",
    );
    assert_eq!(sent, spawn(arrow(old), 8));
}

#[test]
fn equipment_splits_and_joins() {
    // Recorded: a 1.16 armour stand's main hand and helmet in one packet;
    // the 1.15.2 server sent the same as two.
    let equipment = hex(
        "0d 80 01ca04010a000003000644616d61676500000000 00 05 01fa04010a000003000644616d6167650000000000",
    );
    match downgrading().clientbound("set_equipment", &equipment) {
        Err(error) if unimplemented(&error) => {}
        Err(error) => panic!("{error}"),
        Ok(sent) => {
            assert_eq!(sent.len(), 2);
            assert_eq!(sent[0].data[..2], [0x0d, 0x00]);
            assert_eq!(sent[1].data[..2], [0x0d, 0x05]);
        }
    }
    let equipped = hex("0d 05 01bf04010a000003000644616d6167650000000000");
    match upgrading().clientbound("set_equipped_item", &equipped) {
        Err(error) if unimplemented(&error) => {}
        Err(error) => panic!("{error}"),
        Ok(sent) => {
            assert_eq!(sent[0].name, "set_equipment");
            assert_eq!(sent[0].data[..2], [0x0d, 0x05]);
        }
    }
    // Slots outside the six fail before any item is read.
    assert!(!unimplemented(
        &upgrading()
            .clientbound("set_equipped_item", &hex("0d 06 00"))
            .unwrap_err()
    ));
    assert!(!unimplemented(
        &downgrading()
            .clientbound("set_equipment", &hex("0d 07 00"))
            .unwrap_err()
    ));
}

#[test]
fn enchanting_table_hints_follow_the_registry() {
    let (old, new) = (data::version(OLDER), data::version(NEWER));
    let enchantment = |version: &data::VersionData, name: &str| {
        version.expect_registry("enchantment").id(name).unwrap() as i16
    };
    let menu =
        |version: &data::VersionData, name: &str| version.expect_registry("menu").id(name).unwrap();
    let open = |version: &data::VersionData, kind: &str| {
        body(|w| {
            w.varint(3)
                .varint(menu(version, kind))
                .string("{\"text\":\"\"}");
        })
    };
    let data = |property: i16, value: i16| {
        body(|w| {
            w.u8(3).i16(property).i16(value);
        })
    };
    let mut step = downgrading();
    step.clientbound("open_screen", &open(new, "enchantment"))
        .unwrap();
    let mending = (enchantment(new, "mending"), enchantment(old, "mending"));
    assert_ne!(mending.0, mending.1);
    let sent = step.one(
        Direction::Clientbound,
        "container_set_data",
        &data(5, mending.0),
        "container_set_data",
    );
    assert_eq!(sent, data(5, mending.1));
    // Levels and empty hints stay.
    for (property, value) in [(1, mending.0), (5, -1)] {
        let sent = step.one(
            Direction::Clientbound,
            "container_set_data",
            &data(property, value),
            "container_set_data",
        );
        assert_eq!(sent, data(property, value));
    }
    // A furnace's property 4 is not an enchantment.
    step.clientbound("open_screen", &open(new, "furnace"))
        .unwrap();
    let sent = step.one(
        Direction::Clientbound,
        "container_set_data",
        &data(4, mending.0),
        "container_set_data",
    );
    assert_eq!(sent, data(4, mending.0));

    let mut step = upgrading();
    step.clientbound("open_screen", &open(old, "enchantment"))
        .unwrap();
    let sent = step.one(
        Direction::Clientbound,
        "container_set_data",
        &data(6, mending.1),
        "container_set_data",
    );
    assert_eq!(sent, data(6, mending.0));
}

#[test]
fn map_colours_stay_within_what_old_clients_know() {
    assert_eq!(legacy_map_color(0), 0);
    assert_eq!(legacy_map_color(51 * 4 + 3), 51 * 4 + 3);
    assert_eq!(legacy_map_color(52 * 4 + 1), 28 * 4 + 1);
    assert_eq!(legacy_map_color(58 * 4 + 2), 23 * 4 + 2);
    assert_eq!(legacy_map_color(63 * 4), 11 * 4);
    let map = |colors: &[u8]| {
        body(|w| {
            w.varint(3).i8(0).bool(true).bool(false).varint(0);
            w.u8(2).u8(1).u8(0).u8(0).byte_array(colors);
        })
    };
    let sent = downgrading().one(
        Direction::Clientbound,
        "map_item_data",
        &map(&[0, 4, 208, 235]),
        "map_item_data",
    );
    assert_eq!(sent, map(&[0, 4, 112, 95]));
    // Without colour data.
    let empty = body(|w| {
        w.varint(3).i8(0).bool(true).bool(true).varint(1);
        w.varint(0).i8(1).i8(2).i8(3).bool(false);
        w.u8(0);
    });
    assert_eq!(
        downgrading().one(
            Direction::Clientbound,
            "map_item_data",
            &empty,
            "map_item_data"
        ),
        empty
    );
}

#[test]
fn interactions_gain_and_lose_the_sneaking_flag() {
    let attack = body(|w| {
        w.varint(5).varint(1);
    });
    let at = body(|w| {
        w.varint(5).varint(2).f32(0.5).f32(1.0).f32(0.5).varint(1);
    });
    let mut step = downgrading();
    let with = |packet: &[u8], sneaking: bool| {
        let mut packet = packet.to_vec();
        packet.push(u8::from(sneaking));
        packet
    };
    assert_eq!(
        step.one(Direction::Serverbound, "interact", &attack, "interact"),
        with(&attack, false)
    );
    let sneak = body(|w| {
        w.varint(1).varint(0).varint(0);
    });
    assert!(
        !step
            .serverbound("player_command", &sneak)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        step.one(Direction::Serverbound, "interact", &at, "interact"),
        with(&at, true)
    );
    let stand = body(|w| {
        w.varint(1).varint(1).varint(0);
    });
    step.serverbound("player_command", &stand).unwrap();
    assert_eq!(
        step.one(Direction::Serverbound, "interact", &at, "interact"),
        with(&at, false)
    );
    assert_eq!(
        upgrading().one(
            Direction::Serverbound,
            "interact",
            &with(&at, true),
            "interact"
        ),
        at
    );
    assert!(
        upgrading()
            .serverbound("interact", &hex("05 03 00"))
            .is_err()
    );
}

#[test]
fn abilities_lose_and_regain_the_speeds() {
    let mut step = upgrading();
    // Recorded: the 1.15.2 server's abilities for a creative player.
    step.clientbound("player_abilities", &hex("0d 3d4ccccd 3dcccccd"))
        .unwrap();
    assert_eq!(
        step.one(
            Direction::Serverbound,
            "player_abilities",
            &[0x02],
            "player_abilities"
        ),
        hex("02 3d4ccccd 3dcccccd")
    );
    assert_eq!(
        downgrading().one(
            Direction::Serverbound,
            "player_abilities",
            &hex("0f 3d4ccccd 3dcccccd"),
            "player_abilities"
        ),
        [0x02]
    );
}

#[test]
fn jigsaw_editor_converts_and_generate_is_dropped() {
    let old = body(|w| {
        w.i64(77)
            .string("minecraft:bottom")
            .string("minecraft:village/pool")
            .string("minecraft:air");
    });
    let new = body(|w| {
        w.i64(77)
            .string("minecraft:empty")
            .string("minecraft:bottom")
            .string("minecraft:village/pool")
            .string("minecraft:air")
            .string("rollable");
    });
    assert_eq!(
        downgrading().one(
            Direction::Serverbound,
            "set_jigsaw_block",
            &old,
            "set_jigsaw_block"
        ),
        new
    );
    assert_eq!(
        upgrading().one(
            Direction::Serverbound,
            "set_jigsaw_block",
            &new,
            "set_jigsaw_block"
        ),
        old
    );
    let generate = body(|w| {
        w.i64(77).varint(3).bool(false);
    });
    assert!(
        upgrading()
            .serverbound("jigsaw_generate", &generate)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn malformed_packets_fail() {
    type Case = (fn() -> Harness, Direction, &'static str, Vec<u8>);
    let cases: [Case; 12] = [
        (
            upgrading,
            Direction::Clientbound,
            "login",
            hex(LOGIN_1_15_2),
        ),
        (
            downgrading,
            Direction::Clientbound,
            "login",
            login_1_16("0000000000000000"),
        ),
        (
            upgrading,
            Direction::Clientbound,
            "respawn",
            respawn_1_15(0, "default"),
        ),
        (
            downgrading,
            Direction::Clientbound,
            "respawn",
            respawn_1_16("minecraft:overworld", "w", true),
        ),
        (
            upgrading,
            Direction::Clientbound,
            "add_global_entity",
            body(|w| {
                w.varint(1).i8(1).f64(0.0).f64(0.0).f64(0.0);
            }),
        ),
        (
            upgrading,
            Direction::Serverbound,
            "interact",
            hex("05 00 01 00"),
        ),
        (
            downgrading,
            Direction::Serverbound,
            "interact",
            hex("05 00 01"),
        ),
        (
            upgrading,
            Direction::Serverbound,
            "player_abilities",
            hex("02"),
        ),
        (
            downgrading,
            Direction::Serverbound,
            "player_abilities",
            hex("02 3d4ccccd 3dcccccd"),
        ),
        (
            downgrading,
            Direction::Clientbound,
            "container_set_data",
            hex("03 0004 0002"),
        ),
        (
            downgrading,
            Direction::Clientbound,
            "map_item_data",
            body(|w| {
                w.varint(3)
                    .i8(0)
                    .bool(true)
                    .bool(false)
                    .varint(0)
                    .u8(1)
                    .u8(1)
                    .u8(0)
                    .u8(0)
                    .byte_array(&[4]);
            }),
        ),
        (
            downgrading,
            Direction::Serverbound,
            "set_jigsaw_block",
            body(|w| {
                w.i64(1).string("a").string("b").string("c");
            }),
        ),
    ];
    for (make, direction, name, packet) in cases {
        truncations_fail(&mut make(), direction, name, &packet);
    }
    let mut step = Harness::new((down::STEP.unwrap())(), OLDER, NEWER);
    for size in 0..hex(LOGIN_FINISHED_1_16).len() {
        assert!(
            step.send(
                Direction::Clientbound,
                State::Login,
                "login_finished",
                &hex(LOGIN_FINISHED_1_16)[..size]
            )
            .is_err()
        );
    }
}

#[test]
fn recorded_sessions_pass_through() {
    let Some(count) = replay(&mut downgrading(), NEWER, Direction::Clientbound) else {
        eprintln!("no corpus for {NEWER}; skipped");
        return;
    };
    assert!(count > 1000);
    replay(&mut downgrading(), OLDER, Direction::Serverbound).unwrap();
    replay(&mut upgrading(), OLDER, Direction::Clientbound).unwrap();
    replay(&mut upgrading(), NEWER, Direction::Serverbound).unwrap();
}

#[test]
fn block_state_rules_keep_walls_connected_and_jigsaws_oriented() {
    let map = |from: i32, to: i32, state: &str| {
        let (source, target) = (data::version(from), data::version(to));
        let id = source.block_states.parse(state).unwrap();
        let mapped = crate::mapping::mapping(from, to, "block_state")
            .get(id)
            .unwrap();
        target.block_states.describe(mapped).unwrap()
    };
    assert_eq!(
        map(
            OLDER,
            NEWER,
            "brick_wall[east=true,north=false,south=true,up=true,waterlogged=true,west=false]"
        ),
        "brick_wall[east=low,north=none,south=low,up=true,waterlogged=true,west=none]"
    );
    assert_eq!(
        map(
            NEWER,
            OLDER,
            "diorite_wall[east=tall,north=low,south=none,up=false,waterlogged=false,west=tall]"
        ),
        "diorite_wall[east=true,north=true,south=false,up=false,waterlogged=false,west=true]"
    );
    assert_eq!(
        map(
            NEWER,
            OLDER,
            "polished_blackstone_brick_wall[east=none,north=tall,south=none,up=true,waterlogged=false,west=low]"
        ),
        "stone_brick_wall[east=false,north=true,south=false,up=true,waterlogged=false,west=true]"
    );
    assert_eq!(
        map(OLDER, NEWER, "jigsaw[facing=up]"),
        "jigsaw[orientation=up_north]"
    );
    assert_eq!(
        map(OLDER, NEWER, "jigsaw[facing=west]"),
        "jigsaw[orientation=west_up]"
    );
    assert_eq!(
        map(NEWER, OLDER, "jigsaw[orientation=down_east]"),
        "jigsaw[facing=down]"
    );
    assert_eq!(
        map(NEWER, OLDER, "jigsaw[orientation=south_up]"),
        "jigsaw[facing=south]"
    );
    // New blocks keep their shape: stairs stay stairs facing the same way.
    assert_eq!(
        map(
            NEWER,
            OLDER,
            "crimson_stairs[facing=east,half=top,shape=outer_left,waterlogged=true]"
        ),
        "acacia_stairs[facing=east,half=top,shape=outer_left,waterlogged=true]"
    );
    assert_eq!(
        map(NEWER, OLDER, "soul_wall_torch[facing=west]"),
        "wall_torch[facing=west]"
    );
    // Every state of both versions has a counterpart.
    for (from, to) in [(OLDER, NEWER), (NEWER, OLDER)] {
        let mapping = crate::mapping::mapping(from, to, "block_state");
        assert_eq!(mapping.unmapped_ids().count(), 0, "{from}->{to}");
    }
}
