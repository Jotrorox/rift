//! `commands`: the Brigadier command graph the client completes and
//! highlights commands with (1.13+).
//!
//! Argument nodes name their parser by identifier until 1.18.2 and by
//! `command_argument_type` registry ID from 1.19, followed by parser-specific
//! properties: number ranges, string modes, entity and score holder flags, a
//! minimum for `time` (1.19.4) and a registry for the resource parsers
//! (1.18.2, 1.19.3, 1.21.5). A parser the receiver lacks becomes a greedy
//! string that asks the server for suggestions: the client still lets the
//! player type the command, and the server validates it. 1.21.6 added a
//! flag for commands restricted to trusted sources.
use super::registry;
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    data,
    step::Cx,
    wire::{Reader, Writer, invalid},
};
use std::io;

/// Parsers are registry IDs from 1.19.
const NUMBERED: i32 = 759;
/// `minecraft:time` carries a minimum from 1.19.4.
const TIME_MINIMUM: i32 = 762;
/// The restricted flag exists from 1.21.6.
const RESTRICTED: i32 = 771;

const TYPE: u8 = 0x03;
const EXECUTABLE: u8 = 0x04;
const REDIRECT: u8 = 0x08;
const SUGGESTIONS: u8 = 0x10;
const RESTRICTED_FLAG: u8 = 0x20;

const ARGUMENT: u8 = 2;

const ASK_SERVER: &str = "minecraft:ask_server";
const GREEDY_PHRASE: i32 = 2;

/// The parsers of 1.13 to 1.18.2, which name them, with the protocols that
/// have each; from 1.19 the `command_argument_type` registry lists them.
const NAMED_PARSERS: &[(&str, i32, i32)] = &[
    ("brigadier:bool", 393, 758),
    ("brigadier:float", 393, 758),
    ("brigadier:double", 393, 758),
    ("brigadier:integer", 393, 758),
    ("brigadier:long", 498, 758),
    ("brigadier:string", 393, 758),
    ("minecraft:entity", 393, 758),
    ("minecraft:game_profile", 393, 758),
    ("minecraft:block_pos", 393, 758),
    ("minecraft:column_pos", 401, 758),
    ("minecraft:vec3", 393, 758),
    ("minecraft:vec2", 393, 758),
    ("minecraft:block_state", 393, 758),
    ("minecraft:block_predicate", 393, 758),
    ("minecraft:item_stack", 393, 758),
    ("minecraft:item_predicate", 393, 758),
    ("minecraft:color", 393, 758),
    ("minecraft:component", 393, 758),
    ("minecraft:message", 393, 758),
    ("minecraft:nbt", 393, 404),
    ("minecraft:nbt_compound_tag", 477, 758),
    ("minecraft:nbt_tag", 477, 758),
    ("minecraft:nbt_path", 393, 758),
    ("minecraft:objective", 393, 758),
    ("minecraft:objective_criteria", 393, 758),
    ("minecraft:operation", 393, 758),
    ("minecraft:particle", 393, 758),
    ("minecraft:angle", 751, 758),
    ("minecraft:rotation", 393, 758),
    ("minecraft:scoreboard_slot", 393, 758),
    ("minecraft:score_holder", 393, 758),
    ("minecraft:swizzle", 393, 758),
    ("minecraft:team", 393, 758),
    ("minecraft:item_slot", 393, 758),
    ("minecraft:resource_location", 393, 758),
    ("minecraft:mob_effect", 393, 758),
    ("minecraft:function", 393, 758),
    ("minecraft:entity_anchor", 393, 758),
    ("minecraft:int_range", 393, 758),
    ("minecraft:float_range", 393, 758),
    ("minecraft:item_enchantment", 393, 758),
    ("minecraft:entity_summon", 393, 758),
    ("minecraft:dimension", 401, 758),
    ("minecraft:time", 477, 758),
    ("minecraft:uuid", 735, 758),
    ("minecraft:resource", 758, 758),
    ("minecraft:resource_or_tag", 758, 758),
];

/// Parsers that kept their meaning under a new name: (older, newer, first
/// protocol with the newer name).
const RENAMED: &[(&str, &str, i32)] = &[
    ("minecraft:nbt", "minecraft:nbt_compound_tag", 477),
    ("minecraft:color", "minecraft:team_color", 776),
];

/// Parsers 1.19.3 replaced by `minecraft:resource` of a registry.
const RESOURCES: &[(&str, &str)] = &[
    ("minecraft:mob_effect", "minecraft:mob_effect"),
    ("minecraft:item_enchantment", "minecraft:enchantment"),
    ("minecraft:entity_summon", "minecraft:entity_type"),
];
const RESOURCE_PARSERS: i32 = 761;

fn full(name: &str) -> String {
    if name.contains(':') {
        name.to_owned()
    } else {
        format!("minecraft:{name}")
    }
}

/// Every parser of `protocol` by full identifier, in ID order from 1.19.
fn parsers(protocol: i32) -> Vec<String> {
    if protocol >= NUMBERED {
        return data::version(protocol)
            .expect_registry("command_argument_type")
            .names()
            .map(|(_, name)| full(name))
            .collect();
    }
    NAMED_PARSERS
        .iter()
        .filter(|(_, since, until)| (*since..=*until).contains(&protocol))
        .map(|(name, _, _)| (*name).to_owned())
        .collect()
}

fn has_parser(protocol: i32, name: &str) -> bool {
    if protocol >= NUMBERED {
        return parser_id(protocol, name).is_some();
    }
    NAMED_PARSERS
        .iter()
        .any(|(known, since, until)| *known == name && (*since..=*until).contains(&protocol))
}

fn parser_id(protocol: i32, name: &str) -> Option<i32> {
    data::version(protocol)
        .expect_registry("command_argument_type")
        .id(name)
}

/// A parser's properties in the protocol the node was read in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Properties {
    None,
    /// Brigadier numbers: flags (1 min, 2 max), then min and max in the
    /// parser's width, kept as bytes.
    Number {
        flags: u8,
        bounds: Vec<u8>,
    },
    /// Brigadier strings: 0 word, 1 quotable phrase, 2 greedy phrase.
    String(i32),
    /// `minecraft:entity` (1 single, 2 players only) and
    /// `minecraft:score_holder` (1 multiple) flags.
    Flags(u8),
    /// `minecraft:time` from 1.19.4: the minimum in ticks.
    Time(i32),
    /// The resource parsers: the registry they read from.
    Registry(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parser {
    /// The full identifier, such as `brigadier:integer`.
    pub name: String,
    pub properties: Properties,
}

/// One node. `flags` is kept as sent; `suggestions` only on argument nodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub flags: u8,
    pub children: Vec<i32>,
    pub redirect: Option<i32>,
    pub name: Option<String>,
    pub parser: Option<Parser>,
    pub suggestions: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commands {
    pub nodes: Vec<Node>,
    pub root: i32,
}

fn number_width(name: &str) -> Option<usize> {
    Some(match name {
        "brigadier:float" | "brigadier:integer" => 4,
        "brigadier:double" | "brigadier:long" => 8,
        _ => return None,
    })
}

fn read_properties(protocol: i32, name: &str, reader: &mut Reader<'_>) -> io::Result<Properties> {
    if let Some(width) = number_width(name) {
        let flags = reader.u8()?;
        if flags & !3 != 0 {
            return Err(invalid("unknown number range flags"));
        }
        let size = width * (usize::from(flags & 1) + usize::from(flags >> 1 & 1));
        return Ok(Properties::Number {
            flags,
            bounds: reader.take(size)?.to_vec(),
        });
    }
    Ok(match name {
        "brigadier:string" => {
            let mode = reader.varint()?;
            if !(0..=2).contains(&mode) {
                return Err(invalid("unknown string mode"));
            }
            Properties::String(mode)
        }
        "minecraft:entity" | "minecraft:score_holder" => {
            let flags = reader.u8()?;
            let known = if name == "minecraft:entity" { 3 } else { 1 };
            if flags & !known != 0 {
                return Err(invalid("unknown argument flags"));
            }
            Properties::Flags(flags)
        }
        "minecraft:time" if protocol >= TIME_MINIMUM => Properties::Time(reader.i32()?),
        "minecraft:resource"
        | "minecraft:resource_or_tag"
        | "minecraft:resource_key"
        | "minecraft:resource_or_tag_key"
        | "minecraft:resource_selector" => Properties::Registry(reader.str()?.to_owned()),
        _ => Properties::None,
    })
}

fn write_properties(properties: &Properties, writer: &mut Writer) {
    match properties {
        Properties::None => {}
        Properties::Number { flags, bounds } => {
            writer.u8(*flags).bytes(bounds);
        }
        Properties::String(mode) => {
            writer.varint(*mode);
        }
        Properties::Flags(flags) => {
            writer.u8(*flags);
        }
        Properties::Time(minimum) => {
            writer.i32(*minimum);
        }
        Properties::Registry(registry) => {
            writer.string(registry);
        }
    }
}

/// Parse a `commands` body of `protocol`, checking the graph's references.
pub fn read(protocol: i32, data: &[u8]) -> io::Result<Commands> {
    let mut reader = Reader::new(data);
    let count = reader.count()?;
    let known_flags = if protocol >= RESTRICTED { 0x3f } else { 0x1f };
    let mut nodes = Vec::with_capacity(count);
    for _ in 0..count {
        let flags = reader.u8()?;
        if flags & !known_flags != 0 || flags & TYPE == 3 {
            return Err(invalid("unknown command node flags"));
        }
        let children_count = reader.count()?;
        let mut children = Vec::with_capacity(children_count);
        for _ in 0..children_count {
            children.push(reader.varint()?);
        }
        let redirect = if flags & REDIRECT != 0 {
            Some(reader.varint()?)
        } else {
            None
        };
        let (mut name, mut parser, mut suggestions) = (None, None, None);
        if flags & TYPE != 0 {
            name = Some(reader.str()?.to_owned());
        }
        if flags & TYPE == ARGUMENT {
            let parser_name = if protocol >= NUMBERED {
                let id = reader.varint()?;
                full(
                    data::version(protocol)
                        .expect_registry("command_argument_type")
                        .name(id)
                        .ok_or_else(|| invalid("unknown argument parser"))?,
                )
            } else {
                let name = reader.str()?;
                if !has_parser(protocol, name) {
                    return Err(invalid("unknown argument parser"));
                }
                name.to_owned()
            };
            let properties = read_properties(protocol, &parser_name, &mut reader)?;
            parser = Some(Parser {
                name: parser_name,
                properties,
            });
            if flags & SUGGESTIONS != 0 {
                suggestions = Some(reader.str()?.to_owned());
            }
        }
        nodes.push(Node {
            flags,
            children,
            redirect,
            name,
            parser,
            suggestions,
        });
    }
    let root = reader.varint()?;
    reader.finish()?;
    let commands = Commands { nodes, root };
    validate(&commands)?;
    Ok(commands)
}

fn validate(commands: &Commands) -> io::Result<()> {
    let count = commands.nodes.len() as i32;
    let valid = |index: i32| (0..count).contains(&index);
    if !valid(commands.root) || commands.nodes[commands.root as usize].flags & TYPE != 0 {
        return Err(invalid("invalid command root"));
    }
    for node in &commands.nodes {
        if !node.children.iter().all(|child| valid(*child)) || !node.redirect.is_none_or(valid) {
            return Err(invalid("command node refers past the graph"));
        }
    }
    Ok(())
}

/// Serialize a graph whose parsers `protocol` has.
pub fn write(protocol: i32, commands: &Commands) -> io::Result<Vec<u8>> {
    let mut writer = Writer::new();
    writer.varint(commands.nodes.len() as i32);
    for node in &commands.nodes {
        writer.u8(node.flags).varint(node.children.len() as i32);
        for child in &node.children {
            writer.varint(*child);
        }
        if let Some(redirect) = node.redirect {
            writer.varint(redirect);
        }
        if let Some(name) = &node.name {
            writer.string(name);
        }
        if let Some(parser) = &node.parser {
            if protocol >= NUMBERED {
                let id = parser_id(protocol, &parser.name)
                    .ok_or_else(|| invalid("the receiver lacks an argument parser"))?;
                writer.varint(id);
            } else {
                writer.string(&parser.name);
            }
            write_properties(&parser.properties, &mut writer);
            if let Some(suggestions) = &node.suggestions {
                writer.string(suggestions);
            }
        }
    }
    writer.varint(commands.root);
    Ok(writer.into_inner())
}

/// Convert one parser; `None` when the receiver has no equivalent.
fn convert_parser(source: i32, target: i32, mut parser: Parser) -> Option<Parser> {
    for (older, newer, since) in RENAMED {
        if parser.name == *older && target >= *since {
            parser.name = (*newer).to_owned();
        } else if parser.name == *newer && target < *since {
            parser.name = (*older).to_owned();
        }
    }
    if target >= RESOURCE_PARSERS
        && let Some((_, registry)) = RESOURCES.iter().find(|(name, _)| *name == parser.name)
    {
        parser = Parser {
            name: "minecraft:resource".into(),
            properties: Properties::Registry((*registry).to_owned()),
        };
    } else if target < RESOURCE_PARSERS
        && parser.name == "minecraft:resource"
        && let Properties::Registry(registry) = &parser.properties
        && let Some((name, _)) = RESOURCES
            .iter()
            .find(|(_, known)| data::strip(known) == data::strip(registry))
    {
        parser = Parser {
            name: (*name).to_owned(),
            properties: Properties::None,
        };
    }
    if parser.name == "minecraft:time" {
        parser.properties = match (parser.properties, target >= TIME_MINIMUM) {
            (Properties::Time(minimum), true) => Properties::Time(minimum),
            (_, true) => Properties::Time(0),
            (_, false) => Properties::None,
        };
    }
    if !has_parser(target, &parser.name) {
        return None;
    }
    // A registry lookup the receiver cannot resolve fails its whole graph.
    if let Properties::Registry(registry) = &parser.properties
        && matches!(
            parser.name.as_str(),
            "minecraft:resource" | "minecraft:resource_or_tag"
        )
        && registry::registry(source, registry).is_some()
        && registry::registry(target, registry).is_none()
    {
        return None;
    }
    Some(parser)
}

/// Convert a graph between the sender's and the receiver's protocols.
pub fn convert(source: i32, target: i32, mut commands: Commands) -> Commands {
    for node in &mut commands.nodes {
        if target < RESTRICTED {
            node.flags &= !RESTRICTED_FLAG;
        }
        let Some(parser) = node.parser.take() else {
            continue;
        };
        node.parser = Some(match convert_parser(source, target, parser) {
            Some(parser) => parser,
            None => {
                node.flags |= SUGGESTIONS;
                node.suggestions = Some(ASK_SERVER.into());
                Parser {
                    name: "brigadier:string".into(),
                    properties: Properties::String(GREEDY_PHRASE),
                }
            }
        });
    }
    commands
}

/// A graph that accepts any command, for a client whose server sends none
/// (a 1.13 client on a 1.12 backend): one executable greedy argument under
/// the root, completed by the server.
pub fn catch_all(protocol: i32) -> io::Result<Vec<u8>> {
    let commands = Commands {
        nodes: vec![
            Node {
                flags: 0,
                children: vec![1],
                redirect: None,
                name: None,
                parser: None,
                suggestions: None,
            },
            Node {
                flags: ARGUMENT | EXECUTABLE | SUGGESTIONS,
                children: vec![],
                redirect: None,
                name: Some("command".into()),
                parser: Some(Parser {
                    name: "brigadier:string".into(),
                    properties: Properties::String(GREEDY_PHRASE),
                }),
                suggestions: Some(ASK_SERVER.into()),
            },
        ],
        root: 0,
    };
    write(protocol, &commands)
}

pub fn commands<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared: &mut Shared = step.shared();
    let direction = Direction::Clientbound;
    let (source, target) = (shared.source(direction), shared.target(direction));
    if target < 393 {
        // 1.12 has no command graph.
        return Ok(());
    }
    let commands = convert(source, target, read(source, &packet.data)?);
    cx.to_client(state, Packet::new(packet.id, write(target, &commands)?));
    Ok(())
}

/// Whether command graphs need converting from `source` to `target`.
pub fn needed(source: i32, target: i32) -> bool {
    let shape = |protocol: i32| {
        (
            protocol >= NUMBERED,
            protocol >= TIME_MINIMUM,
            protocol >= RESTRICTED,
            parsers(protocol),
        )
    };
    target < 393 || shape(source) != shape(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::testing::Defaults;

    fn argument(name: &str, parser: &str, properties: Properties) -> Node {
        Node {
            flags: ARGUMENT,
            children: vec![],
            redirect: None,
            name: Some(name.into()),
            parser: Some(Parser {
                name: parser.into(),
                properties,
            }),
            suggestions: None,
        }
    }

    fn graph(arguments: Vec<Node>) -> Commands {
        let mut nodes = vec![Node {
            flags: 0,
            children: (1..=arguments.len() as i32).collect(),
            redirect: None,
            name: None,
            parser: None,
            suggestions: None,
        }];
        nodes.extend(arguments);
        Commands { nodes, root: 0 }
    }

    #[test]
    fn named_parsers_match_the_first_numbered_registry() {
        let mut named = parsers(758);
        named.extend([
            "minecraft:template_mirror".into(),
            "minecraft:template_rotation".into(),
        ]);
        named.sort();
        let mut numbered = parsers(759);
        numbered.sort();
        assert_eq!(named, numbered);
    }

    #[test]
    fn layouts_round_trip_and_malformed_graphs_fail() {
        let commands = graph(vec![
            argument(
                "n",
                "brigadier:integer",
                Properties::Number {
                    flags: 3,
                    bounds: vec![0, 0, 0, 1, 0, 0, 0, 9],
                },
            ),
            argument("s", "brigadier:string", Properties::String(1)),
            argument("e", "minecraft:entity", Properties::Flags(2)),
            argument(
                "r",
                "minecraft:resource",
                Properties::Registry("minecraft:mob_effect".into()),
            ),
            argument("t", "minecraft:time", Properties::Time(5)),
        ]);
        for protocol in [762, 777] {
            let bytes = write(protocol, &commands).unwrap();
            assert_eq!(read(protocol, &bytes).unwrap(), commands);
        }
        let old = graph(vec![argument(
            "n",
            "brigadier:double",
            Properties::Number {
                flags: 1,
                bounds: 1.5f64.to_be_bytes().to_vec(),
            },
        )]);
        let bytes = write(393, &old).unwrap();
        assert_eq!(read(393, &bytes).unwrap(), old);
        // A child index past the graph, a bad root, unknown flags and
        // unknown parsers all fail.
        let mut broken = old.clone();
        broken.nodes[0].children.push(7);
        assert!(read(393, &write(393, &broken).unwrap()).is_err());
        let mut broken = old.clone();
        broken.root = 1;
        assert!(read(393, &write(393, &broken).unwrap()).is_err());
        let mut broken = old.clone();
        broken.nodes[1].flags |= RESTRICTED_FLAG;
        assert!(read(770, &write(770, &broken).unwrap()).is_err());
        assert!(read(771, &write(771, &broken).unwrap()).is_ok());
        let unknown = graph(vec![argument("x", "minecraft:uuid", Properties::None)]);
        assert!(read(404, &write(404, &unknown).unwrap()).is_err());
        assert!(read(393, &[1, 3, 0, 0]).is_err());
        assert!(read(393, &[0xff, 0xff, 0xff, 0xff, 0x0f]).is_err());
    }

    #[test]
    fn parsers_convert_across_their_changes() {
        // 1.19.3 turned three parsers into registry resources.
        let effect = graph(vec![argument(
            "e",
            "minecraft:mob_effect",
            Properties::None,
        )]);
        let up = convert(760, 761, effect.clone());
        assert_eq!(
            up.nodes[1].parser.as_ref().unwrap().properties,
            Properties::Registry("minecraft:mob_effect".into())
        );
        assert_eq!(convert(761, 760, up), effect);
        // 1.19.4 gave time a minimum.
        let time = graph(vec![argument("t", "minecraft:time", Properties::None)]);
        let up = convert(761, 762, time.clone());
        assert_eq!(
            up.nodes[1].parser.as_ref().unwrap().properties,
            Properties::Time(0)
        );
        assert_eq!(convert(762, 761, up), time);
        // Renamed parsers keep their meaning.
        let nbt = graph(vec![argument("n", "minecraft:nbt", Properties::None)]);
        assert_eq!(
            convert(404, 477, nbt).nodes[1]
                .parser
                .as_ref()
                .unwrap()
                .name,
            "minecraft:nbt_compound_tag"
        );
        let color = graph(vec![argument(
            "c",
            "minecraft:team_color",
            Properties::None,
        )]);
        assert_eq!(
            convert(776, 775, color).nodes[1]
                .parser
                .as_ref()
                .unwrap()
                .name,
            "minecraft:color"
        );
        // A parser the receiver lacks becomes a server-completed greedy string.
        let mut uuid = graph(vec![argument("u", "minecraft:uuid", Properties::None)]);
        uuid.nodes[1].children = vec![0];
        let down = convert(735, 578, uuid);
        assert_eq!(
            down.nodes[1].parser.as_ref().unwrap(),
            &Parser {
                name: "brigadier:string".into(),
                properties: Properties::String(GREEDY_PHRASE),
            }
        );
        assert_eq!(down.nodes[1].suggestions.as_deref(), Some(ASK_SERVER));
        assert_eq!(down.nodes[1].flags, ARGUMENT | SUGGESTIONS);
        assert_eq!(down.nodes[1].children, vec![0]);
        assert!(write(578, &down).is_ok());
        // The restricted flag disappears for older clients.
        let mut restricted = graph(vec![]);
        restricted.nodes[0].flags = RESTRICTED_FLAG;
        assert_eq!(convert(771, 770, restricted).nodes[0].flags, 0);
    }

    #[test]
    fn the_catch_all_graph_parses_and_1_12_gets_no_graph() {
        let commands = read(393, &catch_all(393).unwrap()).unwrap();
        assert_eq!(commands.nodes.len(), 2);
        assert!(read(777, &catch_all(777).unwrap()).is_ok());
        let mut step = Defaults::new(340, 393);
        assert!(
            step.clientbound(State::Play, "commands", &catch_all(393).unwrap())
                .unwrap()
                .is_empty()
        );
        assert!(!needed(480, 477));
        // 1.14.4 added brigadier:long.
        assert!(needed(498, 490));
        assert!(needed(758, 759));
    }

    /// Every recorded graph round-trips, and converts to both neighbours into
    /// graphs that parse there with the same shape and names.
    #[test]
    fn corpus_graphs_round_trip_and_convert() {
        use crate::version::PROTOCOLS;
        let (mut checked, mut converted, mut fallbacks) = (0, 0, 0);
        for (index, &(protocol, _)) in PROTOCOLS.iter().enumerate() {
            let packets: Vec<_> = crate::codec::misc::sessions(protocol)
                .into_iter()
                .flatten()
                .collect();
            for packet in packets
                .iter()
                .filter(|packet| packet.name(protocol) == "commands")
            {
                let commands = read(protocol, &packet.data).unwrap();
                assert_eq!(
                    write(protocol, &commands).unwrap(),
                    packet.data,
                    "{protocol}"
                );
                checked += 1;
                for neighbour in [index.checked_sub(1), Some(index + 1)]
                    .into_iter()
                    .flatten()
                {
                    let Some(&(other, _)) = PROTOCOLS.get(neighbour) else {
                        continue;
                    };
                    if other < 393 {
                        continue;
                    }
                    let mut step = Defaults::new(other, protocol);
                    let out = step
                        .send_id(
                            Direction::Clientbound,
                            packet.state,
                            packet.id,
                            &packet.data,
                        )
                        .unwrap();
                    let data = out.first().map_or(&packet.data, |out| &out.data);
                    let back = read(other, data).unwrap();
                    assert_eq!(back.nodes.len(), commands.nodes.len());
                    for (before, after) in commands.nodes.iter().zip(&back.nodes) {
                        assert_eq!(
                            (&before.name, &before.children, before.redirect),
                            (&after.name, &after.children, after.redirect)
                        );
                        if before.parser != after.parser
                            && after.suggestions.as_deref() == Some(ASK_SERVER)
                        {
                            fallbacks += 1;
                        }
                    }
                    converted += 1;
                }
            }
        }
        eprintln!(
            "{checked} recorded graphs round-tripped, {converted} converted, {fallbacks} parsers fell back"
        );
    }
}
