//! Tags, statistics, command trees, sounds and menus.
//!
//! Each packet's handler is installed only for the pairs whose two
//! protocols lay it out differently or number a registry it refers to
//! differently; elsewhere it passes through untouched.
pub mod commands;
pub mod container;
pub mod registry;
pub mod screen;
pub mod sound;
pub mod stats;
pub mod tags;

use super::HasShared;
use crate::{Direction, State as Phase, step::RouteBuilder};

/// What this codec keeps for one step of one connection.
#[derive(Default)]
pub struct State {
    /// Drives the seeds given to sounds that gain one (1.19 and later).
    seed: u64,
    /// The window last opened, for its data properties.
    window: container::Window,
}

impl State {
    /// A seed for a sound whose sender did not give one (SplitMix64).
    pub fn next_seed(&mut self) -> i64 {
        self.seed = self.seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.seed;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (value ^ (value >> 31)) as i64
    }
}

/// Install this codec's default routes for the builder's pair.
pub fn install<S: HasShared + 'static>(builder: RouteBuilder<S>) -> RouteBuilder<S> {
    use Direction::{Clientbound, Serverbound};
    let (source, target) = (builder.backend().protocol, builder.client().protocol);
    let mut builder = builder;
    if tags::needed(source, target) {
        for phase in [Phase::Configuration, Phase::Play] {
            builder =
                builder.default_handler(Clientbound, phase, "update_tags", tags::update_tags::<S>);
        }
    }
    if commands::needed(source, target) {
        builder = builder.default_handler(
            Clientbound,
            Phase::Play,
            "commands",
            commands::commands::<S>,
        );
    }
    if stats::needed(source, target) {
        builder = builder.default_handler(
            Clientbound,
            Phase::Play,
            "award_stats",
            stats::award_stats::<S>,
        );
    }
    for (kind, name, handler) in [
        (
            sound::Kind::Sound,
            "sound",
            sound::sound::<S> as crate::step::Handler<S>,
        ),
        (
            sound::Kind::CustomSound,
            "custom_sound",
            sound::custom_sound::<S>,
        ),
        (
            sound::Kind::SoundEntity,
            "sound_entity",
            sound::sound_entity::<S>,
        ),
    ] {
        if sound::needed(kind, source, target) {
            builder = builder.default_handler(Clientbound, Phase::Play, name, handler);
        }
    }
    if source >= 393 && sound::stop_needed(source, target) {
        builder = builder.default_handler(
            Clientbound,
            Phase::Play,
            "stop_sound",
            sound::stop_sound::<S>,
        );
    }
    if screen::open_needed(source, target) {
        builder = builder.default_handler(
            Clientbound,
            Phase::Play,
            "open_screen",
            screen::open_screen::<S>,
        );
    }
    if source >= 477 && screen::mount_needed(source, target) {
        builder = builder.default_handler(
            Clientbound,
            Phase::Play,
            "mount_screen_open",
            screen::mount_screen_open::<S>,
        );
    }
    if container::data_needed(source, target) {
        builder = builder
            .tap(
                Clientbound,
                Phase::Play,
                "open_screen",
                container::track_open_screen::<S>,
            )
            .default_handler(
                Clientbound,
                Phase::Play,
                "container_set_data",
                container::container_set_data::<S>,
            );
    }
    // Serverbound packets travel from the client's protocol to the backend's.
    if target >= 393 && container::beacon_needed(target, source) {
        builder = builder.default_handler(
            Serverbound,
            Phase::Play,
            "set_beacon",
            container::set_beacon::<S>,
        );
    }
    builder
}

/// Real sessions for this codec's tests: the shared corpus and, when
/// present, a supplementary one recorded with an operator player who also
/// requests statistics (`$RIFT_MISC_CORPUS`, default
/// `~/.cache/rift-via/misc-corpus`), so that command graphs hold every
/// command. Empty when neither was captured on this machine.
#[cfg(test)]
pub(crate) fn sessions(protocol: i32) -> Vec<Vec<super::testing::Recorded>> {
    use super::testing::{Recorded, corpus};
    let mut sessions: Vec<Vec<Recorded>> = corpus(protocol).into_iter().collect();
    let directory = std::env::var_os("RIFT_MISC_CORPUS")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            Some(
                std::path::PathBuf::from(std::env::var_os("HOME")?)
                    .join(".cache/rift-via/misc-corpus"),
            )
        });
    let Some(text) = directory.and_then(|directory| {
        std::fs::read_to_string(directory.join(format!("{protocol}.jsonl"))).ok()
    }) else {
        return sessions;
    };
    let mut packets = Vec::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line).expect("corpus line is JSON");
        let hex = value["hex"].as_str().expect("corpus line has hex");
        packets.push(Recorded {
            direction: match value["d"].as_str() {
                Some("c") => Direction::Clientbound,
                _ => Direction::Serverbound,
            },
            state: match value["s"].as_str() {
                Some("login") => Phase::Login,
                Some("configuration") => Phase::Configuration,
                _ => Phase::Play,
            },
            id: value["id"].as_i64().expect("corpus line has an id") as i32,
            data: (0..hex.len())
                .step_by(2)
                .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("corpus hex"))
                .collect(),
        });
    }
    sessions.push(packets);
    sessions
}

#[cfg(test)]
mod tests {
    use crate::{codec::testing::Defaults, step::RouteBuilder, version::PROTOCOLS};

    /// The packets this codec converts by default.
    const OWN: &[&str] = &[
        "update_tags",
        "award_stats",
        "commands",
        "sound",
        "custom_sound",
        "sound_entity",
        "stop_sound",
        "open_screen",
        "mount_screen_open",
        "container_set_data",
        "set_beacon",
    ];

    #[test]
    fn every_pair_routes_this_codecs_packets() {
        for pair in PROTOCOLS.windows(2) {
            let (older, newer) = (pair[0].0, pair[1].0);
            for (client, backend) in [(newer, older), (older, newer)] {
                let routes = RouteBuilder::<Defaults>::new(client, backend)
                    .defaults()
                    .build();
                let problems: Vec<&String> = routes
                    .problems()
                    .iter()
                    .filter(|problem| {
                        problem
                            .split_whitespace()
                            .nth(2)
                            .is_some_and(|name| OWN.contains(&name))
                    })
                    .collect();
                assert!(problems.is_empty(), "{client} on {backend}: {problems:?}");
            }
        }
    }
}

/// Writes every conversion of this codec's packets in the recorded sessions
/// to `$RIFT_MISC_DUMP` (one JSON object per line: the receiver's protocol,
/// state, direction, packet ID and body), for checking with an independent
/// decoder such as tests/protodef.py.
#[cfg(test)]
#[test]
#[ignore]
fn dump_conversions() {
    use crate::{codec::testing::Defaults, version::PROTOCOLS};
    use std::io::Write;
    let path = std::env::var_os("RIFT_MISC_DUMP").expect("set RIFT_MISC_DUMP");
    let mut out = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let own = [
        "update_tags",
        "award_stats",
        "commands",
        "sound",
        "custom_sound",
        "sound_entity",
        "stop_sound",
        "open_screen",
        "mount_screen_open",
        "container_set_data",
        "set_beacon",
    ];
    for (index, &(protocol, _)) in PROTOCOLS.iter().enumerate() {
        for packet in sessions(protocol).into_iter().flatten() {
            if !own.contains(&packet.name(protocol)) {
                continue;
            }
            for neighbour in [index.checked_sub(1), Some(index + 1)]
                .into_iter()
                .flatten()
            {
                let Some(&(other, _)) = PROTOCOLS.get(neighbour) else {
                    continue;
                };
                let (client, backend) = match packet.direction {
                    Direction::Clientbound => (other, protocol),
                    Direction::Serverbound => (protocol, other),
                };
                let mut step = Defaults::new(client, backend);
                let converted = step
                    .send_id(packet.direction, packet.state, packet.id, &packet.data)
                    .unwrap();
                for converted in converted {
                    let receiver = match converted.direction {
                        Direction::Clientbound => client,
                        Direction::Serverbound => backend,
                    };
                    let id = crate::data::version(receiver)
                        .packet_id(converted.state, converted.direction, &converted.name)
                        .unwrap_or_else(|| {
                            panic!(
                                "{} from {protocol} to {receiver} became an unknown packet",
                                packet.name(protocol)
                            )
                        });
                    let hex: String = converted
                        .data
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect();
                    writeln!(
                        out,
                        r#"{{"from":{protocol},"protocol":{receiver},"state":"{:?}","direction":"{:?}","name":"{}","id":{id},"hex":"{hex}"}}"#,
                        converted.state, converted.direction, converted.name
                    )
                    .unwrap();
                }
            }
        }
    }
}
