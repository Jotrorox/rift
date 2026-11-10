//! Every adjacent pair in both directions, without recorded traffic: the
//! whole vanilla tag set, a command graph using every parser, every sound
//! event and every menu convert, and the receiver's reader accepts the
//! result.
use super::{commands, screen, sound, tags};
use crate::{
    Direction, State,
    codec::{Shared, testing::Defaults},
    data,
    version::PROTOCOLS,
    wire::{Rewriter, Writer},
};
use std::io;

/// (receiver, sender) for both directions of every adjacent pair.
fn directions() -> impl Iterator<Item = (i32, i32)> {
    PROTOCOLS.windows(2).flat_map(|pair| {
        let (older, newer) = (pair[0].0, pair[1].0);
        [(newer, older), (older, newer)]
    })
}

#[test]
fn vanilla_tags_convert_across_every_pair() {
    for (client, backend) in directions() {
        if backend < 393 {
            continue;
        }
        let body = tags::vanilla_update_tags(backend).unwrap();
        let mut step = Defaults::new(client, backend);
        let out = step
            .clientbound(State::Play, "update_tags", &body)
            .unwrap_or_else(|error| panic!("{backend} -> {client}: {error}"));
        if client < 393 {
            assert!(out.is_empty());
            continue;
        }
        let data = out.first().map_or(&body, |out| &out.data);
        let set = tags::read(client, data).unwrap();
        // Upgrading, the receiver has every vanilla tag of its version.
        if client > backend {
            let vanilla = super::registry::vanilla_tags(client).unwrap();
            for (registry, expected) in &vanilla.registries {
                let sent = set
                    .registries
                    .iter()
                    .find(|tags| data::strip(&tags.registry) == *registry)
                    .unwrap_or_else(|| panic!("{backend} -> {client} lacks {registry} tags"));
                for (tag, _) in expected {
                    assert!(
                        sent.tags.iter().any(|(name, _)| data::strip(name) == *tag),
                        "{backend} -> {client} lacks {registry} {tag}"
                    );
                }
            }
        }
    }
}

/// Default properties of a parser in `protocol`, as the server would send.
fn properties(protocol: i32, parser: &str) -> commands::Properties {
    use commands::Properties;
    match parser {
        "brigadier:float" | "brigadier:integer" => Properties::Number {
            flags: 1,
            bounds: vec![0; 4],
        },
        "brigadier:double" | "brigadier:long" => Properties::Number {
            flags: 2,
            bounds: vec![0; 8],
        },
        "brigadier:string" => Properties::String(1),
        "minecraft:entity" => Properties::Flags(3),
        "minecraft:score_holder" => Properties::Flags(1),
        "minecraft:time" if protocol >= 762 => Properties::Time(0),
        "minecraft:resource" | "minecraft:resource_or_tag" => {
            Properties::Registry("minecraft:mob_effect".into())
        }
        "minecraft:resource_key"
        | "minecraft:resource_or_tag_key"
        | "minecraft:resource_selector" => {
            Properties::Registry("minecraft:worldgen/structure".into())
        }
        _ => Properties::None,
    }
}

fn every_parser(protocol: i32) -> commands::Commands {
    let parsers: Vec<String> = if protocol >= 759 {
        data::version(protocol)
            .expect_registry("command_argument_type")
            .names()
            .map(|(_, name)| {
                if name.contains(':') {
                    name.to_owned()
                } else {
                    format!("minecraft:{name}")
                }
            })
            .collect()
    } else {
        // Probe every name any protocol knows; keep those this one reads.
        let mut names: Vec<String> = Vec::new();
        for &(other, _) in PROTOCOLS.iter().filter(|(other, _)| *other >= 759) {
            for (_, name) in data::version(other)
                .expect_registry("command_argument_type")
                .names()
            {
                let name = if name.contains(':') {
                    name.to_owned()
                } else {
                    format!("minecraft:{name}")
                };
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names.extend(["minecraft:nbt".into(), "minecraft:color".into()]);
        names
            .into_iter()
            .filter(|name| {
                let graph = graph(protocol, vec![name.clone()]);
                commands::write(protocol, &graph)
                    .and_then(|body| commands::read(protocol, &body))
                    .is_ok()
            })
            .collect()
    };
    graph(protocol, parsers)
}

fn graph(protocol: i32, parsers: Vec<String>) -> commands::Commands {
    let mut nodes = vec![commands::Node {
        flags: 0,
        children: (1..=parsers.len() as i32).collect(),
        redirect: None,
        name: None,
        parser: None,
        suggestions: None,
    }];
    for (index, parser) in parsers.into_iter().enumerate() {
        nodes.push(commands::Node {
            // An executable argument with a redirect to the root.
            flags: 2 | 4 | 8,
            children: vec![],
            redirect: Some(0),
            name: Some(format!("a{index}")),
            parser: Some(commands::Parser {
                properties: properties(protocol, &parser),
                name: parser,
            }),
            suggestions: None,
        });
    }
    commands::Commands { nodes, root: 0 }
}

#[test]
fn every_parser_converts_across_every_pair() {
    for (client, backend) in directions() {
        if backend < 393 || client < 393 {
            continue;
        }
        let graph = every_parser(backend);
        assert!(
            graph.nodes.len() > 30,
            "{backend} has {} parsers",
            graph.nodes.len() - 1
        );
        let body = commands::write(backend, &graph).unwrap();
        let mut step = Defaults::new(client, backend);
        let out = step
            .clientbound(State::Play, "commands", &body)
            .unwrap_or_else(|error| panic!("{backend} -> {client}: {error}"));
        let data = out.first().map_or(&body, |out| &out.data);
        let converted = commands::read(client, data).unwrap();
        assert_eq!(converted.nodes.len(), graph.nodes.len());
        for (before, after) in graph.nodes.iter().zip(&converted.nodes) {
            assert_eq!(before.redirect, after.redirect);
            assert_eq!(before.flags & 0x0f, after.flags & 0x0f);
        }
    }
}

#[test]
fn every_sound_converts_across_every_pair() {
    for (client, backend) in directions() {
        let registry = data::version(backend).expect_registry("sound_event");
        let mut step = Defaults::new(client, backend);
        let seed = (backend >= 759).then_some(1);
        for (id, _) in registry.names() {
            let sound = sound::Sound {
                holder: sound::Holder::Id(id),
                category: 0,
                volume: 1.0,
                pitch: 1.0,
                seed,
            };
            let (_, body) = sound::write(backend, &sound, sound::Emitter::Position([0, 0, 0]))
                .unwrap()
                .unwrap();
            for out in step
                .clientbound(State::Play, "sound", &body)
                .unwrap_or_else(|error| panic!("{backend} -> {client} sound {id}: {error}"))
            {
                let kind = if out.name == "custom_sound" {
                    sound::Kind::CustomSound
                } else {
                    sound::Kind::Sound
                };
                sound::read(client, kind, &out.data).unwrap();
            }
        }
    }
}

/// Titles are the text codec's; copy them as they are.
fn raw_title(shared: &mut Shared, direction: Direction, rw: &mut Rewriter<'_>) -> io::Result<()> {
    if shared.source(direction) >= 765 {
        rw.nbt(false).map(drop)
    } else {
        rw.str().map(drop)
    }
}

#[test]
fn every_menu_converts_across_every_pair() {
    for (client, backend) in directions() {
        if backend < 477 || (client >= 765) != (backend >= 765) {
            continue;
        }
        let menus = data::version(backend).expect_registry("menu");
        for (id, menu) in menus.names() {
            let mut writer = Writer::new();
            writer.varint(1).varint(id);
            if backend >= 765 {
                crate::nbt::Nbt::String("T".into()).write(&mut writer, false);
            } else {
                writer.string(r#"{"text":"T"}"#);
            }
            let mut shared = Shared::new(client, backend);
            let converted = screen::convert_open_screen(
                &mut shared,
                Direction::Clientbound,
                &writer.buf,
                raw_title,
            )
            .unwrap_or_else(|error| panic!("{backend} -> {client} {menu}: {error}"));
            assert_eq!(converted.name, "open_screen");
        }
    }
}

/// Random bytes and every truncation of valid packets, through every
/// handler of every pair: errors, never panics.
#[test]
fn malformed_input_fails_without_panicking() {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut random = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let names = [
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
    ];
    for (client, backend) in directions() {
        let mut step = Defaults::new(client, backend);
        let source = data::version(backend);
        let mut samples: Vec<Vec<u8>> = (0..24)
            .map(|_| {
                let size = (random() % 48) as usize;
                (0..size).map(|_| random() as u8).collect()
            })
            .collect();
        if backend >= 393 {
            let body = commands::write(backend, &every_parser(backend)).unwrap();
            samples.extend((0..body.len()).step_by(61).map(|end| body[..end].to_vec()));
            let tags = tags::vanilla_update_tags(backend).unwrap();
            samples.extend(
                (0..tags.len())
                    .step_by(4093)
                    .map(|end| tags[..end].to_vec()),
            );
        }
        for name in names {
            if source
                .packet_id(State::Play, Direction::Clientbound, name)
                .is_none()
            {
                continue;
            }
            for sample in &samples {
                let _ = step.clientbound(State::Play, name, sample);
            }
        }
        if source
            .packet_id(State::Play, Direction::Serverbound, "set_beacon")
            .is_some()
        {
            let mut reverse = Defaults::new(backend, client);
            for sample in &samples {
                let _ = reverse.serverbound(State::Play, "set_beacon", sample);
            }
        }
    }
}
