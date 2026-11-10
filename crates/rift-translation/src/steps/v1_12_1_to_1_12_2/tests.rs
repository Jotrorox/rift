use super::{NEWER, OLDER, down::Down, up::Up};
use crate::{
    Direction,
    steps::v1_12_to_1_12_1::testing::{Session, hex, recorded, to_backend, to_client},
    wire::{Reader, Writer},
};

fn varint(value: i32) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.varint(value);
    writer.into_inner()
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(Up::routes().problems(), &[] as &[String]);
    assert_eq!(Down::routes().problems(), &[] as &[String]);
}

#[test]
fn keep_alives_widen_for_a_1_12_2_client() {
    let mut session = Session::play(NEWER, OLDER);
    // Recorded from the official 1.12.1 server: 110_659_855 ms.
    let request = hex("8f92e234");
    let id = Reader::new(&request).varint().unwrap();
    assert_eq!(id, 110_659_855);
    let long = i64::from(id).to_be_bytes();
    assert_eq!(
        session.clientbound("keep_alive", &request).unwrap(),
        [to_client("keep_alive", &long)]
    );
    assert_eq!(
        session.serverbound("keep_alive", &long).unwrap(),
        [to_backend("keep_alive", &request)]
    );
    // Negative VarInts are IDs too.
    assert_eq!(
        session.clientbound("keep_alive", &varint(-5)).unwrap(),
        [to_client("keep_alive", &(-5_i64).to_be_bytes())]
    );
    assert_eq!(
        session
            .serverbound("keep_alive", &(-5_i64).to_be_bytes())
            .unwrap(),
        [to_backend("keep_alive", &varint(-5))]
    );
}

#[test]
fn answers_no_1_12_1_backend_can_have_asked_for_vanish() {
    let mut session = Session::play(NEWER, OLDER);
    for id in [i64::from(i32::MAX) + 1, i64::from(i32::MIN) - 1, i64::MAX] {
        assert_eq!(
            session
                .serverbound("keep_alive", &id.to_be_bytes())
                .unwrap(),
            []
        );
    }
}

#[test]
fn keep_alives_narrow_for_a_1_12_1_client() {
    let mut session = Session::play(OLDER, NEWER);
    // Recorded from the official 1.12.2 server.
    let request = hex("000000000697605b");
    let short = varint(0x0697_605b);
    assert_eq!(
        session.clientbound("keep_alive", &request).unwrap(),
        [to_client("keep_alive", &short)]
    );
    assert_eq!(
        session.serverbound("keep_alive", &short).unwrap(),
        [to_backend("keep_alive", &request)]
    );
}

#[test]
fn ids_a_1_12_1_client_cannot_hold_get_stand_ins() {
    let mut session = Session::play(OLDER, NEWER);
    // After 25 days of host uptime, and IDs other servers might choose.
    let ids = [3_000_000_000_i64, -1, i64::MIN];
    for (index, id) in ids.iter().enumerate() {
        let stand_in = varint(-1 - index as i32);
        assert_eq!(
            session
                .clientbound("keep_alive", &id.to_be_bytes())
                .unwrap(),
            [to_client("keep_alive", &stand_in)]
        );
    }
    // Answered out of order, each exactly once.
    for (index, id) in ids.iter().enumerate().rev() {
        let stand_in = varint(-1 - index as i32);
        assert_eq!(
            session.serverbound("keep_alive", &stand_in).unwrap(),
            [to_backend("keep_alive", &id.to_be_bytes())]
        );
        assert_eq!(session.serverbound("keep_alive", &stand_in).unwrap(), []);
    }
    // A stand-in that was never handed out would get the player kicked.
    assert_eq!(session.serverbound("keep_alive", &varint(-9)).unwrap(), []);
}

#[test]
fn only_recent_stand_ins_are_remembered() {
    let mut session = Session::play(OLDER, NEWER);
    for id in 0..20_i64 {
        session
            .clientbound("keep_alive", &(i64::MAX - id).to_be_bytes())
            .unwrap();
    }
    // The first four of twenty were forgotten; the rest still resolve.
    for stand_in in [-1, -4] {
        assert_eq!(
            session
                .serverbound("keep_alive", &varint(stand_in))
                .unwrap(),
            []
        );
    }
    assert_eq!(
        session.serverbound("keep_alive", &varint(-5)).unwrap(),
        [to_backend("keep_alive", &(i64::MAX - 4).to_be_bytes())]
    );
    assert_eq!(
        session.serverbound("keep_alive", &varint(-20)).unwrap(),
        [to_backend("keep_alive", &(i64::MAX - 19).to_be_bytes())]
    );
}

#[test]
fn malformed_keep_alives_fail() {
    for (client, backend) in [(NEWER, OLDER), (OLDER, NEWER)] {
        let mut session = Session::play(client, backend);
        let (long, short) = (&[0u8; 8][..], &[0x05u8][..]);
        let (request, answer) = if client == NEWER {
            (short, long)
        } else {
            (long, short)
        };
        for body in [
            &[][..],
            &request[..request.len() - 1],
            &[request, &[0]].concat(),
        ] {
            if !body.is_empty() || client == OLDER {
                assert!(session.clientbound("keep_alive", body).is_err());
            }
        }
        for body in [&answer[..answer.len() - 1], &[answer, &[0]].concat()] {
            assert!(session.serverbound("keep_alive", body).is_err());
        }
        assert!(session.serverbound("keep_alive", &[0xff; 6]).is_err());
    }
}

/// Every keep-alive the official servers sent, answered by a client of the
/// other version through the pair, reaches the server as it was sent.
#[test]
fn recorded_keep_alives_round_trip() {
    for (client, backend) in [(NEWER, OLDER), (OLDER, NEWER), (335, NEWER), (NEWER, 335)] {
        let Some(requests) = recorded(backend, Direction::Clientbound, "keep_alive") else {
            return;
        };
        assert!(!requests.is_empty());
        let mut session = Session::play(client, backend);
        for request in requests {
            let out = session.clientbound("keep_alive", &request).unwrap();
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].name, "keep_alive");
            let answer = session.serverbound("keep_alive", &out[0].data).unwrap();
            assert_eq!(answer, [to_backend("keep_alive", &request)]);
        }
    }
}
