use super::{
    NEWER, OLDER,
    down::Down,
    testing::{Session, recorded, to_client},
    up::Up,
};
use crate::{Direction, codec::testing::corpus};

#[test]
fn every_packet_has_a_route() {
    assert_eq!(Up::routes().problems(), &[] as &[String]);
    assert_eq!(Down::routes().problems(), &[] as &[String]);
}

#[test]
fn a_recipe_placement_shows_the_ghost_recipe_on_a_1_12_backend() {
    let mut session = Session::play(NEWER, OLDER);
    // Window 1, recipe 404, not "make all".
    let out = session
        .serverbound("place_recipe", &[0x01, 0x94, 0x03, 0x00])
        .unwrap();
    assert_eq!(out, [to_client("place_ghost_recipe", &[0x01, 0x94, 0x03])]);
    // Shift-clicked, in the player's own 2x2 grid.
    let out = session
        .serverbound("place_recipe", &[0x00, 0x05, 0x01])
        .unwrap();
    assert_eq!(out, [to_client("place_ghost_recipe", &[0x00, 0x05])]);
}

#[test]
fn malformed_recipe_placements_fail() {
    let mut session = Session::play(NEWER, OLDER);
    for body in [
        &[][..],
        &[0x01, 0x94],
        &[0x01, 0x05],
        &[0x01, 0x05, 0x02],
        &[0x01, 0x05, 0x00, 0x00],
    ] {
        assert!(
            session.serverbound("place_recipe", body).is_err(),
            "{body:?}"
        );
    }
}

#[test]
fn recipe_placements_vanish_for_a_1_12_client() {
    let mut session = Session::play(OLDER, NEWER);
    assert_eq!(
        session
            .clientbound("place_ghost_recipe", &[0x01, 0x94, 0x03])
            .unwrap(),
        []
    );
    // Window 1, action 7, no returned and one prepared stone into slot 1.
    let placement = [
        0x01, 0x00, 0x07, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x01, 0x24,
    ];
    assert_eq!(
        session
            .serverbound("crafting_recipe_placement", &placement)
            .unwrap(),
        []
    );
}

/// Apart from recipe placement nothing changed: every packet of a real
/// session crosses the pair, in both orientations and through the chain to
/// 1.12.2, with its body untouched.
#[test]
fn real_sessions_pass_through_unchanged() {
    for (client, backend) in [(NEWER, OLDER), (OLDER, NEWER), (340, OLDER), (OLDER, 340)] {
        let (Some(from_backend), Some(from_client)) = (corpus(backend), corpus(client)) else {
            eprintln!("no corpus for {client} or {backend}; skipping");
            return;
        };
        let mut session = Session::play(client, backend);
        let packets = from_backend
            .iter()
            .filter(|packet| packet.direction == Direction::Clientbound)
            .chain(
                from_client
                    .iter()
                    .filter(|packet| packet.direction == Direction::Serverbound),
            )
            .filter(|packet| packet.state == crate::State::Play);
        let mut count = 0;
        for packet in packets {
            let (sender, out) = match packet.direction {
                Direction::Clientbound => (
                    backend,
                    session.clientbound_id(packet.id, &packet.data).unwrap(),
                ),
                Direction::Serverbound => (
                    client,
                    session.serverbound_id(packet.id, &packet.data).unwrap(),
                ),
            };
            let name = packet.name(sender);
            if name == "keep_alive" && (client == 340 || backend == 340) {
                // 1.12.2 widened keep-alive IDs; see that pair's tests.
                assert_eq!(out.len(), 1);
                continue;
            }
            assert_eq!(out.len(), 1, "{name}");
            assert_eq!((out[0].name.as_str(), &out[0].data), (name, &packet.data));
            count += 1;
        }
        assert!(count > 800, "{client} on {backend}: {count} packets");
    }
}

#[test]
fn the_corpus_has_no_recipe_placements_to_lose() {
    // The recorded scenario never clicks the recipe book; the hand-built
    // packets above stand in for real ones.
    if let Some(packets) = recorded(NEWER, Direction::Serverbound, "place_recipe") {
        assert!(packets.is_empty());
    }
}
