use super::{DOWN, NEWER, OLDER, UP};
use crate::{
    Direction::{Clientbound, Serverbound},
    State::{Login as LoginState, Play},
    nbt::Nbt,
    steps::v1_18_2_to_1_19::{
        chat,
        join::Login,
        messages::{Placement, ServerData, SystemChat},
        testing::{Harness, first, hex, truncations_fail},
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

fn system(protocol: i32, data: &[u8]) -> (Value, Placement) {
    let message = SystemChat::read(protocol, data).unwrap();
    (
        serde_json::from_str(&message.json).unwrap(),
        message.placement,
    )
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(super::up::Up::routes().problems(), &[] as &[String]);
    assert_eq!(super::down::Down::routes().problems(), &[] as &[String]);
}

/// `/say hello from console`, as both versions' servers send it.
const SAY_1_19: &str = "1d7b2274657874223a2268656c6c6f2066726f6d20636f6e736f6c65227d00030000000000000000\
    0000000000000000117b2274657874223a22536572766572227d00000001a1c4060519000000000000000000";
const SAY_1_19_1: &str = "0000000000000000000000000000000000001268656c6c6f2066726f6d20636f6e736f6c65000000\
    01a1c4066daf4177d0a82c7e9eda00000001117b2274657874223a22536572766572227d00";

fn say_display() -> Value {
    json!({"translate": "chat.type.announcement",
           "with": [{"text": "Server"}, {"text": "hello from console"}]})
}

#[test]
fn player_chat_becomes_decorated_system_chat_both_ways() {
    // The backend's chat types come from its login.
    let mut up = up();
    if let Some(login) = first(OLDER, Clientbound, Play, "login") {
        up.clientbound(Play, "login", &login).unwrap();
    }
    let out = up.one(Clientbound, Play, "player_chat", &hex(SAY_1_19));
    assert_eq!(out.name, "system_chat");
    assert_eq!(
        system(NEWER, &out.data),
        (say_display(), Placement::Overlay(false))
    );

    let mut down = down();
    if let Some(login) = first(NEWER, Clientbound, Play, "login") {
        down.clientbound(Play, "login", &login).unwrap();
    }
    let out = down.one(Clientbound, Play, "player_chat", &hex(SAY_1_19_1));
    assert_eq!(out.name, "system_chat");
    assert_eq!(
        system(OLDER, &out.data),
        (say_display(), Placement::ChatType(chat::SYSTEM_1_19))
    );
    for (make, client, backend, data) in [
        (UP.unwrap(), NEWER, OLDER, hex(SAY_1_19)),
        (DOWN.unwrap(), OLDER, NEWER, hex(SAY_1_19_1)),
    ] {
        truncations_fail(
            make,
            client,
            backend,
            Clientbound,
            Play,
            "player_chat",
            &data,
        );
    }
}

#[test]
fn system_chat_placement_follows_the_chat_type() {
    let tellraw = |placement: &[u8]| {
        let mut data = bytes(|w| {
            w.string(r#"{"text":"raw"}"#);
        });
        data.extend_from_slice(placement);
        data
    };
    let mut up = up();
    let mut codec = crate::nbt::Compound::new();
    codec.insert("minecraft:chat_type", chat::vanilla_1_19());
    // Without a login the vanilla types are unknown: nothing is overlay.
    let out = up.one(Clientbound, Play, "system_chat", &tellraw(&[7]));
    assert_eq!(out.data, tellraw(&[0]));
    if let Some(login) = first(OLDER, Clientbound, Play, "login") {
        up.clientbound(Play, "login", &login).unwrap();
        let out = up.one(Clientbound, Play, "system_chat", &tellraw(&[2]));
        assert_eq!(out.data, tellraw(&[1]));
    }
    let mut down = down();
    assert_eq!(
        down.one(Clientbound, Play, "system_chat", &tellraw(&[1]))
            .data,
        tellraw(&[2])
    );
    assert_eq!(
        down.one(Clientbound, Play, "system_chat", &tellraw(&[0]))
            .data,
        tellraw(&[1])
    );
    truncations_fail(
        DOWN.unwrap(),
        OLDER,
        NEWER,
        Clientbound,
        Play,
        "system_chat",
        &tellraw(&[1]),
    );
}

#[test]
fn login_swaps_the_chat_type_registry_and_keeps_the_rest() {
    for (make, client, backend) in [(UP.unwrap(), NEWER, OLDER), (DOWN.unwrap(), OLDER, NEWER)] {
        let (Some(sent), Some(native)) = (
            first(backend, Clientbound, Play, "login"),
            first(client, Clientbound, Play, "login"),
        ) else {
            return;
        };
        // Each version's own registry is what its vanilla server sends.
        let native = Login::read(client, &native).unwrap();
        let vanilla = if client == NEWER {
            chat::vanilla_1_19_1()
        } else {
            chat::vanilla_1_19()
        };
        assert_eq!(
            native.codec.get("minecraft:chat_type"),
            Some(&Nbt::Compound(vanilla))
        );

        let original = Login::read(backend, &sent).unwrap();
        assert_eq!(original.write(backend).unwrap(), sent, "login round trip");
        let out = Harness::new(make, client, backend).one(Clientbound, Play, "login", &sent);
        let translated = Login::read(client, &out.data).unwrap();
        assert_eq!(
            translated.codec.get("minecraft:chat_type"),
            native.codec.get("minecraft:chat_type")
        );
        let mut expected = original.clone();
        expected.codec = translated.codec.clone();
        assert_eq!(translated, expected);
        truncations_fail(
            make,
            client,
            backend,
            Clientbound,
            Play,
            "login",
            &sent[..200],
        );
    }
}

#[test]
fn server_data_announces_unsigned_chat() {
    let Some(sent) = first(OLDER, Clientbound, Play, "server_data") else {
        return;
    };
    let out = up().one(Clientbound, Play, "server_data", &sent);
    // The motd, no icon, no previews, secure chat not enforced.
    let mut expected = sent.clone();
    expected.push(0);
    assert_eq!(out.data, expected);
    let back = down().one(Clientbound, Play, "server_data", &out.data);
    assert_eq!(back.data, sent);
    assert_eq!(
        ServerData::read(OLDER, &back.data).unwrap().motd.unwrap(),
        r#"{"text":"rift-vanilla-1.19-test"}"#
    );
}

#[test]
fn hello_carries_the_uuid_only_where_it_exists() {
    let (Some(newer), Some(older)) = (
        first(NEWER, Serverbound, LoginState, "hello"),
        first(OLDER, Serverbound, LoginState, "hello"),
    ) else {
        return;
    };
    // What the real 1.19 client sent for the same player.
    assert_eq!(
        up().one(Serverbound, LoginState, "hello", &newer).data,
        older
    );
    let out = down().one(Serverbound, LoginState, "hello", &older);
    assert_eq!(out.data, hex("06436f727075730000"));
    truncations_fail(
        UP.unwrap(),
        NEWER,
        OLDER,
        Serverbound,
        LoginState,
        "hello",
        &newer,
    );
}

fn chat_packet(protocol: i32) -> Vec<u8> {
    bytes(|w| {
        w.string("hi")
            .i64(1_700_000_000_000)
            .i64(42)
            .byte_array(&[7; 4])
            .bool(false);
        if protocol == NEWER {
            // One last-seen entry and a rejected one.
            w.varint(1)
                .uuid(5)
                .byte_array(&[1])
                .bool(true)
                .uuid(6)
                .byte_array(&[2]);
        }
    })
}

#[test]
fn serverbound_chat_is_sent_unsigned() {
    let out = up().one(Serverbound, Play, "chat", &chat_packet(NEWER));
    assert_eq!(
        out.data,
        bytes(|w| {
            w.string("hi")
                .i64(1_700_000_000_000)
                .i64(42)
                .byte_array(&[])
                .bool(false);
        })
    );
    let out = down().one(Serverbound, Play, "chat", &chat_packet(OLDER));
    assert_eq!(
        out.data,
        bytes(|w| {
            w.string("hi")
                .i64(1_700_000_000_000)
                .i64(42)
                .byte_array(&[])
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
        "chat",
        &chat_packet(NEWER),
    );

    let command = |protocol: i32| {
        bytes(|w| {
            w.string("tp 0 0 0")
                .i64(9)
                .i64(3)
                .varint(1)
                .string("x")
                .byte_array(&[1, 2])
                .bool(true);
            if protocol == NEWER {
                w.varint(0).bool(false);
            }
        })
    };
    let out = up().one(Serverbound, Play, "chat_command", &command(NEWER));
    assert_eq!(
        out.data,
        bytes(|w| {
            w.string("tp 0 0 0").i64(9).i64(3).varint(0).bool(false);
        })
    );
    let out = down().one(Serverbound, Play, "chat_command", &command(OLDER));
    assert_eq!(
        out.data,
        bytes(|w| {
            w.string("tp 0 0 0")
                .i64(9)
                .i64(3)
                .varint(0)
                .bool(false)
                .varint(0)
                .bool(false);
        })
    );
    assert!(
        up().serverbound(Play, "chat_ack", &[0, 0])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn chat_features_the_older_client_lacks_are_dropped() {
    let mut down = down();
    for name in [
        "player_chat_header",
        "delete_chat",
        "custom_chat_completions",
    ] {
        assert!(down.clientbound(Play, name, &[]).unwrap().is_empty());
    }
}

#[test]
fn velocity_forwarding_requests_are_clamped() {
    let request = bytes(|w| {
        w.varint(1).string("velocity:player_info").u8(3);
    });
    for mut harness in [up(), down()] {
        let out = harness.one(Clientbound, LoginState, "custom_query", &request);
        assert_eq!(
            out.data,
            bytes(|w| {
                w.varint(1).string("velocity:player_info").u8(1);
            })
        );
    }
}
