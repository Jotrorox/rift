use super::{
    DOWN, NEWER, OLDER, UP,
    chat::{self, ChatTypes},
    layout::{Dimension, Login, Respawn, registry_entries},
    messages::{Placement, SystemChat},
    testing::{Harness, first, hex, recorded, truncations_fail},
};
use crate::{
    Direction::{Clientbound, Serverbound},
    State::{Login as LoginState, Play},
    codec::{
        Shared,
        registries::{self, Side},
    },
    data,
    nbt::Nbt,
    wire::{Position, Reader, Writer},
};
use serde_json::{Value, json};

fn up() -> Harness {
    Harness::new(UP.unwrap(), NEWER, OLDER)
}

fn down() -> Harness {
    Harness::new(DOWN.unwrap(), OLDER, NEWER)
}

fn bytes(write: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut writer = Writer::new();
    write(&mut writer);
    writer.buf
}

fn entity_name(protocol: i32, id: i32) -> &'static str {
    data::version(protocol)
        .expect_registry("entity_type")
        .name(id)
        .unwrap()
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(super::up::Up::routes().problems(), &[] as &[String]);
    assert_eq!(super::down::Down::routes().problems(), &[] as &[String]);
}

#[test]
fn login_names_the_dimension_and_brings_chat_types() {
    let (Some(older), Some(newer)) = (
        first(OLDER, Clientbound, Play, "login"),
        first(NEWER, Clientbound, Play, "login"),
    ) else {
        return;
    };
    let original = Login::read(OLDER, &older).unwrap();
    assert_eq!(original.write(OLDER).unwrap(), older, "login round trip");

    let out = up().one(Clientbound, Play, "login", &older);
    let translated = Login::read(NEWER, &out.data).unwrap();
    assert_eq!(
        translated.dimension,
        Dimension::Key("minecraft:overworld".into())
    );
    assert_eq!(translated.death, None);
    let native = Login::read(NEWER, &newer).unwrap();
    assert_eq!(
        translated.codec.get("minecraft:chat_type"),
        native.codec.get("minecraft:chat_type")
    );
    for entry in registry_entries(&translated.codec, "minecraft:dimension_type").unwrap() {
        let element = entry.as_compound().unwrap().compound("element").unwrap();
        assert!(element.contains("monster_spawn_light_level"));
        assert!(element.contains("monster_spawn_block_light_limit"));
    }
    assert_eq!(
        (translated.world.as_str(), translated.seed, translated.flat),
        (original.world.as_str(), original.seed, original.flat)
    );

    let out = down().one(Clientbound, Play, "login", &newer);
    let translated = Login::read(OLDER, &out.data).unwrap();
    let Dimension::Element(element) = &translated.dimension else {
        panic!("1.18.2 gets the element");
    };
    assert_eq!(element.str("effects"), Some("minecraft:overworld"));
    assert_eq!(element.int("min_y"), Some(-64));
    assert!(!translated.codec.contains("minecraft:chat_type"));
    for biome in registry_entries(&translated.codec, "minecraft:worldgen/biome").unwrap() {
        let element = biome.as_compound().unwrap().compound("element").unwrap();
        assert!(element.contains("category"));
    }
    truncations_fail(
        DOWN.unwrap(),
        OLDER,
        NEWER,
        Clientbound,
        Play,
        "login",
        &newer[..300],
    );
}

#[test]
fn respawn_follows_the_dimension_types_of_login() {
    let (Some(older), Some(newer)) = (
        first(OLDER, Clientbound, Play, "login"),
        first(NEWER, Clientbound, Play, "login"),
    ) else {
        return;
    };
    let nether = |dimension: Dimension| Respawn {
        dimension,
        world: "minecraft:the_nether".into(),
        seed: 1,
        game_mode: 0,
        previous_game_mode: -1,
        debug: false,
        flat: false,
        keep_attributes: true,
        keep_entity_data: true,
        death: None,
    };
    let login = Login::read(OLDER, &older).unwrap();
    let element = registry_entries(&login.codec, "minecraft:dimension_type").unwrap()[2]
        .as_compound()
        .unwrap()
        .compound("element")
        .unwrap()
        .clone();
    // Before any login the vanilla types identify the element.
    let mut up = up();
    let out = up.one(
        Clientbound,
        Play,
        "respawn",
        &nether(Dimension::Element(element.clone()))
            .write(OLDER)
            .unwrap(),
    );
    assert_eq!(
        Respawn::read(NEWER, &out.data).unwrap(),
        nether(Dimension::Key("minecraft:the_nether".into()))
    );
    up.clientbound(Play, "login", &older).unwrap();
    let out = up.one(
        Clientbound,
        Play,
        "respawn",
        &nether(Dimension::Element(element)).write(OLDER).unwrap(),
    );
    assert_eq!(
        Respawn::read(NEWER, &out.data).unwrap(),
        nether(Dimension::Key("minecraft:the_nether".into()))
    );

    let mut down = down();
    down.clientbound(Play, "login", &newer).unwrap();
    let mut died = nether(Dimension::Key("minecraft:the_nether".into()));
    died.death = Some(("minecraft:overworld".into(), Position { x: 1, y: 2, z: 3 }));
    let out = down.one(Clientbound, Play, "respawn", &died.write(NEWER).unwrap());
    let translated = Respawn::read(OLDER, &out.data).unwrap();
    let Dimension::Element(element) = translated.dimension else {
        panic!("1.18.2 gets the element");
    };
    assert_eq!(element.str("effects"), Some("minecraft:the_nether"));
    // A type the backend never announced degrades to the vanilla type of
    // its kind: the overworld for any name but the nether's and the end's.
    let unknown = nether(Dimension::Key("minecraft:moon".into()))
        .write(NEWER)
        .unwrap();
    let out = down.one(Clientbound, Play, "respawn", &unknown);
    let Dimension::Element(element) = Respawn::read(OLDER, &out.data).unwrap().dimension else {
        panic!("1.18.2 gets the element");
    };
    assert_eq!(element.str("effects"), Some("minecraft:overworld"));
}

/// The entity type of a 1.19 `add_entity`.
fn spawned_type(data: &[u8]) -> (i32, &'static str) {
    let mut r = Reader::new(data);
    let entity = r.varint().unwrap();
    r.uuid().unwrap();
    (entity, entity_name(NEWER, r.varint().unwrap()))
}

#[test]
fn spawns_merge_into_add_entity_for_1_19() {
    let (Some(session), Some(native)) = (
        crate::codec::testing::corpus(OLDER),
        crate::codec::testing::corpus(NEWER),
    ) else {
        return;
    };
    // The 1.19 server spawned the same kinds of entities (and a few more of
    // its own, so IDs differ) in its session.
    let native: std::collections::HashSet<&'static str> = native
        .iter()
        .filter(|packet| packet.direction == Clientbound && packet.name(NEWER) == "add_entity")
        .map(|packet| spawned_type(&packet.data).1)
        .collect();
    let mut up = up();
    let mut checked = 0;
    for packet in &session {
        let name = packet.name(OLDER);
        if packet.direction != Clientbound
            || !["add_entity", "add_mob", "add_painting"].contains(&name)
        {
            continue;
        }
        let out = up.clientbound(Play, name, &packet.data).unwrap();
        assert_eq!(out[0].name, "add_entity");
        let (entity, kind) = spawned_type(&out[0].data);
        assert!(
            native.contains(kind),
            "entity {entity} ({kind}) from {name}"
        );
        checked += 1;
        if name == "add_painting" {
            // As the 1.19 server spawns the scenario's kebab painting.
            let painting = first(NEWER, Clientbound, Play, "add_entity")
                .and(recorded(NEWER, Clientbound, Play, "add_entity"))
                .unwrap()
                .into_iter()
                .find(|data| spawned_type(data).1 == "painting")
                .unwrap();
            // The worlds differ: compare all but the UUID and x and z.
            let fields = |data: &[u8]| [&data[17..18], &data[26..34], &data[42..]].concat();
            assert_eq!(fields(&out[0].data), fields(&painting));
            assert_eq!(out[1].name, "set_entity_data");
            assert_eq!(out[1].data, hex("1c081600ff"));
        }
    }
    assert!(checked > 20, "checked {checked} spawns");
}

/// The step spawns entities itself; the entity codec still learns their
/// types, so a zombie's own entries (mob flags, health) keep through its
/// entity data in both directions.
#[test]
fn spawned_entities_keep_their_own_entity_data() {
    let zombie = |protocol: i32| {
        data::version(protocol)
            .expect_registry("entity_type")
            .id("zombie")
            .unwrap()
    };
    let entity_data = bytes(|w| {
        w.varint(7).u8(15).varint(0).u8(1);
        w.u8(9).varint(2).f32(20.0).u8(0xff);
    });
    let mut up = up();
    let spawn = bytes(|w| {
        w.varint(7).uuid(1).varint(zombie(OLDER));
        w.f64(0.0).f64(-60.0).f64(0.0).i8(0).i8(0).i8(0);
        w.i16(0).i16(0).i16(0);
    });
    up.clientbound(Play, "add_mob", &spawn).unwrap();
    let out = up
        .clientbound(Play, "set_entity_data", &entity_data)
        .unwrap();
    assert_eq!(out[0].data, entity_data);
    let mut down = down();
    let spawn = bytes(|w| {
        w.varint(7).uuid(1).varint(zombie(NEWER));
        w.f64(0.0).f64(-60.0).f64(0.0).i8(0).i8(0).i8(0).varint(0);
        w.i16(0).i16(0).i16(0);
    });
    down.clientbound(Play, "add_entity", &spawn).unwrap();
    let out = down
        .clientbound(Play, "set_entity_data", &entity_data)
        .unwrap();
    assert_eq!(out[0].data, entity_data);
}

#[test]
fn add_entity_splits_for_1_18_2() {
    let Some(session) = crate::codec::testing::corpus(NEWER) else {
        return;
    };
    let mut down = down();
    let mut names = Vec::new();
    for packet in &session {
        if packet.direction != Clientbound || packet.state != Play {
            continue;
        }
        let name = packet.name(NEWER);
        if !["add_entity", "set_entity_data"].contains(&name) {
            continue;
        }
        for out in down.clientbound(Play, name, &packet.data).unwrap() {
            if out.name.starts_with("add_") {
                names.push(out.name.clone());
            }
            if out.name == "add_painting" {
                // As the 1.18.2 server spawned it in its world: kebab,
                // facing south, at the same height.
                let native = first(OLDER, Clientbound, Play, "add_painting").unwrap();
                let fields = |data: &[u8]| {
                    let mut r = Reader::new(&data[17..]);
                    (
                        r.varint().unwrap(),
                        r.position().unwrap().y,
                        r.i8().unwrap(),
                    )
                };
                assert_eq!(fields(&out.data), fields(&native));
            }
        }
    }
    for kind in ["add_entity", "add_mob", "add_painting"] {
        assert!(
            names.iter().any(|name| name == kind),
            "no {kind} in {names:?}"
        );
    }
    // A painting without entity data spawns with the default variant before
    // the next packet; one with a later variant gets that variant.
    let painting = |variant: Option<i32>| {
        let mut down = Harness::new(DOWN.unwrap(), OLDER, NEWER);
        let spawn = bytes(|w| {
            w.varint(9)
                .uuid(1)
                .varint(63)
                .f64(3.0)
                .f64(-59.0)
                .f64(-13.0);
            w.i8(0).i8(0).i8(0).varint(5).bytes(&[0; 6]);
        });
        assert!(
            down.clientbound(Play, "add_entity", &spawn)
                .unwrap()
                .is_empty()
        );
        let next = match variant {
            Some(variant) => {
                let data = bytes(|w| {
                    w.varint(9)
                        .u8(0)
                        .varint(0)
                        .u8(0)
                        .u8(8)
                        .varint(22)
                        .varint(variant)
                        .u8(0xff);
                });
                down.clientbound(Play, "set_entity_data", &data).unwrap()
            }
            None => down.clientbound(Play, "set_time", &[0; 16]).unwrap(),
        };
        assert_eq!(next[0].name, "add_painting");
        let mut r = Reader::new(&next[0].data);
        r.varint().unwrap();
        r.uuid().unwrap();
        let motive = r.varint().unwrap();
        let position = r.position().unwrap();
        (motive, position, r.i8().unwrap())
    };
    let at = Position {
        x: 3,
        y: -59,
        z: -13,
    };
    assert_eq!(painting(None), (0, at, 3));
    // `donkey_kong` moved from 29 to 25; `fire` (28) has a stand-in.
    assert_eq!(painting(Some(29)).0, 25);
    let fire = painting(Some(28)).0;
    assert!((0..26).contains(&fire));
}

#[test]
fn vibrations_become_particles() {
    let Some(signals) = recorded(OLDER, Clientbound, Play, "add_vibration_signal") else {
        return;
    };
    for signal in signals {
        let out = up().one(Clientbound, Play, "add_vibration_signal", &signal);
        assert_eq!(out.name, "level_particles");
        let mut r = Reader::new(&out.data);
        assert_eq!(r.varint().unwrap(), 40); // `vibration` in 1.19.
        r.bool().unwrap();
        let x = r.f64().unwrap();
        assert_eq!(x.fract().abs(), 0.5);
        r.take(2 * 8 + 4 * 4).unwrap();
        assert_eq!(r.i32().unwrap(), 1);
        assert_eq!(r.str().unwrap(), "minecraft:block");
        r.i64().unwrap();
        r.varint().unwrap();
        r.finish().unwrap();
        truncations_fail(
            UP.unwrap(),
            NEWER,
            OLDER,
            Clientbound,
            Play,
            "add_vibration_signal",
            &signal,
        );
    }
}

fn component(json: &str) -> Value {
    serde_json::from_str(json).unwrap()
}

#[test]
fn chat_splits_and_merges() {
    let (Some(older), Some(newer)) = (
        recorded(OLDER, Clientbound, Play, "chat"),
        recorded(NEWER, Clientbound, Play, "system_chat"),
    ) else {
        return;
    };
    // The 1.18.2 game mode messages are exactly what 1.19 sends, and back.
    let out = up().one(Clientbound, Play, "chat", &older[0]);
    assert_eq!((out.name.as_str(), &out.data), ("system_chat", &newer[0]));
    let mut down = down();
    if let Some(login) = first(NEWER, Clientbound, Play, "login") {
        down.clientbound(Play, "login", &login).unwrap();
    }
    for (newer, older) in newer.iter().zip(&older) {
        let out = down.one(Clientbound, Play, "system_chat", newer);
        assert_eq!((out.name.as_str(), &out.data), ("chat", older));
    }
    // An action bar message stays one.
    let info = bytes(|w| {
        w.string(r#"{"text":"!"}"#).i8(2).uuid(0);
    });
    let out = up().one(Clientbound, Play, "chat", &info);
    assert_eq!(
        SystemChat::read(NEWER, &out.data).unwrap().placement,
        Placement::ChatType(chat::GAME_INFO_1_19)
    );
    assert_eq!(
        down.one(Clientbound, Play, "system_chat", &out.data).data,
        info
    );

    let say = first(NEWER, Clientbound, Play, "player_chat").unwrap();
    let out = down.one(Clientbound, Play, "player_chat", &say);
    let message = SystemChat::read(OLDER, &out.data).unwrap();
    assert_eq!(
        component(&message.json),
        component(&SystemChat::read(OLDER, &older[4]).unwrap().json)
    );
    assert_eq!(message.placement, Placement::Position(0));
    truncations_fail(
        DOWN.unwrap(),
        OLDER,
        NEWER,
        Clientbound,
        Play,
        "player_chat",
        &say,
    );
    truncations_fail(UP.unwrap(), NEWER, OLDER, Clientbound, Play, "chat", &info);
    for name in [
        "chat_preview",
        "set_display_chat_preview",
        "server_data",
        "block_changed_ack",
    ] {
        assert!(down.clientbound(Play, name, &[0]).unwrap().is_empty());
    }
}

#[test]
fn serverbound_chat_and_commands() {
    let signed = bytes(|w| {
        w.string("hello")
            .i64(5)
            .i64(6)
            .byte_array(&[1; 8])
            .bool(false);
    });
    assert_eq!(
        up().one(Serverbound, Play, "chat", &signed).data,
        bytes(|w| {
            w.string("hello");
        })
    );
    let command = bytes(|w| {
        w.string("gamemode creative")
            .i64(5)
            .i64(6)
            .varint(1)
            .string("x")
            .byte_array(&[2])
            .bool(false);
    });
    let out = up().one(Serverbound, Play, "chat_command", &command);
    assert_eq!(
        (out.name.as_str(), out.data),
        (
            "chat",
            bytes(|w| {
                w.string("/gamemode creative");
            })
        )
    );
    // A full-length command loses its last character to the slash.
    let long = "a".repeat(256);
    let command = bytes(|w| {
        w.string(&long).i64(5).i64(6).varint(0).bool(false);
    });
    let out = up().one(Serverbound, Play, "chat_command", &command);
    assert_eq!(
        out.data,
        bytes(|w| {
            w.string(&format!("/{}", &long[..255]));
        })
    );
    assert!(
        up().serverbound(Play, "chat_preview", &[0; 5])
            .unwrap()
            .is_empty()
    );

    let mut down = down();
    let out = down.one(
        Serverbound,
        Play,
        "chat",
        &bytes(|w| {
            w.string("hello");
        }),
    );
    let mut r = Reader::new(&out.data);
    assert_eq!(r.str().unwrap(), "hello");
    let timestamp = r.i64().unwrap();
    assert!(timestamp > 1_600_000_000_000);
    assert_eq!(
        r.rest(),
        bytes(|w| {
            w.i64(0).byte_array(&[]).bool(false);
        })
    );
    let out = down.one(
        Serverbound,
        Play,
        "chat",
        &bytes(|w| {
            w.string("/tp 0 0 0");
        }),
    );
    assert_eq!(out.name, "chat_command");
    let mut r = Reader::new(&out.data);
    assert_eq!(r.str().unwrap(), "tp 0 0 0");
    assert!(r.i64().unwrap() >= timestamp);
    assert_eq!(
        r.rest(),
        bytes(|w| {
            w.i64(0).varint(0).bool(false);
        })
    );
}

#[test]
fn login_phase_matches_each_version() {
    let (Some(older), Some(newer)) = (
        first(OLDER, Serverbound, LoginState, "hello"),
        first(NEWER, Serverbound, LoginState, "hello"),
    ) else {
        return;
    };
    assert_eq!(
        up().one(Serverbound, LoginState, "hello", &newer).data,
        older
    );
    assert_eq!(
        down().one(Serverbound, LoginState, "hello", &older).data,
        newer
    );
    let (older, newer) = (
        first(OLDER, Clientbound, LoginState, "login_finished").unwrap(),
        first(NEWER, Clientbound, LoginState, "login_finished").unwrap(),
    );
    assert_eq!(
        up().one(Clientbound, LoginState, "login_finished", &older)
            .data,
        newer
    );
    assert_eq!(
        down()
            .one(Clientbound, LoginState, "login_finished", &newer)
            .data,
        older
    );
    let with_skin = bytes(|w| {
        w.uuid(1)
            .string("Alex")
            .varint(1)
            .string("textures")
            .string("e30=")
            .bool(true)
            .string("sig");
    });
    let out = down().one(Clientbound, LoginState, "login_finished", &with_skin);
    assert_eq!(
        out.data,
        bytes(|w| {
            w.uuid(1).string("Alex");
        })
    );
    truncations_fail(
        DOWN.unwrap(),
        OLDER,
        NEWER,
        Clientbound,
        LoginState,
        "login_finished",
        &with_skin,
    );
    let key = bytes(|w| {
        w.byte_array(&[1; 4]).byte_array(&[2; 4]);
    });
    let keyed = bytes(|w| {
        w.byte_array(&[1; 4]).bool(true).byte_array(&[2; 4]);
    });
    assert_eq!(down().one(Serverbound, LoginState, "key", &key).data, keyed);
    assert_eq!(up().one(Serverbound, LoginState, "key", &keyed).data, key);
}

const AT: Position = Position {
    x: 10,
    y: -60,
    z: -3,
};

#[test]
fn predicted_block_changes_are_acknowledged_for_1_19() {
    let mut up = up();
    let stop = bytes(|w| {
        w.varint(2).position(AT).u8(1).varint(3);
    });
    let out = up.one(Serverbound, Play, "player_action", &stop);
    assert_eq!(out.data, stop[..stop.len() - 1]);
    // The backend agrees and says the block is air now.
    let answer = bytes(|w| {
        w.position(AT).varint(0).varint(2).bool(true);
    });
    let out = up.clientbound(Play, "block_break_ack", &answer).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(
        (out[0].name.as_str(), out[0].data.as_slice()),
        ("block_update", &answer[..9])
    );
    assert_eq!(
        (out[1].name.as_str(), out[1].data.as_slice()),
        ("block_changed_ack", &[3][..])
    );

    // Placing against the top of a block waits for the block above.
    let place = bytes(|w| {
        w.varint(0)
            .position(AT)
            .varint(1)
            .f32(0.5)
            .f32(1.0)
            .f32(0.5)
            .bool(false)
            .varint(4);
    });
    up.one(Serverbound, Play, "use_item_on", &place);
    let update = |position: Position| {
        bytes(|w| {
            w.position(position).varint(1);
        })
    };
    assert_eq!(
        up.clientbound(Play, "block_update", &update(AT))
            .unwrap()
            .len(),
        1
    );
    let out = up
        .clientbound(Play, "block_update", &update(Position { y: -59, ..AT }))
        .unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(
        (out[1].name.as_str(), out[1].data.as_slice()),
        ("block_changed_ack", &[4][..])
    );
    let out = up.serverbound(Play, "use_item", &[0, 5]).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(
        (out[1].direction, out[1].data.as_slice()),
        (Clientbound, &[5][..])
    );
}

#[test]
fn digging_is_acknowledged_for_1_18_2() {
    let mut down = down();
    let dig = |action: i32| {
        bytes(|w| {
            w.varint(action).position(AT).u8(1);
        })
    };
    let out = down.serverbound(Play, "player_action", &dig(0)).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(
        (out[0].direction, out[0].name.as_str()),
        (Clientbound, "block_break_ack")
    );
    assert_eq!(out[1].data, [&dig(0)[..], &[0]].concat());
    down.serverbound(Play, "player_action", &dig(2)).unwrap();
    let update = bytes(|w| {
        w.position(AT).varint(0);
    });
    let out = down.clientbound(Play, "block_update", &update).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[1].name, "block_break_ack");
    assert_eq!(
        out[1].data,
        bytes(|w| {
            w.position(AT).varint(0).varint(2).bool(true);
        })
    );
    let place = bytes(|w| {
        w.varint(0)
            .position(AT)
            .varint(1)
            .f32(0.5)
            .f32(1.0)
            .f32(0.5)
            .bool(false);
    });
    assert_eq!(
        down.one(Serverbound, Play, "use_item_on", &place).data,
        [&place[..], &[0]].concat()
    );
    assert_eq!(down.one(Serverbound, Play, "use_item", &[1]).data, [1, 0]);
    truncations_fail(
        DOWN.unwrap(),
        OLDER,
        NEWER,
        Serverbound,
        Play,
        "use_item_on",
        &place,
    );
}

#[test]
fn beacons_and_looms() {
    let none_and_haste = bytes(|w| {
        w.bool(false).bool(true).varint(3);
    });
    let legacy = bytes(|w| {
        w.varint(-1).varint(3);
    });
    assert_eq!(
        up().one(Serverbound, Play, "set_beacon", &none_and_haste)
            .data,
        legacy
    );
    assert_eq!(
        down().one(Serverbound, Play, "set_beacon", &legacy).data,
        none_and_haste
    );

    let open_loom = bytes(|w| {
        w.varint(4).varint(17).string(r#"{"text":"Loom"}"#);
    });
    let mut up = up();
    up.clientbound(Play, "open_screen", &open_loom).unwrap();
    let selected = |value: i16| {
        bytes(|w| {
            w.u8(4).i16(0).i16(value);
        })
    };
    assert_eq!(
        up.one(Clientbound, Play, "container_set_data", &selected(0))
            .data,
        selected(-1)
    );
    assert_eq!(
        up.one(Clientbound, Play, "container_set_data", &selected(5))
            .data,
        selected(4)
    );
    assert_eq!(
        up.one(Clientbound, Play, "container_set_data", &selected(36))
            .data,
        selected(0)
    );
    assert_eq!(
        up.one(Serverbound, Play, "container_button_click", &[4, 4])
            .data,
        [4, 5]
    );
    up.serverbound(Play, "container_close", &[4]).unwrap();
    assert_eq!(
        up.one(Serverbound, Play, "container_button_click", &[4, 4])
            .data,
        [4, 4]
    );

    let mut down = down();
    down.clientbound(Play, "open_screen", &open_loom).unwrap();
    assert_eq!(
        down.one(Clientbound, Play, "container_set_data", &selected(-1))
            .data,
        selected(0)
    );
    assert_eq!(
        down.one(Clientbound, Play, "container_set_data", &selected(4))
            .data,
        selected(5)
    );
    assert_eq!(
        down.one(Serverbound, Play, "container_button_click", &[4, 5])
            .data,
        [4, 4]
    );
}

#[test]
fn tags_gain_banner_patterns_for_1_19() {
    let (Some(tags), Some(native)) = (
        first(OLDER, Clientbound, Play, "update_tags"),
        first(NEWER, Clientbound, Play, "update_tags"),
    ) else {
        return;
    };
    // One packet, with the banner pattern tags a 1.19 loom offers: a
    // second, partial tags packet would be completed with vanilla tags
    // further up a chain, replacing the backend's own.
    let out = up().clientbound(Play, "update_tags", &tags).unwrap();
    assert_eq!(out.len(), 1);
    let banner_patterns = |body: &[u8]| {
        let mut r = Reader::new(body);
        for _ in 0..r.varint().unwrap() {
            let registry = r.str().unwrap().to_owned();
            let mut tags = Vec::new();
            for _ in 0..r.varint().unwrap() {
                let name = r.str().unwrap().to_owned();
                let ids: Vec<i32> = (0..r.varint().unwrap())
                    .map(|_| r.varint().unwrap())
                    .collect();
                tags.push((name, ids));
            }
            if registry == "minecraft:banner_pattern" {
                tags.sort();
                return Some(tags);
            }
        }
        None
    };
    let converted = banner_patterns(&out[0].data).unwrap();
    assert_eq!(Some(converted.clone()), banner_patterns(&native));
    let selectable = converted
        .iter()
        .find(|(name, _)| name == "minecraft:no_item_required")
        .unwrap();
    assert_eq!(selectable.1.len(), 34);
}

#[test]
fn sound_seeds_come_and_go() {
    let Some(sound) = first(OLDER, Clientbound, Play, "custom_sound") else {
        return;
    };
    let out = up().one(Clientbound, Play, "custom_sound", &sound);
    assert_eq!(out.data.len(), sound.len() + 8);
    assert_eq!(
        down()
            .one(Clientbound, Play, "custom_sound", &out.data)
            .data,
        sound
    );
    if let Some(native) = first(NEWER, Clientbound, Play, "custom_sound") {
        assert_eq!(native.len(), out.data.len());
    }
}

#[test]
fn chat_types_of_the_backend_decorate() {
    let Some(login) = first(NEWER, Clientbound, Play, "login") else {
        return;
    };
    let mut shared = Shared::new(OLDER, NEWER);
    let parsed = registries::login::read_login(NEWER, &mut Reader::new(&login)).unwrap();
    for registry in parsed.codec.unwrap() {
        registries::record(&mut shared, Side::Backend, &registry);
    }
    let types = ChatTypes::from_shared(&shared, Side::Backend);
    assert_eq!(types, ChatTypes::vanilla(NEWER));
    assert!(types.is_overlay(2));
    assert_eq!(
        types
            .display(4, &json!({"text": "A"}), None, &json!({"text": "hi"}))
            .unwrap()["translate"],
        "commands.message.display.incoming"
    );
    let _ = Nbt::Byte(0);
}
