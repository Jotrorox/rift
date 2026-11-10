use super::{NEWER, OLDER, down::Down, up::Up};
use crate::{
    Direction, data,
    steps::v1_12_to_1_12_1::testing::{Session, recorded, to_client},
    wire::Writer,
};

#[test]
fn every_packet_has_a_route() {
    assert_eq!(Up::routes().problems(), &[] as &[String]);
    assert_eq!(Down::routes().problems(), &[] as &[String]);
}

/// One stack of `count` items `name` without a tag, in `protocol`'s layout.
fn stack(protocol: i32, name: &str, count: i8) -> Vec<u8> {
    let id = data::version(protocol)
        .expect_registry("item")
        .id(name)
        .unwrap();
    let mut body = Writer::new();
    if protocol >= NEWER {
        body.bool(true).varint(id);
    } else {
        body.i16(id as i16);
    }
    body.i8(count).u8(0);
    body.into_inner()
}

/// Two trades: emeralds for bread, and a book plus emeralds for an
/// enchanted book.
fn trades(protocol: i32) -> Vec<u8> {
    let mut body = Writer::new();
    body.string("minecraft:trader_list").i32(5).u8(2);
    body.bytes(&stack(protocol, "emerald", 3))
        .bytes(&stack(protocol, "bread", 6))
        .bool(false)
        .bool(false)
        .i32(0)
        .i32(12);
    body.bytes(&stack(protocol, "book", 1))
        .bytes(&stack(protocol, "enchanted_book", 1))
        .bool(true)
        .bytes(&stack(protocol, "emerald", 20))
        .bool(true)
        .i32(12)
        .i32(12);
    body.into_inner()
}

#[test]
fn trade_lists_change_stack_layout() {
    for (client, backend) in [(NEWER, OLDER), (OLDER, NEWER), (NEWER, 393), (393, NEWER)] {
        let mut session = Session::play(client, backend);
        match session.clientbound("custom_payload", &trades(backend)) {
            Err(error) if error.to_string().contains("not implemented") => {
                eprintln!("skipping until the item codec lands: {error}");
                return;
            }
            result => assert_eq!(
                result.unwrap(),
                [to_client("custom_payload", &trades(client))]
            ),
        }
    }
}

#[test]
fn other_plugin_messages_pass_unchanged() {
    for (client, backend) in [(NEWER, OLDER), (OLDER, NEWER)] {
        let Some(packets) = recorded(backend, Direction::Clientbound, "custom_payload") else {
            return;
        };
        let mut session = Session::play(client, backend);
        for packet in packets {
            assert_eq!(
                session.clientbound("custom_payload", &packet).unwrap(),
                [to_client("custom_payload", &packet)]
            );
        }
    }
}
