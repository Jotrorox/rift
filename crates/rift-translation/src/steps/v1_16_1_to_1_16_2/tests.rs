use super::{NEWER, OLDER, down, up};
use crate::{
    Direction,
    codec::registries::{defaults, login},
    data,
    nbt::{Compound, Nbt},
    steps::v1_14_4_to_1_15::support::harness::{Harness, body, hex, replay, truncations_fail},
    wire::Writer,
};

fn upgrading() -> Harness {
    Harness::new((up::STEP.unwrap())(), NEWER, OLDER)
}

fn downgrading() -> Harness {
    Harness::new((down::STEP.unwrap())(), OLDER, NEWER)
}

const OVERWORLD: &str = "minecraft:overworld";
const WORLDS: &str = "03 136d696e6563726166743a6f766572776f726c64 146d696e6563726166743a7468655f6e6574686572 116d696e6563726166743a7468655f656e64";

/// What the official servers send as their codec, as named root compounds.
fn vanilla_codec_bytes(protocol: i32) -> Vec<u8> {
    login::default_codec(protocol).unwrap().to_bytes(true)
}

fn vanilla_codec() -> Compound {
    match login::default_codec(NEWER).unwrap() {
        Nbt::Compound(codec) => codec,
        _ => unreachable!(),
    }
}

/// A section position packed as 1.16.2 does: x and z 22 bits, y 20 bits.
fn pack_section(x: i32, y: i32, z: i32) -> i64 {
    (i64::from(x) & 0x3f_ffff) << 42 | (i64::from(z) & 0x3f_ffff) << 20 | (i64::from(y) & 0xf_ffff)
}

/// The vanilla 1.16.2 dimension type `name`.
fn element(name: &str) -> Compound {
    defaults::element(NEWER, "dimension_type", name)
        .unwrap()
        .clone()
}

/// A 1.16.1 `login` as the official server sent it, with the vanilla codec.
fn login_1_16_1(game_mode: u8, dimension: &str) -> Vec<u8> {
    let mut packet = body(|w| {
        w.i32(1).u8(game_mode).u8(0xff);
    });
    packet.extend(hex(WORLDS));
    packet.extend(vanilla_codec_bytes(OLDER));
    packet.extend(body(|w| {
        w.string(dimension)
            .string(OVERWORLD)
            .i64(5)
            .u8(20)
            .varint(2)
            .bool(false)
            .bool(true)
            .bool(false)
            .bool(true);
    }));
    packet
}

/// The same `login` as the official 1.16.2 server sends it.
fn login_1_16_2(hardcore: bool, game_mode: u8, dimension: &str, codec: &Compound) -> Vec<u8> {
    let mut packet = body(|w| {
        w.i32(1).bool(hardcore).u8(game_mode).u8(0xff);
    });
    packet.extend(hex(WORLDS));
    packet.extend(Nbt::Compound(codec.clone()).to_bytes(true));
    packet.extend(Nbt::Compound(element(dimension)).to_bytes(true));
    packet.extend(body(|w| {
        w.string(OVERWORLD)
            .i64(5)
            .varint(20)
            .varint(2)
            .bool(false)
            .bool(true)
            .bool(false)
            .bool(true);
    }));
    packet
}

fn respawn_tail(w: &mut Writer) {
    w.string("minecraft:the_nether")
        .i64(5)
        .u8(0)
        .u8(1)
        .bool(false)
        .bool(false)
        .bool(true);
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(up::Up::routes().problems(), &[] as &[String]);
    assert_eq!(down::Down::routes().problems(), &[] as &[String]);
}

#[test]
fn login_converts_like_the_servers_send_it() {
    let sent = upgrading().one(
        Direction::Clientbound,
        "login",
        &login_1_16_1(0x09, OVERWORLD),
        "login",
    );
    assert_eq!(sent, login_1_16_2(true, 1, OVERWORLD, &vanilla_codec()));
    let sent = downgrading().one(
        Direction::Clientbound,
        "login",
        &login_1_16_2(true, 1, OVERWORLD, &vanilla_codec()),
        "login",
    );
    assert_eq!(sent, login_1_16_1(0x09, OVERWORLD));
    let sent = downgrading().one(
        Direction::Clientbound,
        "login",
        &login_1_16_2(false, 2, "minecraft:the_end", &vanilla_codec()),
        "login",
    );
    assert_eq!(sent, login_1_16_1(2, "minecraft:the_end"));
}

#[test]
fn recorded_login_converts_to_what_the_other_server_sent() {
    use crate::codec::testing::corpus;
    let find = |protocol: i32| {
        corpus(protocol)?
            .into_iter()
            .find(|packet| {
                packet.direction == Direction::Clientbound && packet.name(protocol) == "login"
            })
            .map(|packet| packet.data)
    };
    let (Some(old), Some(new)) = (find(OLDER), find(NEWER)) else {
        eprintln!("no corpus for {OLDER} and {NEWER}; skipped");
        return;
    };
    // The two sessions differ only in their seed (8 bytes before the tail).
    let seedless = |packet: &[u8]| {
        let mut packet = packet.to_vec();
        let at = packet.len() - 14;
        packet[at..at + 8].fill(0);
        packet
    };
    let sent = upgrading().one(Direction::Clientbound, "login", &old, "login");
    assert_eq!(seedless(&sent), seedless(&new));
    let sent = downgrading().one(Direction::Clientbound, "login", &new, "login");
    assert_eq!(seedless(&sent), seedless(&old));
}

#[test]
fn respawn_names_and_describes_dimension_types() {
    let mut step = upgrading();
    step.clientbound("login", &login_1_16_1(0, OVERWORLD))
        .unwrap();
    let old = body(|w| {
        w.string("minecraft:the_nether")
            .string("minecraft:the_nether");
        w.i64(5).u8(0).u8(1).bool(false).bool(false).bool(true);
    });
    let mut new = Nbt::Compound(element("minecraft:the_nether")).to_bytes(true);
    new.extend(body(respawn_tail));
    assert_eq!(
        step.one(Direction::Clientbound, "respawn", &old, "respawn"),
        new
    );

    let mut step = downgrading();
    step.clientbound(
        "login",
        &login_1_16_2(false, 0, OVERWORLD, &vanilla_codec()),
    )
    .unwrap();
    assert_eq!(
        step.one(Direction::Clientbound, "respawn", &new, "respawn"),
        old
    );
    // An element no type has: the type with its effects.
    let mut custom = element("minecraft:the_nether");
    custom.insert("logical_height", 64);
    let mut packet = Nbt::Compound(custom).to_bytes(true);
    packet.extend(body(respawn_tail));
    assert_eq!(
        step.one(Direction::Clientbound, "respawn", &packet, "respawn"),
        old
    );
}

#[test]
fn a_login_dimension_without_a_type_gets_one() {
    let mut custom = element(OVERWORLD);
    custom.insert("logical_height", 128);
    let mut packet = body(|w| {
        w.i32(1).bool(false).u8(0).u8(0xff).varint(0);
    });
    packet.extend(vanilla_codec_bytes(NEWER));
    packet.extend(Nbt::Compound(custom.clone()).to_bytes(true));
    packet.extend(body(|w| {
        w.string("rift:world")
            .i64(0)
            .varint(500)
            .varint(8)
            .bool(false)
            .bool(true)
            .bool(false)
            .bool(false);
    }));
    let sent = downgrading().one(Direction::Clientbound, "login", &packet, "login");
    let mut reader = crate::wire::Reader::new(&sent[7..]);
    let codec = Nbt::read(&mut reader, true).unwrap().unwrap();
    let list = codec.as_compound().unwrap().list("dimension").unwrap();
    assert_eq!(list.len(), 5);
    let last = list[4].as_compound().unwrap();
    assert_eq!(last.str("name"), Some("rift:dimension_4"));
    assert_eq!(last.int("logical_height"), Some(128));
    assert_eq!(reader.str().unwrap(), "rift:dimension_4");
    reader.str().unwrap();
    reader.i64().unwrap();
    assert_eq!(reader.u8().unwrap(), 255); // Max players, clamped.
}

#[test]
fn block_updates_split_into_sections_and_join_again() {
    let (old, new) = (data::version(OLDER), data::version(NEWER));
    let state = |version: &data::VersionData, name: &str| version.block_states.parse(name).unwrap();
    // Chains gained an axis in 1.16.2, which moved every later state.
    let lantern = (
        state(old, "lantern[hanging=true]"),
        state(new, "lantern[hanging=true,waterlogged=false]"),
    );
    let stone = (state(old, "stone"), state(new, "stone"));
    assert_ne!(lantern.0, lantern.1);
    let column = body(|w| {
        w.i32(-2).i32(-11).varint(3);
        w.u8(0x0a).u8(4).varint(lantern.0);
        w.u8(0xf1).u8(70).varint(stone.0);
        w.u8(0x1a).u8(5).varint(lantern.0);
    });
    let sent = upgrading()
        .clientbound("chunk_blocks_update", &column)
        .unwrap();
    assert_eq!(sent.len(), 2);
    let section = |y: i32, records: &[i64]| {
        body(|w| {
            w.i64(pack_section(-2, y, -11))
                .bool(false)
                .varint(records.len() as i32);
            for record in records {
                w.varlong(*record);
            }
        })
    };
    let lantern_record = |x: i64, y: i64, z: i64| i64::from(lantern.1) << 12 | x << 8 | z << 4 | y;
    assert_eq!(sent[0].name, "section_blocks_update");
    assert_eq!(
        sent[0].data,
        section(0, &[lantern_record(0, 4, 10), lantern_record(1, 5, 10)])
    );
    assert_eq!(
        sent[1].data,
        section(4, &[i64::from(stone.1) << 12 | 15 << 8 | 1 << 4 | 6])
    );

    let sent = downgrading().one(
        Direction::Clientbound,
        "section_blocks_update",
        &section(0, &[lantern_record(0, 4, 10), lantern_record(1, 5, 10)]),
        "chunk_blocks_update",
    );
    assert_eq!(
        sent,
        body(|w| {
            w.i32(-2).i32(-11).varint(2);
            w.u8(0x0a).u8(4).varint(lantern.0);
            w.u8(0x1a).u8(5).varint(lantern.0);
        })
    );
    // A section above what 1.16.1 can address has nothing to send.
    let high = section(16, &[lantern_record(0, 0, 0)]);
    assert!(
        downgrading()
            .clientbound("section_blocks_update", &high)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn recipe_book_settings_split_and_join() {
    let mut step = upgrading();
    // The backend's own crafting and furnace settings.
    let recipe = body(|w| {
        w.varint(0)
            .bool(true)
            .bool(false)
            .bool(false)
            .bool(true)
            .varint(0)
            .varint(0);
    });
    step.clientbound("recipe", &recipe).unwrap();
    let change = body(|w| {
        w.varint(2).bool(true).bool(true);
    });
    let sent = step.one(
        Direction::Serverbound,
        "recipe_book_change_settings",
        &change,
        "recipe_book_update",
    );
    assert_eq!(sent, hex("01 01 00 00 01 01 01 00 00"));
    let seen = body(|w| {
        w.string("minecraft:stick");
    });
    let sent = step.one(
        Direction::Serverbound,
        "recipe_book_seen_recipe",
        &seen,
        "recipe_book_update",
    );
    assert_eq!(sent[0], 0);
    assert_eq!(&sent[1..], seen);
    assert!(
        step.serverbound("recipe_book_change_settings", &hex("04 00 00"))
            .is_err()
    );

    let mut step = downgrading();
    let mut shown = vec![0];
    shown.extend(&seen);
    assert_eq!(
        step.one(
            Direction::Serverbound,
            "recipe_book_update",
            &shown,
            "recipe_book_seen_recipe"
        ),
        seen
    );
    let sent = step
        .serverbound("recipe_book_update", &hex("01 01 00 00 01 01 01 00 00"))
        .unwrap();
    let bodies: Vec<Vec<u8>> = sent.iter().map(|out| out.data.clone()).collect();
    assert!(
        sent.iter()
            .all(|out| out.name == "recipe_book_change_settings")
    );
    assert_eq!(
        bodies,
        [
            hex("00 01 00"),
            hex("01 00 01"),
            hex("02 01 01"),
            hex("03 00 00")
        ]
    );
    assert!(step.serverbound("recipe_book_update", &hex("02")).is_err());
}

#[test]
fn malformed_packets_fail() {
    truncations_fail(
        &mut upgrading(),
        Direction::Clientbound,
        "login",
        &login_1_16_1(0, OVERWORLD),
    );
    truncations_fail(
        &mut downgrading(),
        Direction::Clientbound,
        "login",
        &login_1_16_2(false, 0, OVERWORLD, &vanilla_codec()),
    );
    truncations_fail(
        &mut upgrading(),
        Direction::Clientbound,
        "chunk_blocks_update",
        &hex("00000000 00000000 01 00 00 01"),
    );
    truncations_fail(
        &mut downgrading(),
        Direction::Clientbound,
        "section_blocks_update",
        &body(|w| {
            w.i64(0).bool(false).varint(1).varlong(1 << 12);
        }),
    );
    truncations_fail(
        &mut upgrading(),
        Direction::Serverbound,
        "recipe_book_change_settings",
        &hex("00 01 00"),
    );
    truncations_fail(
        &mut downgrading(),
        Direction::Serverbound,
        "recipe_book_update",
        &hex("01 01 00 00 01 01 01 00 00"),
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

/// A full 1.16.2 column without sections whose 1024 biome cells are
/// `biomes`.
fn column_1_16_2(biomes: &[i32]) -> Vec<u8> {
    let mut packet = body(|w| {
        w.i32(3).i32(-4).bool(true).varint(0);
    });
    packet.extend(Nbt::Compound(Compound::new()).to_bytes(true));
    packet.extend(body(|w| {
        w.varint(biomes.len() as i32);
        for biome in biomes {
            w.varint(*biome);
        }
        w.varint(0).varint(0);
    }));
    packet
}

/// The biome cells of a full 1.16.1 column without sections.
fn biomes_1_16_1(packet: &[u8]) -> Vec<i32> {
    let mut reader = crate::wire::Reader::new(packet);
    reader.i32().unwrap();
    reader.i32().unwrap();
    assert!(reader.bool().unwrap());
    reader.bool().unwrap(); // Ignore old light.
    assert_eq!(reader.varint().unwrap(), 0);
    Nbt::read(&mut reader, true).unwrap();
    let biomes = (0..1024).map(|_| reader.i32().unwrap()).collect();
    assert_eq!(reader.varint().unwrap(), 0);
    assert_eq!(reader.varint().unwrap(), 0);
    reader.finish().unwrap();
    biomes
}

#[test]
fn chunk_biomes_follow_the_registry_the_backend_sent() {
    use crate::codec::registries::form::{self, Entry};
    // A 1.16.2 backend whose datapack adds a biome before vanilla's and
    // renumbers them: chunks name biomes by the IDs of its own registry.
    let mut registries = defaults::registries(NEWER);
    let biomes = registries
        .iter_mut()
        .find(|registry| registry.name == "worldgen/biome")
        .unwrap();
    let plains = biomes.entry("plains").unwrap().element.clone();
    for entry in &mut biomes.entries {
        entry.id += 1;
    }
    biomes.entries.insert(
        0,
        Entry {
            name: "rift:glade".into(),
            id: 0,
            element: plains,
        },
    );
    let mut login = body(|w| {
        w.i32(1).bool(false).u8(1).u8(0xff);
    });
    login.extend(hex(WORLDS));
    login.extend(
        form::write_codec(NEWER, &registries)
            .unwrap()
            .to_bytes(true),
    );
    login.extend(Nbt::Compound(element(OVERWORLD)).to_bytes(true));
    login.extend(body(|w| {
        w.string(OVERWORLD)
            .i64(5)
            .varint(20)
            .varint(2)
            .bool(false)
            .bool(true)
            .bool(false)
            .bool(true);
    }));
    let mut step = downgrading();
    step.one(Direction::Clientbound, "login", &login, "login");
    let builtin = data::version(OLDER).expect_registry("biome");
    let id = |name: &str| builtin.id(name).unwrap();
    // The glade, the desert and the badlands as the backend numbers them.
    let cells: Vec<i32> = (0..1024)
        .map(|cell| [0, id("desert") + 1, id("badlands") + 1][cell % 3])
        .collect();
    let sent = step.one(
        Direction::Clientbound,
        "level_chunk",
        &column_1_16_2(&cells),
        "level_chunk",
    );
    let expected: Vec<i32> = (0..1024)
        .map(|cell| [id("plains"), id("desert"), id("badlands")][cell % 3])
        .collect();
    assert_eq!(biomes_1_16_1(&sent), expected);

    // Going up, the 1.16.2 client is sent vanilla's registry, which numbers
    // biomes as 1.16.1 does.
    let mut step = upgrading();
    step.one(
        Direction::Clientbound,
        "login",
        &login_1_16_1(0, OVERWORLD),
        "login",
    );
    let mut old = body(|w| {
        w.i32(3).i32(-4).bool(true).bool(true).varint(0);
    });
    old.extend(Nbt::Compound(Compound::new()).to_bytes(true));
    let cells: Vec<i32> = (0..1024)
        .map(|cell| [id("desert"), id("jungle")][cell % 2])
        .collect();
    old.extend(body(|w| {
        for cell in &cells {
            w.i32(*cell);
        }
        w.varint(0).varint(0);
    }));
    let sent = step.one(Direction::Clientbound, "level_chunk", &old, "level_chunk");
    assert_eq!(sent, column_1_16_2(&cells));
}

#[test]
fn the_registries_codec_has_the_recorded_vanilla_codec() {
    use std::io::Read;
    // Recorded from the official 1.16.2 server (gzip of a named root).
    const RECORDED: &[u8] = include_bytes!("recorded_codec.nbt.gz");
    let mut recorded = Vec::new();
    flate2::read::GzDecoder::new(RECORDED)
        .read_to_end(&mut recorded)
        .unwrap();
    assert_eq!(vanilla_codec_bytes(NEWER), recorded);
}

#[test]
fn a_recorded_section_position_unpacks() {
    // Recorded from the 1.16.2 server: the section at chunk 1, 14, height 0.
    let stone = (
        data::version(NEWER).block_states.parse("stone").unwrap(),
        data::version(OLDER).block_states.parse("stone").unwrap(),
    );
    let section = body(|w| {
        w.i64(0x0000_0400_00e0_0000)
            .bool(false)
            .varint(1)
            .varlong(i64::from(stone.0) << 12 | 0x3_2_5);
    });
    let sent = downgrading().one(
        Direction::Clientbound,
        "section_blocks_update",
        &section,
        "chunk_blocks_update",
    );
    let column = body(|w| {
        w.i32(1).i32(14).varint(1).u8(0x32).u8(5).varint(stone.1);
    });
    assert_eq!(sent, column);
}
