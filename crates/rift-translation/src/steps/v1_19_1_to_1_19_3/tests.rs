use super::{DOWN, NEWER, OLDER, UP};
use crate::{
    Direction::{Clientbound, Serverbound},
    State::{Login as LoginState, Play},
    nbt::Nbt,
    steps::v1_18_2_to_1_19::{
        chat::registry_entries,
        join::{Dimension, Login, Respawn},
        messages::{Placement, SystemChat, read_disguised_chat},
        players::{self, Action, Entry},
        testing::{Harness, first, hex, recorded, truncations_fail},
    },
    wire::Writer,
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

#[test]
fn every_packet_has_a_route() {
    assert_eq!(super::up::Up::routes().problems(), &[] as &[String]);
    assert_eq!(super::down::Down::routes().problems(), &[] as &[String]);
}

/// `/say hello from console` as 1.19.2 and 1.19.3 servers send it.
const SAY_1_19_1: &str = "0000000000000000000000000000000000001268656c6c6f2066726f6d20636f6e736f6c65000000\
    01a1c4066daf4177d0a82c7e9eda00000001117b2274657874223a22536572766572227d00";
const SAY_1_19_3: &str = "1d7b2274657874223a2268656c6c6f2066726f6d20636f6e736f6c65227d04117b2274657874223a\
    22536572766572227d00";

#[test]
fn player_chat_reaches_1_19_3_as_disguised_chat() {
    let out = up().one(Clientbound, Play, "player_chat", &hex(SAY_1_19_1));
    assert_eq!(out.name, "disguised_chat");
    let message = read_disguised_chat(&out.data).unwrap();
    // The 1.19.2 server's `say_command` type, which the client got with its
    // login; the content as the 1.19.3 server sends it.
    assert_eq!(message.chat_type, 1);
    assert_eq!(message.content, json!({"text": "hello from console"}));
    assert_eq!(message.sender_name, json!({"text": "Server"}));
    truncations_fail(
        UP.unwrap(),
        NEWER,
        OLDER,
        Clientbound,
        Play,
        "player_chat",
        &hex(SAY_1_19_1),
    );
}

fn say_display() -> Value {
    json!({"translate": "chat.type.announcement",
           "with": [{"text": "Server"}, {"text": "hello from console"}]})
}

#[test]
fn chat_reaches_1_19_1_as_decorated_system_chat() {
    let mut down = down();
    if let Some(login) = first(NEWER, Clientbound, Play, "login") {
        down.clientbound(Play, "login", &login).unwrap();
    }
    let out = down.one(Clientbound, Play, "disguised_chat", &hex(SAY_1_19_3));
    assert_eq!(out.name, "system_chat");
    let message = SystemChat::read(OLDER, &out.data).unwrap();
    assert_eq!(message.placement, Placement::Overlay(false));
    assert_eq!(
        serde_json::from_str::<Value>(&message.json).unwrap(),
        say_display()
    );
    truncations_fail(
        DOWN.unwrap(),
        OLDER,
        NEWER,
        Clientbound,
        Play,
        "disguised_chat",
        &hex(SAY_1_19_3),
    );
}

/// A signed 1.19.3 `player_chat` from player 7, chat type `chat` (0).
fn signed_chat(text: &str) -> Vec<u8> {
    bytes(|w| {
        w.uuid(7)
            .varint(0)
            .bool(true)
            .bytes(&[5; 256])
            .string(text)
            .i64(1)
            .i64(2);
        w.varint(2).varint(0).bytes(&[6; 256]).varint(3); // Last seen.
        w.bool(false).varint(2).varint(1).i64(-1); // Partially filtered.
        w.varint(0).string(r#"{"text":"Alex"}"#).bool(false);
    })
}

#[test]
fn signed_chat_is_acknowledged_for_the_client() {
    let mut down = down();
    if let Some(login) = first(NEWER, Clientbound, Play, "login") {
        down.clientbound(Play, "login", &login).unwrap();
    }
    for _ in 0..64 {
        let out = down.one(Clientbound, Play, "player_chat", &signed_chat("hi"));
        let message = SystemChat::read(OLDER, &out.data).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&message.json).unwrap(),
            json!({"translate": "chat.type.text", "with": [{"text": "Alex"}, {"text": "hi"}]})
        );
    }
    // The 65th pending message makes the step acknowledge, as the vanilla
    // client would.
    let out = down
        .clientbound(Play, "player_chat", &signed_chat("hi"))
        .unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(
        (out[0].direction, out[0].name.as_str()),
        (Serverbound, "chat_ack")
    );
    assert_eq!(out[0].data, [65]);
    // Then the client's own message acknowledges what arrived since.
    down.clientbound(Play, "player_chat", &signed_chat("hi"))
        .unwrap();
    let sent = bytes(|w| {
        w.string("yo")
            .i64(3)
            .i64(4)
            .byte_array(&[])
            .bool(false)
            .varint(0)
            .bool(false);
    });
    let out = down.one(Serverbound, Play, "chat", &sent);
    assert_eq!(
        out.data,
        bytes(|w| {
            w.string("yo")
                .i64(3)
                .i64(4)
                .bool(false)
                .varint(1)
                .bytes(&[0; 3]);
        })
    );
    let command = bytes(|w| {
        w.string("me waves")
            .i64(3)
            .i64(4)
            .varint(0)
            .bool(false)
            .varint(0)
            .bool(false);
    });
    let out = down.one(Serverbound, Play, "chat_command", &command);
    assert_eq!(
        out.data,
        bytes(|w| {
            w.string("me waves")
                .i64(3)
                .i64(4)
                .varint(0)
                .varint(0)
                .bytes(&[0; 3]);
        })
    );
    truncations_fail(
        DOWN.unwrap(),
        OLDER,
        NEWER,
        Clientbound,
        Play,
        "player_chat",
        &signed_chat("x"),
    );
}

#[test]
fn serverbound_chat_reaches_1_19_1_unsigned() {
    let sent = bytes(|w| {
        w.string("yo")
            .i64(3)
            .i64(4)
            .bool(true)
            .bytes(&[1; 256])
            .varint(2)
            .bytes(&[3, 0, 0]);
    });
    let out = up().one(Serverbound, Play, "chat", &sent);
    assert_eq!(
        out.data,
        bytes(|w| {
            w.string("yo")
                .i64(3)
                .i64(4)
                .byte_array(&[])
                .bool(false)
                .varint(0)
                .bool(false);
        })
    );
    let command = bytes(|w| {
        w.string("msg Alex hi")
            .i64(3)
            .i64(4)
            .varint(1)
            .string("message")
            .bytes(&[1; 256]);
        w.varint(0).bytes(&[0; 3]);
    });
    let out = up().one(Serverbound, Play, "chat_command", &command);
    assert_eq!(
        out.data,
        bytes(|w| {
            w.string("msg Alex hi")
                .i64(3)
                .i64(4)
                .varint(0)
                .bool(false)
                .varint(0)
                .bool(false);
        })
    );
    truncations_fail(
        UP.unwrap(),
        NEWER,
        OLDER,
        Serverbound,
        Play,
        "chat_command",
        &command,
    );
    let mut up = up();
    assert!(up.serverbound(Play, "chat_ack", &[1]).unwrap().is_empty());
    assert!(
        up.serverbound(Play, "chat_session_update", &[])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn login_is_followed_by_feature_flags_and_biome_sounds_are_named() {
    let (Some(older), Some(newer)) = (
        first(OLDER, Clientbound, Play, "login"),
        first(NEWER, Clientbound, Play, "login"),
    ) else {
        return;
    };
    let out = up().clientbound(Play, "login", &older).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].data, older);
    assert_eq!(out[1].name, "update_enabled_features");
    // What the 1.19.3 server sends after its login.
    let features = first(NEWER, Clientbound, Play, "update_enabled_features").unwrap();
    assert_eq!(out[1].data, features);

    let original = Login::read(NEWER, &newer).unwrap();
    assert_eq!(original.write(NEWER).unwrap(), newer);
    let out = down().one(Clientbound, Play, "login", &newer);
    let translated = Login::read(OLDER, &out.data).unwrap();
    let biomes = registry_entries(&translated.codec, "minecraft:worldgen/biome").unwrap();
    let jungle = biomes[1]
        .as_compound()
        .unwrap()
        .compound("element")
        .unwrap();
    let effects = jungle.compound("effects").unwrap();
    assert_eq!(
        effects.compound("music").unwrap().get("sound"),
        Some(&Nbt::from("minecraft:music.overworld.jungle_and_forest"))
    );
    assert_eq!(
        effects.compound("mood_sound").unwrap().get("sound"),
        Some(&Nbt::from("minecraft:ambient.cave"))
    );
    // Nothing but sounds changed.
    let mut expected = original.clone();
    expected.codec = translated.codec.clone();
    assert_eq!(translated, expected);
    assert!(
        down()
            .clientbound(Play, "update_enabled_features", &[0])
            .unwrap()
            .is_empty()
    );
}

fn respawn(protocol: i32, keep: u8) -> Vec<u8> {
    Respawn {
        dimension: Dimension::Key("minecraft:the_nether".into()),
        world: "minecraft:the_nether".into(),
        seed: 5,
        game_mode: 0,
        previous_game_mode: -1,
        debug: false,
        flat: false,
        keep_attributes: keep & 1 != 0,
        keep_entity_data: keep & 2 != 0 || protocol < NEWER,
        death: Some((
            "minecraft:overworld".into(),
            crate::wire::Position { x: 1, y: 2, z: 3 },
        )),
    }
    .write(protocol)
    .unwrap()
}

#[test]
fn respawn_keeps_entity_data_and_revives_a_dead_player() {
    let mut up = up();
    assert_eq!(
        up.one(Clientbound, Play, "respawn", &respawn(OLDER, 1))
            .data,
        respawn(NEWER, 3)
    );
    // Dead, with food 7 and saturation 1.5.
    let dead = bytes(|w| {
        w.f32(0.0).varint(7).f32(1.5);
    });
    up.clientbound(Play, "set_health", &dead).unwrap();
    let out = up.clientbound(Play, "respawn", &respawn(OLDER, 0)).unwrap();
    assert_eq!(out[0].data, respawn(NEWER, 2));
    assert_eq!(out[1].name, "set_health");
    assert_eq!(
        out[1].data,
        bytes(|w| {
            w.f32(20.0).varint(7).f32(1.5);
        })
    );
    assert_eq!(
        up.clientbound(Play, "respawn", &respawn(OLDER, 0))
            .unwrap()
            .len(),
        1
    );

    let mut down = down();
    assert_eq!(
        down.one(Clientbound, Play, "respawn", &respawn(NEWER, 3))
            .data,
        respawn(OLDER, 1)
    );
    assert_eq!(
        down.one(Clientbound, Play, "respawn", &respawn(NEWER, 2))
            .data,
        respawn(OLDER, 0)
    );
    truncations_fail(
        DOWN.unwrap(),
        OLDER,
        NEWER,
        Clientbound,
        Play,
        "respawn",
        &respawn(NEWER, 3),
    );
}

#[test]
fn sounds_by_name_and_by_id_cross_both_ways() {
    let tail = |w: &mut Writer| {
        w.varint(4)
            .i32(8)
            .i32(-16)
            .i32(24)
            .f32(1.0)
            .f32(0.5)
            .i64(99);
    };
    let custom = bytes(|w| {
        w.string("minecraft:custom.horn");
        tail(w);
    });
    let inline = bytes(|w| {
        w.varint(0).string("minecraft:custom.horn").bool(false);
        tail(w);
    });
    let out = up().one(Clientbound, Play, "custom_sound", &custom);
    assert_eq!((out.name.as_str(), &out.data), ("sound", &inline));
    let out = down().one(Clientbound, Play, "sound", &inline);
    assert_eq!((out.name.as_str(), &out.data), ("custom_sound", &custom));
    // With a fixed range, which 1.19.1 cannot express.
    let ranged = bytes(|w| {
        w.varint(0)
            .string("minecraft:custom.horn")
            .bool(true)
            .f32(16.0);
        tail(w);
    });
    assert_eq!(down().one(Clientbound, Play, "sound", &ranged).data, custom);
    // Registry IDs of sounds both versions have map by name; 1.19.3's new
    // sounds have none in 1.19.1.
    let by_id = |id: i32| {
        bytes(|w| {
            w.varint(id);
            tail(w);
        })
    };
    let out = down().one(Clientbound, Play, "sound", &by_id(2));
    assert_eq!((out.name.as_str(), &out.data), ("sound", &by_id(1)));
    truncations_fail(
        DOWN.unwrap(),
        OLDER,
        NEWER,
        Clientbound,
        Play,
        "sound",
        &ranged,
    );
    truncations_fail(
        UP.unwrap(),
        NEWER,
        OLDER,
        Clientbound,
        Play,
        "custom_sound",
        &custom,
    );
    // Every sound the 1.19.3 server sent reaches the 1.19.1 client.
    if let Some(sounds) = recorded(NEWER, Clientbound, Play, "sound") {
        let mut down = down();
        for sound in sounds {
            down.clientbound(Play, "sound", &sound).unwrap();
        }
    }
}

fn alex(action: Action) -> Entry {
    let mut entry = Entry {
        uuid: 0xa1e,
        ..Entry::default()
    };
    match action {
        Action::Add => {
            entry.name = "Alex".into();
            entry.properties = bytes(|w| {
                w.string("textures").string("e30=").bool(false);
            });
            entry.property_count = 1;
            entry.game_mode = 1;
            entry.latency = 30;
            entry.display_name = Some(r#"{"text":"A"}"#.into());
        }
        Action::GameMode => entry.game_mode = 3,
        Action::Latency => entry.latency = 12,
        Action::DisplayName => entry.display_name = None,
        Action::Remove => {}
    }
    entry
}

#[test]
fn the_player_list_splits_and_merges() {
    for action in [
        Action::Add,
        Action::GameMode,
        Action::Latency,
        Action::DisplayName,
    ] {
        let legacy = players::write_legacy(OLDER, action, &[alex(action)]);
        let out = up().one(Clientbound, Play, "player_info", &legacy);
        assert_eq!(out.name, "player_info_update");
        let (actions, entries) = players::read_update(&out.data).unwrap();
        // The text codec sets the actions the entry carries: a 1.19.2 player
        // has no chat session to initialise.
        assert_eq!(
            (actions, entries),
            (
                players::update_actions(action) & !players::INITIALIZE_CHAT,
                vec![alex(action)]
            )
        );
        let back = down().one(Clientbound, Play, "player_info_update", &out.data);
        assert_eq!(back.data, legacy);
    }
    let remove = players::write_legacy(OLDER, Action::Remove, &[alex(Action::Remove)]);
    let out = up().one(Clientbound, Play, "player_info", &remove);
    assert_eq!(
        (out.name.as_str(), out.data.as_slice()),
        (
            "player_info_remove",
            &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x0a, 0x1e][..]
        )
    );
    assert_eq!(
        down()
            .one(Clientbound, Play, "player_info_remove", &out.data)
            .data,
        remove
    );
    // An update of latency and display name together: one packet each.
    let both = players::write_update(
        players::LATENCY | players::DISPLAY_NAME,
        &[alex(Action::Latency)],
    );
    let out = down()
        .clientbound(Play, "player_info_update", &both)
        .unwrap();
    assert_eq!(out.len(), 2);
    // Every player list update the 1.19.3 server sent reaches 1.19.1.
    if let Some(updates) = recorded(NEWER, Clientbound, Play, "player_info_update") {
        for update in updates {
            let out = down()
                .clientbound(Play, "player_info_update", &update)
                .unwrap();
            for packet in out {
                players::read_legacy(OLDER, &packet.data).unwrap();
            }
        }
    }
    truncations_fail(
        DOWN.unwrap(),
        OLDER,
        NEWER,
        Clientbound,
        Play,
        "player_info_update",
        &both,
    );
}

#[test]
fn login_phase_drops_keys() {
    let (Some(newer), Some(older)) = (
        first(NEWER, Serverbound, LoginState, "hello"),
        first(OLDER, Serverbound, LoginState, "hello"),
    ) else {
        return;
    };
    // The same player as the real 1.19.2 client sent it.
    assert_eq!(
        up().one(Serverbound, LoginState, "hello", &newer).data,
        older
    );
    assert_eq!(
        down().one(Serverbound, LoginState, "hello", &older).data,
        newer
    );
    let key = bytes(|w| {
        w.byte_array(&[1; 128]).byte_array(&[2; 128]);
    });
    let keyed = bytes(|w| {
        w.byte_array(&[1; 128]).bool(true).byte_array(&[2; 128]);
    });
    assert_eq!(up().one(Serverbound, LoginState, "key", &key).data, keyed);
    assert_eq!(down().one(Serverbound, LoginState, "key", &keyed).data, key);
    truncations_fail(
        UP.unwrap(),
        NEWER,
        OLDER,
        Serverbound,
        LoginState,
        "key",
        &key,
    );
}

#[test]
fn server_data_drops_previews() {
    let (Some(older), Some(newer)) = (
        first(OLDER, Clientbound, Play, "server_data"),
        first(NEWER, Clientbound, Play, "server_data"),
    ) else {
        return;
    };
    let out = up().one(Clientbound, Play, "server_data", &older);
    assert_eq!(out.data, [&older[..older.len() - 2], &[0]].concat());
    let out = down().one(Clientbound, Play, "server_data", &newer);
    assert_eq!(out.data, [&newer[..newer.len() - 1], &[0, 0]].concat());
    let mut up = up();
    for name in [
        "player_chat_header",
        "delete_chat",
        "chat_preview",
        "set_display_chat_preview",
    ] {
        assert!(up.clientbound(Play, name, &[]).unwrap().is_empty());
    }
}
