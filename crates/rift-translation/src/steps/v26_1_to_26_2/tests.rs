//! Conversions of the 26.1 <-> 26.2 steps, on packets the official servers
//! sent.
use super::{DOWN, NEWER, OLDER, UP};
use crate::{
    Direction::{Clientbound, Serverbound},
    State,
    steps::v1_21_11_to_26_1::testing::{Harness, hex},
};

/// `login_finished` for the same offline player from 26.1 and 26.2 (with the
/// server's session UUID at the end).
const LOGIN_FINISHED_26_1: &str = "888b6130823b387bb012d6dc24e47d5906436f7270757300";
const LOGIN_FINISHED_26_2: &str =
    "888b6130823b387bb012d6dc24e47d5906436f727075730024dc220c129a4309a7a8df6a566a4d06";

/// Join game from both servers; they differ in the seed hash and the
/// online-mode flag (false) 26.2 added before the secure chat flag.
const LOGIN_26_1: &str = concat!(
    "000000010003136d696e6563726166743a6f766572776f726c64116d696e6563726166743a7468",
    "655f656e64146d696e6563726166743a7468655f6e657468657214020200010000136d696e6563",
    "726166743a6f766572776f726c64a02ab52894d257a901ff00010000c1ffffff0f00"
);
const LOGIN_26_2: &str = concat!(
    "000000010003136d696e6563726166743a6f766572776f726c64116d696e6563726166743a7468",
    "655f656e64146d696e6563726166743a7468655f6e657468657214020200010000136d696e6563",
    "726166743a6f766572776f726c644ae77737d6c2002f01ff00010000c1ffffff0f0000"
);

#[test]
fn login_finished_gains_and_loses_the_session_uuid() {
    let mut up = Harness::new(UP, NEWER, OLDER);
    let sent = up
        .send(
            Clientbound,
            State::Login,
            "login_finished",
            &hex(LOGIN_FINISHED_26_1),
        )
        .unwrap();
    assert_eq!(sent.len(), 1);
    let data = &sent[0].data;
    assert_eq!(data.len(), hex(LOGIN_FINISHED_26_2).len());
    assert_eq!(data[..24], hex(LOGIN_FINISHED_26_1)[..]);
    // The same connection keeps its session.
    let again = up
        .send(
            Clientbound,
            State::Login,
            "login_finished",
            &hex(LOGIN_FINISHED_26_1),
        )
        .unwrap();
    assert_eq!(again[0].data, *data);
    let mut down = Harness::new(DOWN, OLDER, NEWER);
    let sent = down
        .send(
            Clientbound,
            State::Login,
            "login_finished",
            &hex(LOGIN_FINISHED_26_2),
        )
        .unwrap();
    assert_eq!(sent[0].data, hex(LOGIN_FINISHED_26_1));
    // A signed property survives; a missing session UUID does not pass.
    let mut w = crate::wire::Writer::new();
    w.uuid(7).string("Steve").varint(1);
    w.string("textures")
        .string("e30=")
        .bool(true)
        .string("c2ln");
    let mut with_session = w.buf.clone();
    with_session.extend_from_slice(&[0x11; 16]);
    let sent = down
        .send(Clientbound, State::Login, "login_finished", &with_session)
        .unwrap();
    assert_eq!(sent[0].data, w.buf);
    assert!(
        down.send(Clientbound, State::Login, "login_finished", &w.buf)
            .is_err()
    );
}

#[test]
fn join_game_gains_and_loses_the_online_mode_flag() {
    let mut up = Harness::new(UP, NEWER, OLDER);
    let (_, data) = up.one(Clientbound, "login", &hex(LOGIN_26_1));
    let mut expected = hex(LOGIN_26_1);
    expected.insert(expected.len() - 1, 1);
    assert_eq!(data, expected);
    let mut down = Harness::new(DOWN, OLDER, NEWER);
    let (_, data) = down.one(Clientbound, "login", &hex(LOGIN_26_2));
    let mut expected = hex(LOGIN_26_2);
    expected.remove(expected.len() - 2);
    assert_eq!(data, expected);
    assert!(down.clientbound("login", &hex(LOGIN_26_1)).is_err());
}

#[test]
fn spectator_clicks_keep_their_entity() {
    let mut up = Harness::new(UP, NEWER, OLDER);
    assert_eq!(
        up.one(Serverbound, "spectator_action", &hex("2b")).1,
        hex("2a")
    );
    assert!(
        up.serverbound("spectator_action", &hex("00"))
            .unwrap()
            .is_empty()
    );
    let mut down = Harness::new(DOWN, OLDER, NEWER);
    assert_eq!(
        down.one(Serverbound, "spectator_action", &hex("2a")).1,
        hex("2b")
    );
    assert!(down.serverbound("spectator_action", &hex("2a00")).is_err());
}

mod beds {
    use super::*;
    use crate::{
        codec::chunk::{
            container::{Container, Format, Kind},
            light::Light,
            model::{Biomes, Chunk, Section, World},
        },
        data,
        wire::{Position, Reader, Writer},
    };

    fn state(protocol: i32, description: &str) -> i32 {
        data::version(protocol)
            .block_states
            .parse(description)
            .unwrap()
    }

    const BED: &str = "red_bed[facing=south,occupied=false,part=head]";

    /// An overworld column of air with one bed at (17, -60, 34).
    fn column(protocol: i32) -> Chunk {
        let mut scratch = Vec::new();
        let sections = (-4..20)
            .map(|index| {
                let blocks = if index == -4 {
                    let mut values = vec![0; 4096];
                    values[(4 * 16 + 2) * 16 + 1] = state(protocol, BED);
                    Container::from_values(&values, &Format::blocks(protocol), &mut scratch)
                } else {
                    Container::single(Kind::Blocks, 0)
                };
                Some(Section {
                    block_count: i16::from(index == -4),
                    fluid_count: 0,
                    blocks,
                    biomes: Some(Container::single(Kind::Biomes, 0)),
                })
            })
            .collect();
        Chunk {
            x: 1,
            z: 2,
            full: true,
            ignore_old_data: false,
            min_section: -4,
            sections,
            sky_light: true,
            biomes: Biomes::Sections,
            heightmaps: Vec::new(),
            block_entities: Vec::new(),
            light: Some(Light::default()),
        }
    }

    #[test]
    fn beds_get_their_block_entity_back_in_chunks() {
        let mut down = Harness::new(DOWN, OLDER, NEWER);
        let sent = down
            .send(
                Clientbound,
                State::Play,
                "level_chunk_with_light",
                &column(NEWER).write(NEWER).unwrap(),
            )
            .unwrap();
        assert_eq!(sent.len(), 1);
        let chunk = Chunk::read(&sent[0].data, OLDER, &World::default()).unwrap();
        let bed = data::version(OLDER)
            .expect_registry("block_entity_type")
            .id("bed")
            .unwrap();
        let entities: Vec<_> = chunk
            .block_entities
            .iter()
            .map(|entity| {
                (
                    entity.x,
                    entity.y,
                    entity.z,
                    entity.kind,
                    entity.nbt.clone(),
                )
            })
            .collect();
        assert_eq!(entities, [(17, -60, 34, bed, None)]);
        assert_eq!(chunk.sections, column(OLDER).sections);
        // Going up, the 26.1 server's bed block entity has no type in 26.2.
        let mut up = Harness::new(UP, NEWER, OLDER);
        let sent = up
            .send(
                Clientbound,
                State::Play,
                "level_chunk_with_light",
                &chunk.write(OLDER).unwrap(),
            )
            .unwrap();
        let chunk = Chunk::read(&sent[0].data, NEWER, &World::default()).unwrap();
        assert!(chunk.block_entities.is_empty());
    }

    #[test]
    fn placing_a_bed_sends_its_block_entity() {
        let mut down = Harness::new(DOWN, OLDER, NEWER);
        let position = Position {
            x: 13,
            y: -60,
            z: 4,
        };
        let mut w = Writer::new();
        w.position(position).varint(state(NEWER, BED));
        let sent = down
            .send(Clientbound, State::Play, "block_update", &w.buf)
            .unwrap();
        let names: Vec<&str> = sent.iter().map(|sent| sent.name.as_str()).collect();
        assert_eq!(names, ["block_update", "block_entity_data"]);
        let mut r = Reader::new(&sent[1].data);
        assert_eq!(r.position().unwrap(), position);
        let bed = data::version(OLDER)
            .expect_registry("block_entity_type")
            .id("bed")
            .unwrap();
        assert_eq!(r.varint().unwrap(), bed);
        assert_eq!(r.rest(), [0x0a, 0x00]);
        // Other blocks send nothing more.
        let mut w = Writer::new();
        w.position(position).varint(state(NEWER, "stone"));
        let sent = down
            .send(Clientbound, State::Play, "block_update", &w.buf)
            .unwrap();
        assert_eq!(sent.len(), 1);
        // A section update with a bed among other blocks.
        // Section (0, -4, 0): x and z are zero.
        let section = -4i64 & 0xfffff;
        let mut w = Writer::new();
        w.i64(section).varint(2);
        w.varlong((i64::from(state(NEWER, "stone")) << 12) | 0x123);
        w.varlong((i64::from(state(NEWER, BED)) << 12) | 0x456);
        let sent = down
            .send(Clientbound, State::Play, "section_blocks_update", &w.buf)
            .unwrap();
        let names: Vec<&str> = sent.iter().map(|sent| sent.name.as_str()).collect();
        assert_eq!(names, ["section_blocks_update", "block_entity_data"]);
        let mut r = Reader::new(&sent[1].data);
        assert_eq!(r.position().unwrap(), Position { x: 4, y: -58, z: 5 });
    }
}
