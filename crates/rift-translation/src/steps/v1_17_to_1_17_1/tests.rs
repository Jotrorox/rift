use super::{NEWER, OLDER, down::Down, up::Up};
use crate::{
    Direction, State,
    step::Step,
    steps::v1_16_4_to_1_17::common::testing::{
        Out, awaiting_codec, hex, recorded, replay, send, with_stand_ins,
    },
    wire::{Reader, Writer},
};
use Direction::{Clientbound, Serverbound};
use State::Play;

const UP: (i32, i32) = (NEWER, OLDER);
const DOWN: (i32, i32) = (OLDER, NEWER);

fn up() -> Box<dyn Step> {
    (super::UP.unwrap())()
}

fn down() -> Box<dyn Step> {
    (super::DOWN.unwrap())()
}

fn one(outs: Vec<Out>) -> Out {
    assert_eq!(outs.len(), 1, "{outs:?}");
    outs.into_iter().next().unwrap()
}

/// A 1.17.1 slot packet without the state number after its window byte.
fn without_state(data: &[u8]) -> Vec<u8> {
    let mut r = Reader::new(&data[1..]);
    r.varint().unwrap();
    [&data[..1], r.peek()].concat()
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(Up::routes().problems(), &[] as &[String]);
    assert_eq!(Down::routes().problems(), &[] as &[String]);
}

#[test]
fn slot_packets_match_what_a_1_17_1_server_sends() {
    with_stand_ins(|| {
        let mut up = up();
        let old = recorded(OLDER, Clientbound, "container_set_slot");
        let new = recorded(NEWER, Clientbound, "container_set_slot");
        if old.is_empty() || new.is_empty() {
            eprintln!("no corpus; skipped");
            return;
        }
        // 1.17 sets the empty cursor with a slot packet of its own first.
        assert_eq!(old[0], hex("ffffff00"));
        let mut states = Vec::new();
        let converted: Vec<_> = old
            .iter()
            .map(|body| {
                let out =
                    one(send(&mut *up, UP, Clientbound, Play, "container_set_slot", body).unwrap());
                states.push(Reader::new(&out.data[1..]).varint().unwrap());
                without_state(&out.data)
            })
            .collect();
        let real: Vec<_> = new.iter().map(|body| without_state(body)).collect();
        assert_eq!(converted[1..], real[..]);
        assert!(states.windows(2).all(|pair| pair[0] != pair[1]));

        let old = recorded(OLDER, Clientbound, "container_set_content");
        let new = recorded(NEWER, Clientbound, "container_set_content");
        let out = one(send(
            &mut *up,
            UP,
            Clientbound,
            Play,
            "container_set_content",
            &old[0],
        )
        .unwrap());
        assert_eq!(without_state(&out.data), without_state(&new[0]));

        let mut down = down();
        let outs = send(
            &mut *down,
            DOWN,
            Clientbound,
            Play,
            "container_set_content",
            &new[0],
        )
        .unwrap();
        let bodies: Vec<_> = outs
            .iter()
            .map(|out| (out.name.as_str(), out.data.clone()))
            .collect();
        assert_eq!(
            bodies,
            [
                ("container_set_content", old[0].clone()),
                ("container_set_slot", hex("ffffff00"))
            ]
        );
        for (body, expected) in recorded(NEWER, Clientbound, "container_set_slot")
            .iter()
            .zip(&recorded(OLDER, Clientbound, "container_set_slot")[1..])
        {
            let out = one(send(
                &mut *down,
                DOWN,
                Clientbound,
                Play,
                "container_set_slot",
                body,
            )
            .unwrap());
            assert_eq!(&out.data, expected);
        }
    });
}

#[test]
fn the_cursor_reaches_1_17_1_with_the_contents() {
    with_stand_ins(|| {
        let mut up = up();
        let mut cursor = Writer::new();
        cursor.i8(-1).i16(-1).bool(true).varint(135).i8(1).u8(0);
        let out = one(send(
            &mut *up,
            UP,
            Clientbound,
            Play,
            "container_set_slot",
            &cursor.buf,
        )
        .unwrap());
        assert_eq!(out.data[..3], [0xff, 1, 0xff]);
        let mut content = Writer::new();
        content.u8(0).i16(1).u8(0);
        let out = one(send(
            &mut *up,
            UP,
            Clientbound,
            Play,
            "container_set_content",
            &content.buf,
        )
        .unwrap());
        let mut expected = Writer::new();
        expected
            .u8(0)
            .varint(2)
            .varint(1)
            .u8(0)
            .bool(true)
            .varint(135)
            .i8(1)
            .u8(0);
        assert_eq!(out.data, expected.buf);
        assert!(
            send(
                &mut *up,
                UP,
                Clientbound,
                Play,
                "container_set_content",
                &[0, 0xff, 0xff]
            )
            .is_err()
        );
    });
}

#[test]
fn clicks_gain_and_lose_the_state_number() {
    with_stand_ins(|| {
        let mut down = down();
        // The backend's container 3 is at state 9, the inventory at 4.
        let mut slot = Writer::new();
        slot.i8(3).varint(9).i16(0).u8(0);
        send(
            &mut *down,
            DOWN,
            Clientbound,
            Play,
            "container_set_slot",
            &slot.buf,
        )
        .unwrap();
        let mut slot = Writer::new();
        slot.i8(0).varint(4).i16(0).u8(0);
        send(
            &mut *down,
            DOWN,
            Clientbound,
            Play,
            "container_set_slot",
            &slot.buf,
        )
        .unwrap();

        let mut old = Writer::new();
        old.u8(3)
            .i16(5)
            .i8(1)
            .varint(0)
            .varint(1)
            .i16(5)
            .u8(0)
            .bool(true)
            .varint(1)
            .i8(2)
            .u8(0);
        let out = one(send(
            &mut *down,
            DOWN,
            Serverbound,
            Play,
            "container_click",
            &old.buf,
        )
        .unwrap());
        let mut new = Writer::new();
        new.u8(3).varint(9).bytes(&old.buf[1..]);
        assert_eq!(out.data, new.buf);
        let mut up = up();
        let back = one(send(
            &mut *up,
            UP,
            Serverbound,
            Play,
            "container_click",
            &out.data,
        )
        .unwrap());
        assert_eq!(back.data, old.buf);

        let mut inventory = old.buf.clone();
        inventory[0] = 0;
        let out = one(send(
            &mut *down,
            DOWN,
            Serverbound,
            Play,
            "container_click",
            &inventory,
        )
        .unwrap());
        assert_eq!(out.data[..2], [0, 4]);
        // An unknown window: the backend resends the menu.
        inventory[0] = 7;
        let out = one(send(
            &mut *down,
            DOWN,
            Serverbound,
            Play,
            "container_click",
            &inventory,
        )
        .unwrap());
        assert_eq!(out.data[..2], [7, 0]);
        assert!(
            send(
                &mut *down,
                DOWN,
                Serverbound,
                Play,
                "container_click",
                &old.buf[..6]
            )
            .is_err()
        );
    });
}

#[test]
fn item_codec_is_called_for_every_stack() {
    let mut down = down();
    let mut slot = Writer::new();
    slot.i8(0).varint(1).i16(0).u8(0);
    match send(
        &mut *down,
        DOWN,
        Clientbound,
        Play,
        "container_set_slot",
        &slot.buf,
    ) {
        Ok(out) => assert_eq!(one(out).data, hex("00000000")),
        Err(error) => assert!(awaiting_codec(&error), "{error}"),
    }
}

#[test]
fn entity_removal_is_merged_and_split() {
    let (mut up, mut down) = (up(), down());
    let out = one(send(&mut *up, UP, Clientbound, Play, "remove_entity", &[2]).unwrap());
    // What the 1.17.1 server sent for the same removal.
    assert_eq!(
        (out.name.as_str(), out.data),
        ("remove_entities", hex("0102"))
    );
    let outs = send(
        &mut *down,
        DOWN,
        Clientbound,
        Play,
        "remove_entities",
        &hex("020203"),
    )
    .unwrap();
    let ids: Vec<_> = outs
        .iter()
        .map(|out| (out.name.as_str(), out.data.clone()))
        .collect();
    assert_eq!(
        ids,
        [("remove_entity", vec![2]), ("remove_entity", vec![3])]
    );
    assert!(
        send(
            &mut *down,
            DOWN,
            Clientbound,
            Play,
            "remove_entities",
            &hex("0302")
        )
        .is_err()
    );
}

#[test]
fn book_edits_convert_both_ways() {
    let (mut up, mut down) = (up(), down());
    let mut new = Writer::new();
    new.varint(2).varint(1).string("hello").bool(false);
    let old = one(send(&mut *up, UP, Serverbound, Play, "edit_book", &new.buf).unwrap());
    let back = one(send(&mut *down, DOWN, Serverbound, Play, "edit_book", &old.data).unwrap());
    assert_eq!(back.data, new.buf);
}

#[test]
fn recorded_sessions_replay_through_both_steps() {
    for stand_ins in [false, true] {
        let run = || {
            for (name, mut step, sides, clientbound, serverbound) in [
                ("up", up(), UP, OLDER, NEWER),
                ("down", down(), DOWN, NEWER, OLDER),
            ] {
                for (direction, recorded) in
                    [(Clientbound, clientbound), (Serverbound, serverbound)]
                {
                    let Some(result) = replay(&mut *step, sides, recorded, direction) else {
                        eprintln!("no corpus for {recorded}; skipped");
                        continue;
                    };
                    assert!(
                        result.failed.is_empty(),
                        "{name} {direction:?}: {:?}",
                        result.failed
                    );
                    if stand_ins {
                        assert!(result.awaiting_codec.is_empty());
                    }
                }
            }
        };
        if stand_ins {
            with_stand_ins(run);
        } else {
            run();
        }
    }
}

#[test]
fn garbage_never_panics() {
    let garbage: [&[u8]; 5] = [
        &[],
        &[0xff],
        &[0x80; 8],
        &[1, 0xff, 0xff, 0xff, 0x0f],
        &[0x7f; 40],
    ];
    with_stand_ins(|| {
        for (mut step, sides, direction) in [
            (up(), UP, Clientbound),
            (up(), UP, Serverbound),
            (down(), DOWN, Clientbound),
            (down(), DOWN, Serverbound),
        ] {
            let source = match direction {
                Clientbound => sides.1,
                Serverbound => sides.0,
            };
            for name in crate::data::version(source)
                .packets(Play, direction)
                .names()
            {
                for body in garbage {
                    let _ = send(&mut *step, sides, direction, Play, name, body);
                }
            }
        }
    });
}
