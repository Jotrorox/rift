//! Real packets from official servers, kept inline so that the codec is
//! tested against real traffic without the corpus, each sent through every
//! step to the oldest and the newest protocol.
use super::{commands, sound, stats};
use crate::{
    State,
    codec::testing::Defaults,
    data,
    version::{PROTOCOLS, index},
};

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect()
}

/// Send a backend packet of protocol `from` through every step to a client
/// of protocol `to`; the packets that arrive, by name.
fn chain(from: i32, to: i32, name: &str, body: Vec<u8>) -> Vec<(String, Vec<u8>)> {
    let (start, end) = (index(from).unwrap(), index(to).unwrap());
    let mut packets = vec![(name.to_owned(), body)];
    let mut position = start;
    while position != end {
        let next = if end > start {
            position + 1
        } else {
            position - 1
        };
        let (backend, client) = (PROTOCOLS[position].0, PROTOCOLS[next].0);
        let mut step = Defaults::new(client, backend);
        packets = packets
            .into_iter()
            .flat_map(|(name, body)| {
                step.clientbound(State::Play, &name, &body)
                    .unwrap_or_else(|error| panic!("{name} from {backend} to {client}: {error}"))
            })
            .map(|out| (out.name, out.data))
            .collect();
        position = next;
    }
    packets
}

/// 1.13's command graph for a player who is not an operator.
const COMMANDS_1_13: &str = "110007010203040506070501080468656c70050109046c69737401010a026d6501010b036d73670900040474656c6c01010c07747269676765720900040177060007636f6d6d616e64106272696761646965723a737472696e67020500057575696473060006616374696f6e106272696761646965723a737472696e670202010d0774617267657473106d696e6563726166743a656e746974790216020e0f096f626a656374697665136d696e6563726166743a6f626a656374697665146d696e6563726166743a61736b5f7365727665720600076d657373616765116d696e6563726166743a6d657373616765010110036164640101100373657406000576616c7565116272696761646965723a696e74656765720000";
/// 1.21.6's, with numbered parsers and the `random` command.
const COMMANDS_1_21_6: &str = "1a000a0102030405060708090a01010b026d6505010c0468656c7005010d046c69737401010e036d73670900040474656c6c090004017701020f100672616e646f6d010111077465616d6d736709000802746d0101120774726967676572060006616374696f6e14060007636f6d6d616e6405020500057575696473020113077461726765747306020101140576616c756501011504726f6c6c0600076d6573736167651416021617096f626a65637469766518146d696e6563726166743a61736b5f7365727665720600076d6573736167651406000572616e67652706000572616e676527010118036164640101190373657406000576616c7565030006000576616c7565030000";
/// `/playsound minecraft:entity.pig.ambient` on 1.19.2: a named sound.
const CUSTOM_SOUND_1_19_2: &str = "1c6d696e6563726166743a656e746974792e7069672e616d6269656e74000000004cfffffe200000001c3f8000003f80000029f4b15d55b280d2";
/// A registry sound with a seed on 1.19.3.
const SOUND_1_19_3: &str = "4c06fffffff4fffffe20000000043f8000003f8207a12bc36933c044ace8";
/// A registry sound on 1.12.2.
const SOUND_1_12_2: &str = "9e0207fffff50400000020fffff4243e4ccccd40008c2d";
/// Three custom statistics on 1.16.5.
const STATS_1_16_5: &str = "030803a3010801a3010802a301";

fn names(graph: &commands::Commands) -> Vec<Option<String>> {
    graph.nodes.iter().map(|node| node.name.clone()).collect()
}

#[test]
fn real_command_graphs_reach_every_protocol() {
    for (protocol, text) in [(393, COMMANDS_1_13), (771, COMMANDS_1_21_6)] {
        let body = hex(text);
        let graph = commands::read(protocol, &body).unwrap();
        assert_eq!(commands::write(protocol, &graph).unwrap(), body);
        for target in [393, 777] {
            let out = chain(protocol, target, "commands", body.clone());
            let converted = commands::read(target, &out[0].1).unwrap();
            assert_eq!(names(&converted), names(&graph), "{protocol} -> {target}");
        }
        // 1.12 has no command graph.
        assert!(chain(protocol, 340, "commands", body).is_empty());
    }
}

fn sound_name(protocol: i32, name: &str, body: &[u8]) -> Option<String> {
    let kind = match name {
        "sound" => sound::Kind::Sound,
        "custom_sound" => sound::Kind::CustomSound,
        _ => sound::Kind::SoundEntity,
    };
    match sound::read(protocol, kind, body).unwrap().0.holder {
        sound::Holder::Id(id) => data::version(protocol)
            .expect_registry("sound_event")
            .name(id)
            .map(|name| format!("minecraft:{name}")),
        sound::Holder::Named { name, .. } => Some(name),
    }
}

#[test]
fn real_sounds_keep_their_meaning_through_every_step() {
    let custom = hex(CUSTOM_SOUND_1_19_2);
    let pig = Some("minecraft:entity.pig.ambient".to_owned());
    assert_eq!(sound_name(760, "custom_sound", &custom), pig);
    for target in [393, 777] {
        let out = chain(760, target, "custom_sound", custom.clone());
        assert_eq!(sound_name(target, &out[0].0, &out[0].1), pig, "{target}");
    }
    let registered = hex(SOUND_1_19_3);
    let original = sound_name(761, "sound", &registered);
    // 1.13 has no axolotls: their sound arrives as the rules' stand-in.
    let stand_in = Some("minecraft:entity.dolphin.ambient".to_owned());
    for (target, expected) in [(393, &stand_in), (777, &original)] {
        let out = chain(761, target, "sound", registered.clone());
        assert_eq!(
            sound_name(target, &out[0].0, &out[0].1),
            *expected,
            "{target}"
        );
    }
    // Up from 1.12 the sound maps through the flattening's sound renames.
    let legacy = hex(SOUND_1_12_2);
    let out = chain(340, 777, "sound", legacy);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, "sound");
}

#[test]
fn real_statistics_reach_every_protocol() {
    let body = hex(STATS_1_16_5);
    let original = stats::read(754, &body).unwrap();
    assert_eq!(original.len(), 3);
    let custom = |protocol: i32, stats: &[stats::Stat]| -> Vec<String> {
        let (_, registry) = super::registry::registry(protocol, "custom_stat").unwrap();
        stats
            .iter()
            .map(|stat| match stat {
                stats::Stat::Typed(_, id, _) => registry.name(*id).unwrap().to_owned(),
                stats::Stat::Named(name, _) => name.clone(),
            })
            .collect()
    };
    let expected = custom(754, &original);
    let out = chain(754, 393, "award_stats", body.clone());
    assert_eq!(custom(393, &stats::read(393, &out[0].1).unwrap()), expected);
    // 1.17 renamed play_one_minute to play_time: the statistic survives
    // under its new name once the 754-755 rules rename it, and is dropped
    // until then. Every other statistic arrives as it was.
    let out = chain(754, 777, "award_stats", body.clone());
    let converted = custom(777, &stats::read(777, &out[0].1).unwrap());
    let renamed: Vec<String> = expected
        .iter()
        .map(|name| {
            if name == "play_one_minute" {
                "play_time".into()
            } else {
                name.clone()
            }
        })
        .collect();
    assert!(
        converted.iter().all(|name| renamed.contains(name)),
        "{converted:?}"
    );
    assert!(converted.len() >= expected.len() - 1);
    let out = chain(754, 340, "award_stats", body);
    let legacy: Vec<stats::Stat> = stats::read(340, &out[0].1).unwrap();
    // 1.12 has no time_since_rest (phantoms came with 1.13).
    let legacy: Vec<&str> = legacy
        .iter()
        .map(|stat| match stat {
            stats::Stat::Named(name, _) => name.as_str(),
            stats::Stat::Typed(..) => "",
        })
        .collect();
    assert_eq!(legacy, ["stat.playOneMinute", "stat.timeSinceDeath"]);
}
