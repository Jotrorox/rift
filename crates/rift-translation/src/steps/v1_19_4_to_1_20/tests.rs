use super::{NEWER, OLDER, down::Down, up::Up};
use crate::{
    Direction, Output, Packet, State, Translator,
    codec::testing::{Recorded, corpus},
    data,
    nbt::{Compound, Nbt},
    steps::v1_19_3_to_1_19_4::{
        harness::{canonical, codec_of, entries, hex, mangle, one, recorded_login, send},
        play,
    },
    wire::Writer,
};

const CLIENTBOUND: Direction = Direction::Clientbound;
const SERVERBOUND: Direction = Direction::Serverbound;

/// The recorded features packet of both servers: just `minecraft:vanilla`.
const VANILLA: &str = "01 11 6d696e6563726166743a76616e696c6c61";
const SIGN: &str = "0000000c3ffff045";

fn respawn(death: bool) -> Vec<u8> {
    let mut writer = Writer::new();
    writer
        .string("minecraft:the_nether")
        .string("minecraft:the_nether")
        .i64(-4_172_144_997_902_289_642)
        .u8(1)
        .i8(-1)
        .bool(false)
        .bool(false)
        .i8(3);
    writer.bool(death);
    if death {
        writer
            .string("minecraft:overworld")
            .i64(0x0000_000c_3fff_f045);
    }
    writer.into_inner()
}

fn sign_update(front: Option<bool>) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.bytes(&hex(SIGN));
    if let Some(front) = front {
        writer.bool(front);
    }
    for line in ["first", "", "{\"text\":\"x\"}", "last"] {
        writer.string(line);
    }
    writer.into_inner()
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(Up::routes().problems(), &[] as &[String]);
    assert_eq!(Down::routes().problems(), &[] as &[String]);
}

fn names(codec: &Compound, registry: &str) -> Vec<String> {
    let mut names: Vec<String> = entries(codec, registry)
        .into_iter()
        .map(|(name, _)| name.to_owned())
        .collect();
    names.sort();
    names
}

fn ids(codec: &Compound, registry: &str) -> Vec<(String, i32)> {
    entries(codec, registry)
        .into_iter()
        .map(|(name, entry)| (name.to_owned(), entry.int("id").unwrap()))
        .collect()
}

/// A 1.20 client gets a portal cooldown and the two damage types 1.20
/// added, after the backend's own so its IDs keep their meaning.
#[test]
fn upgraded_login_has_a_cooldown_and_every_1_20_damage_type() {
    let (Some(old), Some(new)) = (recorded_login(OLDER), recorded_login(NEWER)) else {
        return;
    };
    let (name, body) = one(Up::routes(), &mut Up::new(), CLIENTBOUND, "login", &old);
    assert_eq!(name, "login");
    assert_eq!(body.last(), Some(&0));
    let (before, after, official) = (
        codec_of(OLDER, &old),
        codec_of(NEWER, &body),
        codec_of(NEWER, &new),
    );
    assert_eq!(
        names(&after, "damage_type"),
        names(&official, "damage_type")
    );
    let (before_ids, after_ids) = (ids(&before, "damage_type"), ids(&after, "damage_type"));
    assert_eq!(after_ids[..before_ids.len()], before_ids[..]);
    assert_eq!(
        after_ids[before_ids.len()..],
        [
            ("minecraft:generic_kill".to_owned(), 42),
            ("minecraft:outside_border".to_owned(), 43)
        ]
    );
    // The new entries are the official ones.
    for added in ["minecraft:generic_kill", "minecraft:outside_border"] {
        let element = |codec: &Compound| {
            entries(codec, "damage_type")
                .into_iter()
                .find(|(name, _)| *name == added)
                .map(|(_, entry)| canonical(entry.get("element").unwrap()))
        };
        assert_eq!(element(&after), element(&official));
    }
    for registry in ["chat_type", "dimension_type", "trim_pattern"] {
        let key = format!("minecraft:{registry}");
        assert_eq!(after.get(&key), before.get(&key), "{registry}");
    }
    // Biomes keep everything but music 1.20 renamed.
    let music = |codec: &Compound| -> Vec<(String, String)> {
        entries(codec, "worldgen/biome")
            .into_iter()
            .filter_map(|(name, biome)| {
                let element = biome.compound("element")?;
                let sound = element
                    .compound("effects")?
                    .compound("music")?
                    .str("sound")?;
                Some((name.to_owned(), sound.to_owned()))
            })
            .collect()
    };
    let renamed: Vec<_> = music(&before)
        .into_iter()
        .zip(music(&after))
        .filter(|(before, after)| before != after)
        .map(|(before, after)| (before.1, after.1))
        .collect();
    assert!(!renamed.is_empty());
    for (before, after) in renamed {
        assert_eq!(
            (before.as_str(), after.as_str()),
            (
                "minecraft:music.overworld.jungle_and_forest",
                "minecraft:music.overworld.jungle"
            )
        );
    }
    assert_eq!(
        names(&after, "worldgen/biome"),
        names(&before, "worldgen/biome")
    );
    // The rest round-trips through the 1.20 layout.
    let again = play::rewrite_login(&body, NEWER, NEWER, |_| Ok(()));
    assert_eq!(again.unwrap().body, body);
}

/// A 1.19.4 client loses the cooldown and gets only trim templates and
/// biome music it knows.
#[test]
fn downgraded_login_names_only_what_1_19_4_knows() {
    let (Some(old), Some(new)) = (recorded_login(OLDER), recorded_login(NEWER)) else {
        return;
    };
    let (_, body) = one(Down::routes(), &mut Down::new(), CLIENTBOUND, "login", &new);
    let (official, after) = (codec_of(OLDER, &old), codec_of(OLDER, &body));
    // Exactly the 1.19.4 layout: nothing after the death location.
    let again = play::rewrite_login(&body, OLDER, OLDER, |_| Ok(()));
    assert_eq!(again.unwrap().body, body);
    let items = data::version(OLDER).expect_registry("item");
    let sounds = data::version(OLDER).expect_registry("sound_event");
    let patterns = entries(&after, "trim_pattern");
    assert_eq!(patterns.len(), 16);
    for (name, entry) in &patterns {
        let template = entry
            .compound("element")
            .unwrap()
            .str("template_item")
            .unwrap();
        assert!(items.id(template).is_some(), "{name}: {template}");
        if *name == "minecraft:wayfinder" {
            assert_eq!(template, "minecraft:wild_armor_trim_smithing_template");
        }
    }
    for (name, entry) in entries(&after, "trim_material") {
        let ingredient = entry
            .compound("element")
            .unwrap()
            .str("ingredient")
            .unwrap();
        assert!(items.id(ingredient).is_some(), "{name}: {ingredient}");
    }
    let mut music = Vec::new();
    for (name, biome) in entries(&after, "worldgen/biome") {
        let effects = biome
            .compound("element")
            .unwrap()
            .compound("effects")
            .unwrap();
        let mut sounds_of = vec![effects.get("ambient_sound")];
        for key in ["mood_sound", "additions_sound", "music"] {
            sounds_of.push(effects.compound(key).and_then(|holder| holder.get("sound")));
        }
        for sound in sounds_of.into_iter().flatten() {
            let Nbt::String(sound) = sound else {
                panic!("{name}: an inline sound where a known one would do");
            };
            assert!(sounds.id(sound).is_some(), "{name}: {sound}");
        }
        if name == "minecraft:forest" {
            music.push(
                effects
                    .compound("music")
                    .unwrap()
                    .str("sound")
                    .unwrap()
                    .to_owned(),
            );
        }
    }
    assert_eq!(music, ["minecraft:music.overworld.jungle_and_forest"]);
    for registry in ["chat_type", "dimension_type"] {
        let key = format!("minecraft:{registry}");
        assert_eq!(
            after.get(&key).map(canonical),
            official.get(&key).map(canonical),
            "{registry}"
        );
    }
    let wanted: Vec<String> = names(&official, "damage_type");
    let have = names(&after, "damage_type");
    assert!(wanted.iter().all(|name| have.contains(name)));
}

#[test]
fn respawns_gain_and_lose_the_portal_cooldown() {
    for death in [false, true] {
        let old = respawn(death);
        let mut new = old.clone();
        new.push(0);
        assert_eq!(
            one(Up::routes(), &mut Up::new(), CLIENTBOUND, "respawn", &old),
            ("respawn", new.clone())
        );
        let mut cooling = old.clone();
        cooling.extend_from_slice(&[0xac, 0x02]);
        assert_eq!(
            one(
                Down::routes(),
                &mut Down::new(),
                CLIENTBOUND,
                "respawn",
                &cooling
            ),
            ("respawn", old.clone())
        );
        mangle(Up::routes(), Up::new, CLIENTBOUND, "respawn", &old);
        mangle(Down::routes(), Down::new, CLIENTBOUND, "respawn", &new);
    }
}

#[test]
fn signs_edit_the_side_the_server_asked_for() {
    let position = hex(SIGN);
    let with = |front: u8| [position.as_slice(), &[front]].concat();
    assert_eq!(
        one(
            Up::routes(),
            &mut Up::new(),
            CLIENTBOUND,
            "open_sign_editor",
            &position
        ),
        ("open_sign_editor", with(1))
    );
    let mut down = Down::new();
    for (front, edited) in [(1, true), (0, false), (1, true)] {
        let opened = one(
            Down::routes(),
            &mut down,
            CLIENTBOUND,
            "open_sign_editor",
            &with(front),
        );
        assert_eq!(opened, ("open_sign_editor", position.clone()));
        let update = one(
            Down::routes(),
            &mut down,
            SERVERBOUND,
            "sign_update",
            &sign_update(None),
        );
        assert_eq!(update, ("sign_update", sign_update(Some(edited))));
    }
    // The back was asked for one sign, the client edits another: its front.
    one(
        Down::routes(),
        &mut down,
        CLIENTBOUND,
        "open_sign_editor",
        &with(0),
    );
    let mut elsewhere = sign_update(None);
    elsewhere[7] ^= 1;
    let (_, update) = one(
        Down::routes(),
        &mut down,
        SERVERBOUND,
        "sign_update",
        &elsewhere,
    );
    assert_eq!(update[8], 1);
    // And that answer used up the request.
    let (_, update) = one(
        Down::routes(),
        &mut down,
        SERVERBOUND,
        "sign_update",
        &sign_update(None),
    );
    assert_eq!(update[8], 1);

    let mut up = Up::new();
    assert_eq!(
        one(
            Up::routes(),
            &mut up,
            SERVERBOUND,
            "sign_update",
            &sign_update(Some(true))
        ),
        ("sign_update", sign_update(None))
    );
    let back = send(
        Up::routes(),
        &mut up,
        SERVERBOUND,
        "sign_update",
        &sign_update(Some(false)),
    );
    assert!(back.unwrap().is_empty());
}

#[test]
fn combat_end_loses_and_regains_the_killer() {
    // 1.19.4: 60 ticks, killer 7. 1.20: 60 ticks.
    assert_eq!(
        one(
            Up::routes(),
            &mut Up::new(),
            CLIENTBOUND,
            "player_combat_end",
            &[60, 0, 0, 0, 7]
        ),
        ("player_combat_end", vec![60])
    );
    assert_eq!(
        one(
            Down::routes(),
            &mut Down::new(),
            CLIENTBOUND,
            "player_combat_end",
            &[60]
        ),
        ("player_combat_end", vec![60, 0xff, 0xff, 0xff, 0xff])
    );
}

#[test]
fn the_1_20_experiment_is_on_for_1_19_4_and_unknown_to_1_20() {
    let vanilla = hex(VANILLA);
    let mut experimental = Writer::new();
    experimental
        .varint(2)
        .string("minecraft:vanilla")
        .string("minecraft:update_1_20");
    let experimental = experimental.into_inner();
    assert_eq!(
        one(
            Down::routes(),
            &mut Down::new(),
            CLIENTBOUND,
            "update_enabled_features",
            &vanilla
        ),
        ("update_enabled_features", experimental.clone())
    );
    assert_eq!(
        one(
            Down::routes(),
            &mut Down::new(),
            CLIENTBOUND,
            "update_enabled_features",
            &experimental
        ),
        ("update_enabled_features", experimental.clone())
    );
    assert_eq!(
        one(
            Up::routes(),
            &mut Up::new(),
            CLIENTBOUND,
            "update_enabled_features",
            &experimental
        ),
        ("update_enabled_features", vanilla.clone())
    );
}

#[test]
fn malformed_packets_fail_without_panicking() {
    mangle(
        Up::routes(),
        Up::new,
        CLIENTBOUND,
        "open_sign_editor",
        &hex(SIGN),
    );
    mangle(
        Down::routes(),
        Down::new,
        CLIENTBOUND,
        "open_sign_editor",
        &[hex(SIGN), vec![1]].concat(),
    );
    mangle(
        Up::routes(),
        Up::new,
        CLIENTBOUND,
        "player_combat_end",
        &[60, 0, 0, 0, 7],
    );
    mangle(
        Down::routes(),
        Down::new,
        CLIENTBOUND,
        "player_combat_end",
        &[60],
    );
    mangle(
        Up::routes(),
        Up::new,
        CLIENTBOUND,
        "update_enabled_features",
        &hex(VANILLA),
    );
    mangle(
        Down::routes(),
        Down::new,
        CLIENTBOUND,
        "update_enabled_features",
        &hex(VANILLA),
    );
    mangle(
        Up::routes(),
        Up::new,
        SERVERBOUND,
        "sign_update",
        &sign_update(Some(true)),
    );
    mangle(
        Down::routes(),
        Down::new,
        SERVERBOUND,
        "sign_update",
        &sign_update(None),
    );
    if let (Some(old), Some(new)) = (recorded_login(OLDER), recorded_login(NEWER)) {
        mangle(Up::routes(), Up::new, CLIENTBOUND, "login", &old);
        mangle(Down::routes(), Down::new, CLIENTBOUND, "login", &new);
    }
}

#[test]
fn every_recorded_packet_translates() {
    let (Some(old), Some(new)) = (corpus(OLDER), corpus(NEWER)) else {
        return;
    };
    let mut count = 0;
    let mut run = |protocol: i32, packets: &[Recorded], up: &mut Up, down: &mut Down| {
        for packet in packets.iter().filter(|packet| packet.state == State::Play) {
            let name = packet.name(protocol);
            let upgrading = (protocol == OLDER) == (packet.direction == CLIENTBOUND);
            let result = if upgrading {
                send(Up::routes(), up, packet.direction, name, &packet.data)
            } else {
                send(Down::routes(), down, packet.direction, name, &packet.data)
            };
            result.unwrap_or_else(|error| panic!("{protocol} {name}: {error}"));
            count += 1;
        }
    };
    let (mut up, mut down) = (Up::new(), Down::new());
    run(OLDER, &old, &mut up, &mut down);
    run(NEWER, &new, &mut up, &mut down);
    assert!(count > 2000, "{count}");
}

/// The join game of the `backend` session, through a whole translator for
/// a `client`: the registry codec the client gets.
fn chained_codec(backend: i32, client: i32) -> Option<Compound> {
    let session = corpus(backend)?;
    let mut translator = Translator::new(client, backend).unwrap().unwrap();
    for packet in session
        .iter()
        .filter(|packet| packet.direction == CLIENTBOUND)
    {
        let mut out = Output::default();
        translator
            .clientbound(Packet::new(packet.id, packet.data.clone()), &mut out)
            .unwrap();
        if packet.name(backend) == "login" {
            let sent = out
                .client
                .into_iter()
                .find(|sent| sent.state == State::Play)?;
            assert_eq!(
                data::version(client).packet_name(State::Play, CLIENTBOUND, sent.packet.id),
                Some("login")
            );
            return Some(codec_of(client, &sent.packet.data));
        }
    }
    None
}

#[test]
fn chains_across_both_pairs_give_each_client_its_own_codec() {
    let (Some(down), Some(up)) = (chained_codec(763, 761), chained_codec(761, 763)) else {
        return;
    };
    // A 1.19.3 client: no 1.19.4 registries, named precipitation, inline
    // sounds it knows.
    for registry in ["damage_type", "trim_pattern", "trim_material"] {
        assert!(
            !down.contains(&format!("minecraft:{registry}")),
            "{registry}"
        );
    }
    let sounds = data::version(761).expect_registry("sound_event");
    for (name, biome) in entries(&down, "worldgen/biome") {
        let element = biome.compound("element").unwrap();
        assert!(element.str("precipitation").is_some(), "{name}");
        let music = element.compound("effects").unwrap().compound("music");
        if let Some(music) = music {
            let id = music.compound("sound").unwrap().str("sound_id").unwrap();
            assert!(sounds.id(id).is_some(), "{name}: {id}");
        }
    }
    // A 1.20 client: every 1.20 damage type, IDs from 0 without gaps.
    let types = ids(&up, "damage_type");
    assert_eq!(types.len(), 44);
    let mut numbers: Vec<i32> = types.iter().map(|(_, id)| *id).collect();
    numbers.sort();
    assert_eq!(numbers, (0..44).collect::<Vec<_>>());
    for (name, biome) in entries(&up, "worldgen/biome") {
        let element = biome.compound("element").unwrap();
        assert!(element.int("has_precipitation").is_some(), "{name}");
    }
}
