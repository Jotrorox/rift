//! Small container packets that carry registry IDs: `container_set_data`
//! (the enchanting table's enchantment hints, the beacon's effects) and the
//! serverbound `set_beacon` (1.13+).
//!
//! A data property means something only for the menu it belongs to, so the
//! codec follows `open_screen` to know which window is an enchanting table
//! or a beacon. 1.12 packs an enchantment hint's ID and level into one
//! property (`id | level << 8`); 1.13 sends the level as its own property.
//! Beacon effects are mob effect IDs, with -1 for none until 1.20.4 and
//! ID + 1 with 0 for none from 1.20.5. The window ID grew from a byte to a
//! VarInt in 1.21.2.
use super::registry;
use crate::{
    Direction, Packet, State as Phase,
    codec::{HasShared, Shared},
    step::Cx,
    wire::{Reader, Writer, invalid},
};
use std::io;

/// Enchantment hints are split into ID and level properties from 1.13.
const SPLIT_HINTS: i32 = 393;
/// Beacon effects are encoded as ID + 1 from 1.20.5.
const SHIFTED_EFFECTS: i32 = 766;
/// Window IDs are VarInts from 1.21.2.
const WIDE_WINDOWS: i32 = 768;
/// `set_beacon` sends optional effects from 1.19.
const OPTIONAL_EFFECTS: i32 = 759;

/// The kind of the window last opened, as far as its data properties matter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Menu {
    #[default]
    Other,
    Enchantment,
    Beacon,
}

/// What the codec follows of the open window.
#[derive(Clone, Copy, Debug, Default)]
pub struct Window {
    id: i32,
    menu: Menu,
    /// 1.12's enchantment hints, combined from 1.13's ID and level
    /// properties: (ID, level) per hint, -1 when unknown.
    hints: [(i32, i32); 3],
}

/// Follow `open_screen` (in the sender's protocol) to know the open menu.
pub fn track_open_screen<S: HasShared>(step: &mut S, _: Phase, packet: &Packet) -> io::Result<()> {
    let shared = step.shared();
    let source = shared.source(Direction::Clientbound);
    let mut reader = Reader::new(&packet.data);
    let (id, menu) = if source >= 477 {
        let id = reader.varint()?;
        let menu = reader.varint()?;
        let name = registry::registry(source, "menu").and_then(|(_, menus)| menus.name(menu));
        let menu = match name {
            Some("enchantment") => Menu::Enchantment,
            Some("beacon") => Menu::Beacon,
            _ => Menu::Other,
        };
        (id, menu)
    } else {
        let id = i32::from(reader.u8()?);
        let menu = match reader.string(32)? {
            "minecraft:enchanting_table" => Menu::Enchantment,
            "minecraft:beacon" => Menu::Beacon,
            _ => Menu::Other,
        };
        (id, menu)
    };
    shared.misc.window = Window {
        id,
        menu,
        hints: [(-1, -1); 3],
    };
    Ok(())
}

fn read_window(protocol: i32, reader: &mut Reader<'_>) -> io::Result<i32> {
    if protocol >= WIDE_WINDOWS {
        reader.varint()
    } else {
        Ok(i32::from(reader.u8()?))
    }
}

fn write_window(protocol: i32, window: i32, writer: &mut Writer) -> io::Result<()> {
    if protocol >= WIDE_WINDOWS {
        writer.varint(window);
    } else {
        writer.u8(u8::try_from(window).map_err(|_| invalid("window ID out of range"))?);
    }
    Ok(())
}

/// A mob effect ID from a beacon property or `set_beacon`, `None` for none.
fn decode_effect(protocol: i32, value: i32) -> Option<i32> {
    if protocol >= SHIFTED_EFFECTS {
        (value > 0).then(|| value - 1)
    } else {
        (value >= 0).then_some(value)
    }
}

fn encode_effect(protocol: i32, effect: Option<i32>) -> i32 {
    match effect {
        Some(id) if protocol >= SHIFTED_EFFECTS => id + 1,
        Some(id) => id,
        None if protocol >= SHIFTED_EFFECTS => 0,
        None => -1,
    }
}

fn map(shared: &mut Shared, direction: Direction, registry: &str, id: Option<i32>) -> Option<i32> {
    let mapping = registry::mapping(shared, direction, registry)?;
    mapping.get(id?)
}

/// Convert one data property: the properties to send instead, in the
/// receiver's terms.
pub fn convert_property(
    shared: &mut Shared,
    direction: Direction,
    window: i32,
    property: i16,
    value: i16,
) -> Vec<(i16, i16)> {
    let (source, target) = (shared.source(direction), shared.target(direction));
    let open = shared.misc.window;
    let menu = if open.id == window {
        open.menu
    } else {
        Menu::Other
    };
    let value = i32::from(value);
    match (menu, property) {
        (Menu::Enchantment, 4..=6) => {
            let slot = (property - 4) as usize;
            let (id, level) = if source < SPLIT_HINTS {
                if value < 0 {
                    (None, -1)
                } else {
                    (Some(value & 0xff), value >> 8)
                }
            } else {
                ((value >= 0).then_some(value), -1)
            };
            let id = map(shared, direction, "enchantment", id).unwrap_or(-1);
            if target < SPLIT_HINTS {
                let hint = &mut shared.misc.window.hints[slot];
                hint.0 = id;
                if source < SPLIT_HINTS {
                    hint.1 = level;
                }
                return vec![(property, legacy_hint(*hint))];
            }
            let mut properties = vec![(property, id as i16)];
            if source < SPLIT_HINTS {
                properties.push((property + 3, if id < 0 { -1 } else { level as i16 }));
            }
            properties
        }
        (Menu::Enchantment, 7..=9) if source >= SPLIT_HINTS && target < SPLIT_HINTS => {
            let hint = &mut shared.misc.window.hints[(property - 7) as usize];
            hint.1 = value;
            vec![(property - 3, legacy_hint(*hint))]
        }
        (Menu::Beacon, 1..=2) => {
            let effect = map(
                shared,
                direction,
                "mob_effect",
                decode_effect(source, value),
            );
            vec![(property, encode_effect(target, effect) as i16)]
        }
        _ => vec![(property, value as i16)],
    }
}

/// 1.12's packed hint: the ID and the level, or -1 without an ID.
fn legacy_hint((id, level): (i32, i32)) -> i16 {
    if id < 0 {
        -1
    } else {
        (id | level.max(0) << 8) as i16
    }
}

pub fn container_set_data<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: Phase,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let (source, target) = (shared.source(direction), shared.target(direction));
    let mut reader = Reader::new(&packet.data);
    let window = read_window(source, &mut reader)?;
    let property = reader.i16()?;
    let value = reader.i16()?;
    reader.finish()?;
    for (property, value) in convert_property(shared, direction, window, property, value) {
        let mut writer = Writer::new();
        write_window(target, window, &mut writer)?;
        writer.i16(property).i16(value);
        cx.to_client(state, Packet::new(packet.id, writer.into_inner()));
    }
    Ok(())
}

fn read_effect(protocol: i32, reader: &mut Reader<'_>) -> io::Result<Option<i32>> {
    if protocol >= OPTIONAL_EFFECTS {
        if reader.bool()? {
            Ok(Some(reader.varint()?))
        } else {
            Ok(None)
        }
    } else {
        Ok(decode_effect(protocol, reader.varint()?))
    }
}

fn write_effect(protocol: i32, effect: Option<i32>, writer: &mut Writer) {
    if protocol >= OPTIONAL_EFFECTS {
        writer.bool(effect.is_some());
        if let Some(effect) = effect {
            writer.varint(effect);
        }
    } else {
        writer.varint(effect.unwrap_or(-1));
    }
}

/// Convert a `set_beacon` body; for 1.12 the `MC|Beacon` plugin message it
/// replaced (channel included), with the packet name to send.
pub fn convert_set_beacon(
    shared: &mut Shared,
    direction: Direction,
    data: &[u8],
) -> io::Result<(&'static str, Vec<u8>)> {
    let (source, target) = (shared.source(direction), shared.target(direction));
    let mut reader = Reader::new(data);
    let primary = read_effect(source, &mut reader)?;
    let secondary = read_effect(source, &mut reader)?;
    reader.finish()?;
    let primary = map(shared, direction, "mob_effect", primary);
    let secondary = map(shared, direction, "mob_effect", secondary);
    let mut writer = Writer::new();
    if target < 393 {
        writer.string("MC|Beacon");
        writer
            .i32(primary.unwrap_or(-1))
            .i32(secondary.unwrap_or(-1));
        return Ok(("custom_payload", writer.into_inner()));
    }
    write_effect(target, primary, &mut writer);
    write_effect(target, secondary, &mut writer);
    Ok(("set_beacon", writer.into_inner()))
}

pub fn set_beacon<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: Phase,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Serverbound;
    let (name, body) = convert_set_beacon(shared, direction, &packet.data)?;
    let id = shared
        .target_data(direction)
        .expect_packet(state, direction, name);
    cx.to_backend(state, Packet::new(id, body));
    Ok(())
}

/// Whether `container_set_data` needs converting from `source` to `target`.
pub fn data_needed(source: i32, target: i32) -> bool {
    (source >= WIDE_WINDOWS) != (target >= WIDE_WINDOWS)
        || (source >= SHIFTED_EFFECTS) != (target >= SHIFTED_EFFECTS)
        || (source >= SPLIT_HINTS) != (target >= SPLIT_HINTS)
        || !registry::unchanged(source, target, "enchantment")
        || !registry::unchanged(source, target, "mob_effect")
}

/// Whether `set_beacon` (from the client's `source`) needs converting.
pub fn beacon_needed(source: i32, target: i32) -> bool {
    target < 393
        || (source >= OPTIONAL_EFFECTS) != (target >= OPTIONAL_EFFECTS)
        || (source >= SHIFTED_EFFECTS) != (target >= SHIFTED_EFFECTS)
        || !registry::unchanged(source, target, "mob_effect")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::testing::Defaults;
    use crate::data;

    fn open(protocol: i32, menu: &str) -> Vec<u8> {
        let mut writer = Writer::new();
        if protocol >= 477 {
            let id = data::version(protocol)
                .expect_registry("menu")
                .id(menu)
                .unwrap();
            writer.varint(1).varint(id);
        } else {
            writer.u8(1).string(menu);
        }
        writer.string(r#"{"text":""}"#);
        writer.into_inner()
    }

    fn property(protocol: i32, property: i16, value: i16) -> Vec<u8> {
        let mut writer = Writer::new();
        write_window(protocol, 1, &mut writer).unwrap();
        writer.i16(property).i16(value);
        writer.into_inner()
    }

    fn effect(protocol: i32, name: &str) -> i32 {
        data::version(protocol)
            .expect_registry("mob_effect")
            .id(name)
            .unwrap()
    }

    fn enchantment(protocol: i32, name: &str) -> i32 {
        registry::registry(protocol, "enchantment")
            .unwrap()
            .1
            .id(name)
            .unwrap()
    }

    #[test]
    fn beacon_effects_cross_the_encoding_change() {
        let mut step = Defaults::new(766, 765);
        let packet = Packet::new(0, open(765, "beacon"));
        track_open_screen(&mut step, Phase::Play, &packet).unwrap();
        let speed = effect(765, "speed") as i16;
        let out = step
            .clientbound(Phase::Play, "container_set_data", &property(765, 1, speed))
            .unwrap();
        assert_eq!(
            out[0].data,
            property(766, 1, effect(766, "speed") as i16 + 1)
        );
        let out = step
            .clientbound(Phase::Play, "container_set_data", &property(765, 2, -1))
            .unwrap();
        assert_eq!(out[0].data, property(766, 2, 0));
        // Other windows' properties pass unchanged.
        let out = step
            .clientbound(Phase::Play, "container_set_data", &property(765, 0, 3))
            .unwrap();
        assert_eq!(out[0].data, property(766, 0, 3));
    }

    #[test]
    fn enchantment_hints_split_and_combine_across_the_flattening() {
        let sharpness = (enchantment(340, "sharpness"), enchantment(393, "sharpness"));
        let mut up = Defaults::new(393, 340);
        track_open_screen(
            &mut up,
            Phase::Play,
            &Packet::new(0, open(340, "minecraft:enchanting_table")),
        )
        .unwrap();
        let out = up
            .clientbound(
                Phase::Play,
                "container_set_data",
                &property(340, 4, (sharpness.0 | 3 << 8) as i16),
            )
            .unwrap();
        let data: Vec<Vec<u8>> = out.into_iter().map(|out| out.data).collect();
        assert_eq!(
            data,
            vec![property(393, 4, sharpness.1 as i16), property(393, 7, 3)]
        );
        let mut down = Defaults::new(340, 393);
        track_open_screen(
            &mut down,
            Phase::Play,
            &Packet::new(0, open(393, "minecraft:enchanting_table")),
        )
        .unwrap();
        let first = down
            .clientbound(Phase::Play, "container_set_data", &data[0])
            .unwrap();
        assert_eq!(first[0].data, property(340, 4, sharpness.0 as i16));
        let second = down
            .clientbound(Phase::Play, "container_set_data", &data[1])
            .unwrap();
        assert_eq!(
            second[0].data,
            property(340, 4, (sharpness.0 | 3 << 8) as i16)
        );
    }

    #[test]
    fn window_ids_widen_in_1_21_2() {
        let mut up = Defaults::new(768, 767);
        let out = up
            .clientbound(Phase::Play, "container_set_data", &property(767, 0, 5))
            .unwrap();
        assert_eq!(out[0].data, property(768, 0, 5));
        let mut down = Defaults::new(767, 768);
        let mut wide = Writer::new();
        wide.varint(300).i16(0).i16(0);
        assert!(
            down.clientbound(Phase::Play, "container_set_data", &wide.buf)
                .is_err()
        );
        assert!(
            down.clientbound(Phase::Play, "container_set_data", &[1, 0])
                .is_err()
        );
    }

    #[test]
    fn set_beacon_converts_effects_and_becomes_a_plugin_message_for_1_12() {
        let mut writer = Writer::new();
        write_effect(766, Some(effect(766, "haste")), &mut writer);
        write_effect(766, None, &mut writer);
        let mut down = Defaults::new(766, 765);
        let out = down
            .serverbound(Phase::Play, "set_beacon", &writer.buf)
            .unwrap();
        let mut expected = Writer::new();
        write_effect(765, Some(effect(765, "haste")), &mut expected);
        write_effect(765, None, &mut expected);
        assert_eq!(out[0].data, expected.buf);
        let mut legacy = Defaults::new(393, 340);
        let mut body = Writer::new();
        body.varint(effect(393, "speed")).varint(-1);
        let out = legacy
            .serverbound(Phase::Play, "set_beacon", &body.buf)
            .unwrap();
        assert_eq!(out[0].name, "custom_payload");
        let mut reader = Reader::new(&out[0].data);
        assert_eq!(reader.str().unwrap(), "MC|Beacon");
        assert_eq!(
            (reader.i32().unwrap(), reader.i32().unwrap()),
            (effect(340, "speed"), -1)
        );
        assert!(beacon_needed(393, 340));
        assert!(!data_needed(498, 490));
    }
}
