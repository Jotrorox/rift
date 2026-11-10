use super::{NEWER, OLDER, down::Down, up::Up};
use crate::{
    Direction, State,
    steps::v1_16_4_to_1_17::common::{
        Login, Respawn,
        testing::{recorded, replay, send},
    },
};
use Direction::Clientbound;
use State::Play;

const UP: (i32, i32) = (NEWER, OLDER);
const DOWN: (i32, i32) = (OLDER, NEWER);

#[test]
fn every_packet_has_a_route() {
    assert_eq!(Up::routes().problems(), &[] as &[String]);
    assert_eq!(Down::routes().problems(), &[] as &[String]);
}

/// The `infiniburn` value of every dimension type of a join game body.
fn infiniburns(protocol: i32, body: &[u8]) -> Vec<String> {
    let mut login = Login::read(protocol, body).unwrap();
    let mut values = Vec::new();
    login
        .edit_dimension_types(|dimension| {
            values.push(dimension.str("infiniburn").unwrap().to_owned());
            Ok(())
        })
        .unwrap();
    values
}

fn respawn_infiniburn(body: &[u8]) -> String {
    let mut respawn = Respawn::read(body).unwrap();
    respawn
        .dimension_type()
        .unwrap()
        .str("infiniburn")
        .unwrap()
        .to_owned()
}

#[test]
fn infiniburn_matches_what_each_server_sends() {
    let (mut up, mut down) = ((super::UP.unwrap())(), (super::DOWN.unwrap())());
    let (old, new) = (
        recorded(OLDER, Clientbound, "login"),
        recorded(NEWER, Clientbound, "login"),
    );
    if old.is_empty() || new.is_empty() {
        eprintln!("no corpus; skipped");
        return;
    }
    let upgraded = send(&mut *up, UP, Clientbound, Play, "login", &old[0])
        .unwrap()
        .remove(0);
    let mut expected = infiniburns(NEWER, &new[0]);
    let mut got = infiniburns(NEWER, &upgraded.data);
    assert!(got.iter().all(|value| value.starts_with('#')), "{got:?}");
    expected.sort();
    got.sort();
    assert_eq!(got, expected);
    let back = send(&mut *down, DOWN, Clientbound, Play, "login", &upgraded.data)
        .unwrap()
        .remove(0);
    assert_eq!(back.data, old[0]);
    let downgraded = send(&mut *down, DOWN, Clientbound, Play, "login", &new[0])
        .unwrap()
        .remove(0);
    assert!(
        infiniburns(OLDER, &downgraded.data)
            .iter()
            .all(|value| !value.starts_with('#'))
    );

    for body in recorded(OLDER, Clientbound, "respawn") {
        let out = send(&mut *up, UP, Clientbound, Play, "respawn", &body)
            .unwrap()
            .remove(0);
        assert!(respawn_infiniburn(&out.data).starts_with('#'));
        let back = send(&mut *down, DOWN, Clientbound, Play, "respawn", &out.data)
            .unwrap()
            .remove(0);
        assert_eq!(back.data, body);
    }
    assert!(send(&mut *up, UP, Clientbound, Play, "respawn", &[10, 0, 0, 0]).is_err());
}

#[test]
fn recorded_sessions_replay_through_both_steps() {
    for (mut step, sides, clientbound, serverbound) in [
        ((super::UP.unwrap())(), UP, OLDER, NEWER),
        ((super::DOWN.unwrap())(), DOWN, NEWER, OLDER),
    ] {
        for (direction, recorded) in [
            (Clientbound, clientbound),
            (Direction::Serverbound, serverbound),
        ] {
            let Some(result) = replay(&mut *step, sides, recorded, direction) else {
                continue;
            };
            assert!(result.failed.is_empty(), "{:?}", result.failed);
            assert!(result.awaiting_codec.is_empty());
        }
    }
}
