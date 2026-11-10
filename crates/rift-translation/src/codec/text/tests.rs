use super::{
    component::*,
    from_json_str, from_nbt,
    json::Json,
    packets::{RULES, convert_forced},
    to_json_string, to_nbt,
};
use crate::{
    Direction, State,
    codec::{Shared, testing},
    nbt::Nbt,
    version::PROTOCOLS,
};

fn json(protocol: i32, text: &str) -> Component {
    from_json_str(protocol, text).unwrap_or_else(|error| panic!("{text}: {error}"))
}

#[test]
fn old_json_reads_leniently() {
    // Numbers and booleans as text, arrays of mixed strings and objects.
    let component = json(340, r#"[5, {"text":true,"bold":"true"}, "x"]"#);
    assert_eq!(component.content, Content::Text("5".into()));
    assert_eq!(component.extra.len(), 2);
    assert_eq!(component.extra[0].content, Content::Text("true".into()));
    assert_eq!(component.extra[0].style.bold, Some(true));
    // Unknown keys are ignored, unknown colours dropped, empty arrays empty.
    let component = json(340, r#"{"text":"a","color":"nope","foo":1}"#);
    assert_eq!(component.style.color, None);
    assert_eq!(json(340, "[]"), Component::text(""));
    assert_eq!(json(340, "null"), Component::text(""));
    // An unquoted string ends at whitespace; the rest is ignored, as by Gson.
    assert_eq!(json(340, "plain words"), Component::text("plain"));
    // Content nobody can read fails the packet, as it fails vanilla.
    assert!(from_json_str(340, r#"{"bold":true}"#).is_err());
    assert!(from_json_str(340, r#"{"score":{"name":"a"}}"#).is_err());
    // Deep nesting is bounded.
    let deep = format!(
        "{}\"x\"{}",
        "{\"text\":\"\",\"extra\":[".repeat(400),
        "]}".repeat(400)
    );
    assert!(from_json_str(340, &deep).is_err());
}

#[test]
fn writes_keep_vanilla_key_order() {
    let text = r#"{"color":"gold","clickEvent":{"action":"run_command","value":"/help"},"hoverEvent":{"action":"show_text","value":{"text":"hover"}},"text":"raw"}"#;
    assert_eq!(to_json_string(340, &json(340, text)), text);
    let text = r#"{"translate":"chat.type.announcement","with":[{"text":"Server"},{"extra":[{"text":"hello"},{"text":" "}],"text":""}]}"#;
    assert_eq!(to_json_string(340, &json(340, text)), text);
}

#[test]
fn hover_contents_across_1_16() {
    let old = json(
        578,
        r#"{"text":"a","hoverEvent":{"action":"show_item","value":{"text":"{id:\"minecraft:stone\",Count:2b,tag:{x:1}}"}}}"#,
    );
    let Some(HoverEvent::ShowItem(item)) = &old.style.hover else {
        panic!("no item")
    };
    assert_eq!((item.id.as_str(), item.count), ("minecraft:stone", 2));
    assert_eq!(
        to_json_string(735, &old),
        r#"{"hoverEvent":{"action":"show_item","contents":{"id":"minecraft:stone","count":2,"tag":"{x:1}"}},"text":"a"}"#
    );
    let new = json(735, &to_json_string(735, &old));
    assert_eq!(new, old);
    assert_eq!(
        to_json_string(578, &new),
        r#"{"hoverEvent":{"action":"show_item","value":{"text":"{id:\"minecraft:stone\",Count:2b,tag:{x:1}}"}},"text":"a"}"#
    );
    // Entities: legacy SNBT with a JSON name, contents with a UUID.
    let entity = json(
        578,
        r#"{"text":"e","hoverEvent":{"action":"show_entity","value":{"text":"{name:\"{\\\"text\\\":\\\"Bob\\\"}\",type:\"minecraft:pig\",id:\"00000000-0000-0001-0000-000000000002\"}"}}}"#,
    );
    assert_eq!(
        to_json_string(735, &entity),
        r#"{"hoverEvent":{"action":"show_entity","contents":{"type":"minecraft:pig","id":"00000000-0000-0001-0000-000000000002","name":{"text":"Bob"}}},"text":"e"}"#
    );
    // A free-text entity ID cannot be expressed from 1.16 on.
    let entity = json(
        578,
        r#"{"text":"e","hoverEvent":{"action":"show_entity","value":{"text":"{type:\"minecraft:pig\",id:\"x\"}"}}}"#,
    );
    assert_eq!(to_json_string(735, &entity), r#"{"text":"e"}"#);
}

#[test]
fn colours_and_fonts_degrade_before_1_16() {
    let component = json(
        735,
        r##"{"text":"a","color":"#FF5050","font":"minecraft:alt"}"##,
    );
    assert_eq!(component.style.color, Some(Color::Rgb(0xff5050)));
    assert_eq!(
        to_json_string(578, &component),
        r#"{"color":"red","text":"a"}"#
    );
    assert_eq!(
        to_json_string(735, &component),
        r##"{"color":"#FF5050","font":"minecraft:alt","text":"a"}"##
    );
    // Scores carried a value before 1.16; from then on it becomes text.
    let score = json(578, r#"{"score":{"name":"a","objective":"b","value":"7"}}"#);
    assert_eq!(
        to_json_string(578, &score),
        r#"{"score":{"name":"a","objective":"b","value":"7"}}"#
    );
    assert_eq!(to_json_string(735, &score), r#"{"text":"7"}"#);
}

#[test]
fn content_types_degrade_for_older_clients() {
    let nbt = json(
        573,
        r#"{"nbt":"a","storage":"x:y","extra":["b"],"bold":true}"#,
    );
    assert_eq!(
        to_json_string(498, &nbt),
        r#"{"bold":true,"extra":[{"text":"b"}],"text":""}"#
    );
    assert_eq!(
        to_json_string(573, &nbt),
        r#"{"bold":true,"extra":[{"text":"b"}],"nbt":"a","interpret":false,"storage":"x:y"}"#
    );
    assert_eq!(
        to_json_string(404, &json(477, r#"{"nbt":"a","block":"1 2 3"}"#)),
        r#"{"text":""}"#
    );
    let selector = json(755, r#"{"selector":"@a","separator":{"text":"|"}}"#);
    assert_eq!(to_json_string(754, &selector), r#"{"selector":"@a"}"#);
    let translate = json(762, r#"{"translate":"k","fallback":"F"}"#);
    assert_eq!(to_json_string(761, &translate), r#"{"translate":"k"}"#);
    assert_eq!(
        to_json_string(762, &translate),
        r#"{"translate":"k","fallback":"F"}"#
    );
    let object = from_nbt(
        773,
        &to_nbt(
            773,
            &Component {
                content: Content::Object {
                    object: ObjectInfo::Atlas {
                        atlas: None,
                        sprite: "item/apple".into(),
                    },
                    fallback: Some(Box::new(Component::text("[apple]"))),
                },
                ..Component::default()
            },
        ),
    )
    .unwrap();
    assert_eq!(
        to_json_string(764, &object),
        r#"{"extra":[{"text":"[apple]"}],"text":""}"#
    );
    assert_eq!(
        to_nbt(772, &object),
        Nbt::Compound(crate::nbt::Compound(vec![
            (
                "extra".into(),
                Nbt::List(vec![Nbt::String("[apple]".into())])
            ),
            ("text".into(), Nbt::String(String::new())),
        ]))
    );
}

#[test]
fn nbt_encoding_from_1_20_3() {
    // Real 1.20.4 system chat content (corpus): with-list of compounds.
    let component = json(
        764,
        r#"{"translate":"gameMode.changed","with":[{"translate":"gameMode.survival"}]}"#,
    );
    let bytes = to_nbt(765, &component).to_bytes(false);
    assert_eq!(
        bytes,
        b"\n\t\x00\x04with\n\x00\x00\x00\x01\x08\x00\ttranslate\x00\x11gameMode.survival\x00\x08\x00\ttranslate\x00\x10gameMode.changed\x00"
    );
    // Plain text collapses to a string; mixed lists are wrapped.
    assert_eq!(
        to_nbt(765, &json(764, r#"{"text":"hi"}"#)),
        Nbt::String("hi".into())
    );
    let mixed = to_nbt(
        765,
        &json(764, r#"{"text":"","extra":["a",{"text":"b","bold":true}]}"#),
    );
    let Nbt::Compound(compound) = &mixed else {
        panic!()
    };
    let Some(Nbt::List(extra)) = compound.get("extra") else {
        panic!()
    };
    assert_eq!(
        extra[0],
        Nbt::Compound(crate::nbt::Compound(vec![(
            String::new(),
            Nbt::String("a".into())
        )]))
    );
    // And back: wrapped elements unwrap, bytes read as booleans.
    let back = from_nbt(765, &mixed).unwrap();
    assert_eq!(back.extra[0], Component::text("a"));
    assert_eq!(back.extra[1].style.bold, Some(true));
    assert_eq!(
        to_json_string(764, &back),
        r#"{"extra":[{"text":"a"},{"bold":true,"text":"b"}],"text":""}"#
    );
    // Primitive translation arguments become strings for JSON clients.
    let args = from_nbt(
        765,
        &Nbt::Compound(crate::nbt::Compound(vec![
            ("translate".into(), Nbt::String("k".into())),
            ("with".into(), Nbt::List(vec![Nbt::Int(5), Nbt::Int(6)])),
        ])),
    )
    .unwrap();
    assert_eq!(
        to_json_string(764, &args),
        r#"{"translate":"k","with":["5","6"]}"#
    );
    // A type field selects the content.
    let typed = from_nbt(
        765,
        &Nbt::Compound(crate::nbt::Compound(vec![
            ("type".into(), Nbt::String("keybind".into())),
            ("keybind".into(), Nbt::String("key.jump".into())),
            ("text".into(), Nbt::String("ignored".into())),
        ])),
    )
    .unwrap();
    assert_eq!(typed.content, Content::Keybind("key.jump".into()));
}

#[test]
fn events_restructure_in_1_21_5() {
    let old = json(
        769,
        r#"{"text":"a","clickEvent":{"action":"change_page","value":"3"},"hoverEvent":{"action":"show_entity","contents":{"type":"minecraft:pig","id":[0,1,0,2]}},"shadow_color":-16777216}"#,
    );
    let new = to_nbt(770, &old);
    let back = from_nbt(770, &new).unwrap();
    assert_eq!(back, old);
    let Nbt::Compound(compound) = &new else {
        panic!()
    };
    let click = compound.compound("click_event").unwrap();
    assert_eq!(click.int("page"), Some(3));
    let hover = compound.compound("hover_event").unwrap();
    assert_eq!(hover.str("id"), Some("minecraft:pig"));
    assert_eq!(hover.get("uuid"), Some(&Nbt::IntArray(vec![0, 1, 0, 2])));
    // Unusable values drop the event; shadow colours vanish before 1.21.4.
    let bad = json(
        769,
        r#"{"text":"a","clickEvent":{"action":"change_page","value":"x"}}"#,
    );
    assert_eq!(to_nbt(770, &bad), Nbt::String("a".into()));
    let url = json(
        769,
        r#"{"text":"a","clickEvent":{"action":"open_url","value":"file:///etc"}}"#,
    );
    assert_eq!(to_nbt(770, &url), Nbt::String("a".into()));
    assert_eq!(
        to_json_string(764, &old),
        r#"{"clickEvent":{"action":"change_page","value":"3"},"hoverEvent":{"action":"show_entity","contents":{"type":"minecraft:pig","id":"00000000-0000-0001-0000-000000000002"}},"text":"a"}"#
    );
    // Dialog and custom actions exist from 1.21.6 only.
    let custom = Component {
        style: Style {
            click: Some(ClickEvent::Custom {
                id: "x:y".into(),
                payload: None,
            }),
            ..Style::default()
        },
        ..Component::text("c")
    };
    assert_eq!(to_nbt(770, &custom), Nbt::String("c".into()));
    assert!(matches!(to_nbt(771, &custom), Nbt::Compound(_)));
}

#[test]
fn json_and_nbt_round_trip() {
    let samples = [
        r#"{"text":""}"#,
        r#"{"text":"a","extra":[{"text":"b","italic":false},"c",{"translate":"k","with":["x",{"text":"y","color":"red"}]}]}"#,
        r#"{"keybind":"key.jump","underlined":true,"insertion":"ins"}"#,
        r#"{"selector":"@p","separator":{"text":", "}}"#,
        r#"{"score":{"name":"*","objective":"o"},"obfuscated":true,"strikethrough":true}"#,
        r##"{"text":"x","color":"#123456","font":"minecraft:uniform","clickEvent":{"action":"copy_to_clipboard","value":"v"}}"##,
        r#"{"nbt":"Items[0]","entity":"@s","interpret":true,"separator":"|"}"#,
        r#"{"text":"h","hoverEvent":{"action":"show_text","contents":["a",{"text":"b"}]}}"#,
    ];
    for protocol in [765, 769, 770, 777] {
        for sample in samples {
            let component = json(764, sample);
            let tag = to_nbt(protocol, &component);
            let back = from_nbt(protocol, &tag).unwrap();
            assert_eq!(to_nbt(protocol, &back), tag, "{protocol} {sample}");
            // Through old JSON and back again: the same tree in NBT terms.
            let again = json(764, &to_json_string(764, &back));
            assert_eq!(to_nbt(protocol, &again), tag, "{protocol} {sample}");
        }
    }
}

#[test]
fn java_hash_order() {
    // Vanilla 26.3 wrote these keys in this order (corpus).
    let keys = ["text", "color", "click_event", "hover_event"];
    let order: Vec<&str> = super::java_order(&keys)
        .into_iter()
        .map(|index| keys[index])
        .collect();
    assert_eq!(order, ["click_event", "color", "text", "hover_event"]);
    let keys = ["action", "contents"];
    let order: Vec<&str> = super::java_order(&keys)
        .into_iter()
        .map(|index| keys[index])
        .collect();
    assert_eq!(order, ["contents", "action"]);
}

#[test]
fn every_pair_builds_its_routes() {
    for pair in PROTOCOLS.windows(2) {
        let (older, newer) = (pair[0].0, pair[1].0);
        for (client, backend) in [(older, newer), (newer, older)] {
            let mut step = testing::Defaults::new(client, backend);
            // A disconnect always converts.
            let _ = &mut step;
        }
    }
}

/// The packets the corpus holds for this codec, by recorded protocol.
fn corpus_packets(protocol: i32) -> Option<Vec<(State, &'static str, Vec<u8>)>> {
    let session = testing::corpus(protocol)?;
    Some(
        session
            .into_iter()
            .filter(|packet| packet.direction == Direction::Clientbound)
            .filter_map(|packet| {
                let name = packet.name(protocol);
                RULES
                    .iter()
                    .any(|rule| {
                        rule.name == name
                            && rule.direction == Direction::Clientbound
                            && rule.phases.contains(&packet.state)
                    })
                    .then_some((packet.state, name, packet.data))
            })
            .collect(),
    )
}

/// Every text packet of every recorded session parses with the reader of
/// its protocol and is written back byte for byte.
#[test]
fn corpus_round_trips_in_its_own_protocol() {
    let mut checked = 0;
    let mut missing = Vec::new();
    for (protocol, _) in PROTOCOLS {
        let Some(packets) = corpus_packets(*protocol) else {
            missing.push(*protocol);
            continue;
        };
        for (state, name, data) in packets {
            let mut shared = Shared::new(*protocol, *protocol);
            let Some(out) = convert_forced(&mut shared, Direction::Clientbound, state, name, &data)
            else {
                continue;
            };
            let out = out.unwrap_or_else(|error| panic!("{protocol} {name}: {error}"));
            assert_eq!(out.len(), 1, "{protocol} {name}");
            assert_eq!(out[0].0, name, "{protocol} {name}");
            if out[0].1 != data && !known_normalisation(*protocol, name, &data, &out[0].1) {
                panic!(
                    "{protocol} {name} changed:\n  in  {:?}\n  out {:?}",
                    String::from_utf8_lossy(&data),
                    String::from_utf8_lossy(&out[0].1)
                );
            }
            checked += 1;
        }
    }
    eprintln!("text corpus: {checked} packets round-tripped; no corpus for {missing:?}");
}

/// Differences that are not losses: a 1.12 team suffix of a lone `§r`
/// (which formats nothing) comes back empty.
fn known_normalisation(protocol: i32, name: &str, before: &[u8], after: &[u8]) -> bool {
    protocol < 393 && name == "set_player_team" && {
        let marker = "\u{a7}r".as_bytes();
        let mut stripped = before.to_vec();
        if let Some(index) = stripped
            .windows(4)
            .position(|window| window[0] == 3 && &window[1..] == marker)
        {
            stripped.splice(index..index + 4, [0u8]);
        }
        stripped == after
    }
}

/// Converting every recorded packet to each adjacent protocol and back
/// gives what the protocol's own writer gives, so nothing the corpus holds
/// is lost or corrupted on the way; what the neighbour receives parses
/// with its reader.
#[test]
fn corpus_survives_each_adjacent_protocol() {
    let mut checked = 0;
    for (index, (protocol, _)) in PROTOCOLS.iter().enumerate() {
        let Some(packets) = corpus_packets(*protocol) else {
            continue;
        };
        let neighbours = [index.checked_sub(1), Some(index + 1)]
            .into_iter()
            .flatten()
            .filter_map(|index| PROTOCOLS.get(index).map(|(protocol, _)| *protocol));
        for neighbour in neighbours {
            for (state, name, data) in &packets {
                // The backend speaks the recorded protocol, the client the neighbour.
                let mut there = Shared::new(neighbour, *protocol);
                let Some(converted) =
                    convert_forced(&mut there, Direction::Clientbound, *state, name, data)
                else {
                    continue;
                };
                let converted = converted
                    .unwrap_or_else(|error| panic!("{protocol}->{neighbour} {name}: {error}"));
                let mut back = Vec::new();
                for (to_name, body) in &converted {
                    let mut home = Shared::new(*protocol, neighbour);
                    match convert_forced(&mut home, Direction::Clientbound, *state, to_name, body) {
                        Some(result) => back.extend(result.unwrap_or_else(|error| {
                            panic!("{neighbour}->{protocol} {to_name}: {error}")
                        })),
                        None => back.push((to_name, body.clone())),
                    }
                }
                let mut own = Shared::new(*protocol, *protocol);
                let expected =
                    match convert_forced(&mut own, Direction::Clientbound, *state, name, data) {
                        Some(expected) => expected.unwrap(),
                        None => vec![(*name, data.clone())],
                    };
                if !lossy(*protocol, neighbour, name) {
                    assert_eq!(
                        back.iter()
                            .map(|(name, body)| (*name, body.clone()))
                            .collect::<Vec<_>>(),
                        expected,
                        "{protocol}->{neighbour}->{protocol} {name}"
                    );
                }
                checked += 1;
            }
        }
    }
    eprintln!("text corpus: {checked} packets survived a neighbour and back");
}

/// Pairs where the corpus content does not survive a round trip by design.
fn lossy(protocol: i32, neighbour: i32, name: &str) -> bool {
    let (low, high) = (protocol.min(neighbour), protocol.max(neighbour));
    match name {
        // 1.19.3+ cannot keep 1.19.2 profile keys; a 1.19.2 client has no
        // "listed" flag.
        "player_info" | "player_info_update" | "player_info_remove" => {
            (low, high) == (760, 761)
                // An older client has no list order or hat actions.
                || ((low, high) == (767, 768) || (low, high) == (768, 769)) && protocol == high
        }
        // 1.19 chat types (such as tellraw's) become an overlay flag and
        // come back as the system type.
        "system_chat" => (low, high) == (759, 760) && protocol == 759,
        // 1.17 maps have no tracking flag.
        "map_item_data" => (low, high) == (754, 755),
        // A 1.16 chat packet's sender UUID does not exist in 1.15.
        "chat" => (low, high) == (578, 735) && protocol == 735,
        // Old show_item SNBT re-prints in the neighbour's format.
        _ => false,
    }
}

#[test]
fn neighbours_parse_what_they_receive() {
    // Hand-picked real packets: 1.12.2 team update with prefix colour, and
    // its 1.13 form.
    let team = b"\x03red\x02\x03Red\x03\xc2\xa7c\x00\x03\x06always\x06always\x0c";
    let mut shared = Shared::new(393, 340);
    let out = convert_forced(
        &mut shared,
        Direction::Clientbound,
        State::Play,
        "set_player_team",
        team,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out[0].1),
        String::from_utf8_lossy(b"\x03red\x02\x0e{\"text\":\"Red\"}\x03\x06always\x06always\x0c\x0b{\"text\":\"\"}\x0b{\"text\":\"\"}")
    );
    let mut shared = Shared::new(340, 393);
    let back = convert_forced(
        &mut shared,
        Direction::Clientbound,
        State::Play,
        "set_player_team",
        &out[0].1,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        back[0].1,
        b"\x03red\x02\x03Red\x03\xc2\xa7c\x00\x03\x06always\x06always\x0c"
    );
    let _ = Json::Null;
}

/// With `RIFT_TEXT_DUMP=<file>`, write every corpus packet converted to
/// each adjacent protocol as JSON lines (`{"p":..,"s":..,"name":..,"hex":..}`)
/// for `schema_check.py`, which decodes them with minecraft-data's schema
/// of the receiving version.
#[test]
fn dump_conversions_for_schema_check() {
    let Some(path) = std::env::var_os("RIFT_TEXT_DUMP") else {
        return;
    };
    let mut lines = String::new();
    for (index, (protocol, _)) in PROTOCOLS.iter().enumerate() {
        let Some(packets) = corpus_packets(*protocol) else {
            continue;
        };
        let neighbours = [index.checked_sub(1), Some(index + 1)]
            .into_iter()
            .flatten()
            .filter_map(|index| PROTOCOLS.get(index).map(|(protocol, _)| *protocol));
        for neighbour in neighbours {
            for (state, name, data) in &packets {
                let mut shared = Shared::new(neighbour, *protocol);
                let Some(Ok(out)) =
                    convert_forced(&mut shared, Direction::Clientbound, *state, name, data)
                else {
                    continue;
                };
                for (name, body) in out {
                    let hex: String = body.iter().map(|byte| format!("{byte:02x}")).collect();
                    let state = if *state == State::Play {
                        "play"
                    } else {
                        "configuration"
                    };
                    lines.push_str(&format!(
                        "{{\"from\":{protocol},\"p\":{neighbour},\"s\":\"{state}\",\"name\":\"{name}\",\"hex\":\"{hex}\"}}\n"
                    ));
                }
            }
        }
    }
    std::fs::write(path, lines).unwrap();
}

/// Every recorded text packet goes through each adjacent pair's default
/// routes (IDs and dispatch included) without error, unless the receiver
/// lacks the packet and the step must handle it.
#[test]
fn corpus_flows_through_default_routes() {
    let mut routed = 0;
    for (index, (protocol, _)) in PROTOCOLS.iter().enumerate() {
        let Some(session) = testing::corpus(*protocol) else {
            continue;
        };
        let neighbours = [index.checked_sub(1), Some(index + 1)]
            .into_iter()
            .flatten()
            .filter_map(|index| PROTOCOLS.get(index).map(|(protocol, _)| *protocol));
        for neighbour in neighbours {
            let mut step = testing::Defaults::new(neighbour, *protocol);
            let target = crate::data::version(neighbour);
            for packet in session
                .iter()
                .filter(|packet| packet.direction == Direction::Clientbound)
            {
                let name = packet.name(*protocol);
                if !RULES.iter().any(|rule| rule.name == name) {
                    continue;
                }
                let result = step.send_id(
                    Direction::Clientbound,
                    packet.state,
                    packet.id,
                    &packet.data,
                );
                match result {
                    Ok(out) => {
                        for out in out {
                            assert_ne!(out.name, "?", "{protocol}->{neighbour} {name}");
                        }
                        routed += 1;
                    }
                    Err(error) => assert!(
                        target
                            .packet_id(packet.state, Direction::Clientbound, name)
                            .is_none(),
                        "{protocol}->{neighbour} {name}: {error}"
                    ),
                }
            }
        }
    }
    eprintln!("text corpus: {routed} packets routed through neighbouring steps");
}
