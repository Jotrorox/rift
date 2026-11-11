use super::{NEWER, OLDER, down, up};
use crate::{
    Direction, mapping,
    nbt::{Compound, Nbt},
    steps::v1_14_4_to_1_15::support::harness::{Harness, body, replay, truncations_fail},
};

fn upgrading() -> Harness {
    Harness::new((up::STEP.unwrap())(), NEWER, OLDER)
}

fn downgrading() -> Harness {
    Harness::new((down::STEP.unwrap())(), OLDER, NEWER)
}

/// A writable book with one page, signing or not, then `hand`.
fn edit_book(hand: i32) -> Vec<u8> {
    let mut tag = Compound::new();
    tag.insert("pages", vec![Nbt::from("hello")]);
    body(|w| {
        w.bool(true).varint(825).i8(1);
        Nbt::Compound(tag).write(w, true);
        w.bool(false).varint(hand);
    })
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(up::Up::routes().problems(), &[] as &[String]);
    assert_eq!(down::Down::routes().problems(), &[] as &[String]);
}

#[test]
fn items_need_no_conversion() {
    for (from, to) in [(OLDER, NEWER), (NEWER, OLDER)] {
        assert!(mapping::mapping(from, to, "item").is_identity());
    }
}

#[test]
fn edited_books_name_their_slot_or_hand() {
    let mut step = downgrading();
    assert_eq!(
        step.one(
            Direction::Serverbound,
            "edit_book",
            &edit_book(0),
            "edit_book"
        ),
        edit_book(0)
    );
    assert_eq!(
        step.one(
            Direction::Serverbound,
            "edit_book",
            &edit_book(1),
            "edit_book"
        ),
        edit_book(40)
    );
    // The client picks slot 3, the backend then slot 6.
    step.serverbound("set_carried_item", &3i16.to_be_bytes())
        .unwrap();
    assert_eq!(
        step.one(
            Direction::Serverbound,
            "edit_book",
            &edit_book(0),
            "edit_book"
        ),
        edit_book(3)
    );
    step.clientbound("set_held_slot", &[6]).unwrap();
    assert_eq!(
        step.one(
            Direction::Serverbound,
            "edit_book",
            &edit_book(0),
            "edit_book"
        ),
        edit_book(6)
    );
    step.serverbound("set_carried_item", &12i16.to_be_bytes())
        .unwrap();
    assert_eq!(
        step.one(
            Direction::Serverbound,
            "edit_book",
            &edit_book(0),
            "edit_book"
        ),
        edit_book(6)
    );
    assert!(step.serverbound("edit_book", &edit_book(2)).is_err());

    let mut step = upgrading();
    for (slot, hand) in [(0, 0), (5, 0), (40, 1)] {
        assert_eq!(
            step.one(
                Direction::Serverbound,
                "edit_book",
                &edit_book(slot),
                "edit_book"
            ),
            edit_book(hand)
        );
    }
    // An empty stack, as a client may send.
    let empty = body(|w| {
        w.bool(false).bool(false).varint(40);
    });
    let expected = body(|w| {
        w.bool(false).bool(false).varint(1);
    });
    assert_eq!(
        step.one(Direction::Serverbound, "edit_book", &empty, "edit_book"),
        expected
    );
}

#[test]
fn malformed_packets_fail() {
    truncations_fail(
        &mut upgrading(),
        Direction::Serverbound,
        "edit_book",
        &edit_book(40),
    );
    truncations_fail(
        &mut downgrading(),
        Direction::Serverbound,
        "edit_book",
        &edit_book(1),
    );
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
