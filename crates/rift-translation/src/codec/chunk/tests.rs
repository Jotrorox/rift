use super::{
    light::LightUpdate,
    model::{Chunk, World},
};
use crate::{
    Direction, State,
    codec::testing::{Recorded, corpus},
    version::PROTOCOLS,
};

fn recorded(protocol: i32, names: &[&str]) -> Vec<(&'static str, Recorded)> {
    let Some(packets) = corpus(protocol) else {
        return Vec::new();
    };
    packets
        .into_iter()
        .filter(|packet| packet.direction == Direction::Clientbound && packet.state == State::Play)
        .filter_map(|packet| {
            let name = packet.name(protocol);
            names.contains(&name).then_some((name, packet))
        })
        .collect()
}

#[test]
fn corpus_chunks_and_light_round_trip() {
    let mut checked = 0;
    for &(protocol, release) in PROTOCOLS {
        let packets = recorded(
            protocol,
            &["level_chunk", "level_chunk_with_light", "light_update"],
        );
        if packets.is_empty() {
            eprintln!("no corpus for {release}: skipped");
            continue;
        }
        for (name, packet) in packets {
            let bytes = if name == "light_update" {
                let update = LightUpdate::read(&packet.data, protocol)
                    .unwrap_or_else(|error| panic!("{release} {name}: {error}"));
                update.write(protocol).unwrap()
            } else {
                let chunk = Chunk::read(&packet.data, protocol, &World::default())
                    .unwrap_or_else(|error| panic!("{release} {name}: {error}"));
                chunk.write(protocol).unwrap()
            };
            assert!(bytes == packet.data, "{release} {name} does not round trip");
            checked += 1;
        }
    }
    eprintln!("{checked} chunk and light packets round trip");
}
