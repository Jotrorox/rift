//! `open_screen` and the horse inventory's `mount_screen_open`.
//!
//! Until 1.13.2 a container window is named by a string (`minecraft:chest`)
//! with its slot count, and a horse inventory is the type `EntityHorse`
//! followed by the horse's entity ID. 1.14 numbers menus in the `menu`
//! registry (one generic menu per chest size) and gives horses their own
//! packet. Titles are text components, converted by the text codec.
//! `mount_screen_open`'s window ID grew from a byte to a VarInt in 1.21.2.
use super::registry;
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared, text},
    data,
    step::Cx,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::io;

/// Menus are registry IDs from 1.14.
const MENUS: i32 = 477;
/// `mount_screen_open` has a VarInt window ID from 1.21.2.
const WIDE_MOUNT: i32 = 768;
/// Titles are NBT from 1.20.3.
const NBT_TITLES: i32 = 765;

/// Slots each menu's own container has, beside the player's inventory.
fn menu_slots(menu: &str) -> Option<i32> {
    if let Some(rows) = menu.strip_prefix("generic_9x") {
        return rows.parse::<i32>().ok().map(|rows| rows * 9);
    }
    Some(match menu {
        "generic_3x3" => 9,
        "crafter_3x3" | "crafting" => 10,
        "anvil" | "blast_furnace" | "furnace" | "grindstone" | "merchant" | "smoker"
        | "cartography_table" | "smithing" => 3,
        "beacon" | "lectern" => 1,
        "brewing_stand" | "hopper" => 5,
        "enchantment" | "stonecutter" => 2,
        "loom" => 4,
        "shulker_box" => 27,
        _ => return None,
    })
}

/// The generic chest menu that holds at least `slots` slots.
fn generic(slots: i32) -> String {
    let rows = ((slots + 8) / 9).clamp(1, 6);
    format!("generic_9x{rows}")
}

/// A menu for a receiver from 1.14 on that lacks `menu`: the closest one
/// with the same slots.
fn stand_in(menu: &str) -> String {
    match menu {
        "crafter_3x3" => "generic_3x3".into(),
        "smithing" => "anvil".into(),
        "blast_furnace" | "smoker" => "furnace".into(),
        _ => generic(menu_slots(menu).unwrap_or(27)),
    }
}

/// 1.13's window type and slot count for a menu. Menus 1.13 lacks become
/// the window with the same number of slots.
fn legacy_window(menu: &str) -> (&'static str, i32) {
    match menu {
        "generic_3x3" | "crafter_3x3" => ("minecraft:dispenser", 9),
        "crafting" => ("minecraft:crafting_table", 0),
        "enchantment" | "stonecutter" => ("minecraft:enchanting_table", 0),
        "anvil" | "smithing" => ("minecraft:anvil", 0),
        "furnace" | "blast_furnace" | "smoker" | "grindstone" | "cartography_table" => {
            ("minecraft:furnace", 3)
        }
        "brewing_stand" => ("minecraft:brewing_stand", 5),
        "merchant" => ("minecraft:villager", 3),
        "beacon" | "lectern" => ("minecraft:beacon", 1),
        "hopper" | "loom" => ("minecraft:hopper", 5),
        "shulker_box" => ("minecraft:shulker_box", 27),
        menu => ("minecraft:chest", menu_slots(menu).unwrap_or(27)),
    }
}

/// The 1.14 menu for a 1.13 window type and slot count.
fn modern_menu(window: &str, slots: i32) -> String {
    match window {
        "minecraft:dispenser" | "minecraft:dropper" => "generic_3x3",
        "minecraft:crafting_table" => "crafting",
        "minecraft:furnace" => "furnace",
        "minecraft:enchanting_table" => "enchantment",
        "minecraft:brewing_stand" => "brewing_stand",
        "minecraft:villager" => "merchant",
        "minecraft:beacon" => "beacon",
        "minecraft:anvil" => "anvil",
        "minecraft:hopper" => "hopper",
        "minecraft:shulker_box" => "shulker_box",
        _ => return generic(slots),
    }
    .into()
}

const HORSE: &str = "EntityHorse";

/// A title component converter: the text codec in production.
type Title = fn(&mut Shared, Direction, &mut Rewriter<'_>) -> io::Result<()>;

/// Copy a title from `input`, converting it with `title`, into `writer`.
fn copy_title(
    shared: &mut Shared,
    direction: Direction,
    title: Title,
    reader: &mut Reader<'_>,
    writer: &mut Writer,
) -> io::Result<()> {
    let mut rewriter = Rewriter::new(reader.peek());
    title(shared, direction, &mut rewriter)?;
    let consumed = reader.remaining() - rewriter.remaining();
    reader.take(consumed)?;
    writer.bytes(&rewriter.w.buf);
    Ok(())
}

/// A packet to send instead of the one received.
pub struct Converted {
    pub name: &'static str,
    pub data: Vec<u8>,
}

/// Convert an `open_screen` body between the sender's and receiver's
/// protocols. Before 1.14 a horse window opens with `open_screen`; from
/// 1.14 with `mount_screen_open`, so the packet may change.
pub fn convert_open_screen(
    shared: &mut Shared,
    direction: Direction,
    data: &[u8],
    title: Title,
) -> io::Result<Converted> {
    let (source, target) = (shared.source(direction), shared.target(direction));
    let mut reader = Reader::new(data);
    let mut writer = Writer::new();
    if source < MENUS {
        let window = reader.u8()?;
        let kind = reader.string(32)?.to_owned();
        let mut title_bytes = Writer::new();
        copy_title(shared, direction, title, &mut reader, &mut title_bytes)?;
        let slots = i32::from(reader.u8()?);
        let entity = if kind == HORSE {
            Some(reader.i32()?)
        } else {
            None
        };
        reader.finish()?;
        if target < MENUS {
            writer
                .u8(window)
                .string(&kind)
                .bytes(&title_bytes.buf)
                .u8(slots as u8);
            if let Some(entity) = entity {
                writer.i32(entity);
            }
            return Ok(Converted {
                name: "open_screen",
                data: writer.into_inner(),
            });
        }
        if let Some(entity) = entity {
            writer.u8(window).varint(slots).i32(entity);
            return Ok(Converted {
                name: "mount_screen_open",
                data: writer.into_inner(),
            });
        }
        let menu = modern_menu(&kind, slots);
        let id = menu_id(target, &menu)?;
        writer
            .varint(i32::from(window))
            .varint(id)
            .bytes(&title_bytes.buf);
        return Ok(Converted {
            name: "open_screen",
            data: writer.into_inner(),
        });
    }
    let window = reader.varint()?;
    let menu = reader.varint()?;
    let (_, menus) = registry::registry(source, "menu").ok_or_else(|| invalid("no menus"))?;
    let menu = menus
        .name(menu)
        .ok_or_else(|| invalid("unknown menu"))?
        .to_owned();
    let mut title_bytes = Writer::new();
    copy_title(shared, direction, title, &mut reader, &mut title_bytes)?;
    reader.finish()?;
    if target < MENUS {
        let window =
            u8::try_from(window).map_err(|_| invalid("window ID out of range for 1.13"))?;
        let (kind, slots) = legacy_window(&menu);
        writer
            .u8(window)
            .string(kind)
            .bytes(&title_bytes.buf)
            .u8(slots as u8);
    } else {
        let id = match registry::mapping(shared, direction, "menu").and_then(|mapping| {
            let source_id = menus.id(&menu)?;
            mapping.get(source_id)
        }) {
            Some(id) => id,
            None => menu_id(target, &stand_in(&menu))?,
        };
        writer.varint(window).varint(id).bytes(&title_bytes.buf);
    }
    Ok(Converted {
        name: "open_screen",
        data: writer.into_inner(),
    })
}

fn menu_id(protocol: i32, menu: &str) -> io::Result<i32> {
    data::version(protocol)
        .expect_registry("menu")
        .id(menu)
        .ok_or_else(|| invalid("the receiver lacks the menu"))
}

/// An empty title for windows that gain one: 1.13's horse window.
fn empty_title(protocol: i32, writer: &mut Writer) {
    if protocol >= NBT_TITLES {
        crate::nbt::Nbt::String(String::new()).write(writer, false);
    } else {
        writer.string(r#"{"text":""}"#);
    }
}

/// Convert a `mount_screen_open` body (1.14+): the window ID's width, or
/// for 1.13 an `open_screen` of an `EntityHorse` window.
pub fn convert_mount_screen(source: i32, target: i32, data: &[u8]) -> io::Result<Converted> {
    let mut reader = Reader::new(data);
    let window = if source >= WIDE_MOUNT {
        reader.varint()?
    } else {
        i32::from(reader.u8()?)
    };
    let slots = reader.varint()?;
    let entity = reader.i32()?;
    reader.finish()?;
    let mut writer = Writer::new();
    if target < MENUS {
        let window =
            u8::try_from(window).map_err(|_| invalid("window ID out of range for 1.13"))?;
        let slots = u8::try_from(slots).map_err(|_| invalid("slot count out of range for 1.13"))?;
        writer.u8(window).string(HORSE);
        empty_title(target, &mut writer);
        writer.u8(slots).i32(entity);
        return Ok(Converted {
            name: "open_screen",
            data: writer.into_inner(),
        });
    }
    if target >= WIDE_MOUNT {
        writer.varint(window);
    } else {
        writer.u8(u8::try_from(window).map_err(|_| invalid("window ID out of range"))?);
    }
    writer.varint(slots).i32(entity);
    Ok(Converted {
        name: "mount_screen_open",
        data: writer.into_inner(),
    })
}

fn emit(shared: &Shared, cx: &mut Cx<'_>, state: State, converted: Converted) -> io::Result<()> {
    let direction = Direction::Clientbound;
    let id = shared
        .target_data(direction)
        .packet_id(state, direction, converted.name)
        .ok_or_else(|| invalid("the receiver lacks the screen packet"))?;
    cx.to_client(state, Packet::new(id, converted.data));
    Ok(())
}

pub fn open_screen<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let converted = convert_open_screen(
        shared,
        Direction::Clientbound,
        &packet.data,
        text::transcode,
    )?;
    emit(shared, cx, state, converted)
}

pub fn mount_screen_open<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let converted = convert_mount_screen(
        shared.source(direction),
        shared.target(direction),
        &packet.data,
    )?;
    emit(shared, cx, state, converted)
}

/// INTEGRATION: whether the text codec changes titles between the two
/// protocols. Until it says, assume it does where the encoding changes
/// (JSON to NBT in 1.20.3).
fn titles_change(source: i32, target: i32) -> bool {
    (source >= NBT_TITLES) != (target >= NBT_TITLES)
}

/// Whether `open_screen` needs converting from `source` to `target`.
pub fn open_needed(source: i32, target: i32) -> bool {
    (source >= MENUS) != (target >= MENUS)
        || (source >= MENUS && !registry::unchanged(source, target, "menu"))
        || titles_change(source, target)
}

/// Whether `mount_screen_open` needs converting from `source` to `target`.
pub fn mount_needed(source: i32, target: i32) -> bool {
    target < MENUS || (source >= WIDE_MOUNT) != (target >= WIDE_MOUNT)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Titles copied as they are: the text codec's conversions are its own.
    fn raw_title(
        shared: &mut Shared,
        direction: Direction,
        rw: &mut Rewriter<'_>,
    ) -> io::Result<()> {
        if shared.source(direction) >= NBT_TITLES {
            rw.nbt(false)?;
        } else {
            rw.str()?;
        }
        Ok(())
    }

    fn legacy(window: u8, kind: &str, slots: u8, entity: Option<i32>) -> Vec<u8> {
        let mut writer = Writer::new();
        writer
            .u8(window)
            .string(kind)
            .string(r#"{"text":"T"}"#)
            .u8(slots);
        if let Some(entity) = entity {
            writer.i32(entity);
        }
        writer.into_inner()
    }

    fn modern(protocol: i32, window: i32, menu: &str) -> Vec<u8> {
        let mut writer = Writer::new();
        writer
            .varint(window)
            .varint(menu_id(protocol, menu).unwrap());
        if protocol >= NBT_TITLES {
            crate::nbt::Nbt::String("T".into()).write(&mut writer, false);
        } else {
            writer.string(r#"{"text":"T"}"#);
        }
        writer.into_inner()
    }

    fn convert(client: i32, backend: i32, data: &[u8]) -> io::Result<Converted> {
        let mut shared = Shared::new(client, backend);
        convert_open_screen(&mut shared, Direction::Clientbound, data, raw_title)
    }

    #[test]
    fn windows_become_menus_and_back() {
        for (kind, slots, menu) in [
            ("minecraft:chest", 27, "generic_9x3"),
            ("minecraft:chest", 54, "generic_9x6"),
            ("minecraft:container", 9, "generic_9x1"),
            ("minecraft:dropper", 9, "generic_3x3"),
            ("minecraft:crafting_table", 0, "crafting"),
            ("minecraft:villager", 3, "merchant"),
            ("minecraft:shulker_box", 27, "shulker_box"),
        ] {
            let up = convert(477, 404, &legacy(7, kind, slots, None)).unwrap();
            assert_eq!(
                (up.name, up.data.clone()),
                ("open_screen", modern(477, 7, menu)),
                "{kind}"
            );
            let down = convert(404, 477, &up.data).unwrap();
            let (expected_kind, expected_slots) = legacy_window(menu);
            assert_eq!(
                down.data,
                legacy(7, expected_kind, expected_slots as u8, None)
            );
        }
        // Menus 1.13 lacks open the window with the same slots.
        let down = convert(404, 477, &modern(477, 3, "stonecutter")).unwrap();
        assert_eq!(down.data, legacy(3, "minecraft:enchanting_table", 0, None));
    }

    #[test]
    fn horse_windows_get_their_own_packet_from_1_14() {
        let up = convert(477, 404, &legacy(2, HORSE, 17, Some(42))).unwrap();
        assert_eq!(up.name, "mount_screen_open");
        let mut expected = Writer::new();
        expected.u8(2).varint(17).i32(42);
        assert_eq!(up.data, expected.buf);
        let down = convert_mount_screen(477, 404, &up.data).unwrap();
        assert_eq!(down.name, "open_screen");
        let mut reader = Reader::new(&down.data);
        assert_eq!(reader.u8().unwrap(), 2);
        assert_eq!(reader.str().unwrap(), HORSE);
        reader.str().unwrap();
        assert_eq!((reader.u8().unwrap(), reader.i32().unwrap()), (17, 42));
        // The window ID is a VarInt from 1.21.2.
        let wide = convert_mount_screen(767, 768, &up.data).unwrap();
        let mut reader = Reader::new(&wide.data);
        assert_eq!(
            (reader.varint().unwrap(), reader.varint().unwrap()),
            (2, 17)
        );
        assert_eq!(
            convert_mount_screen(768, 767, &wide.data).unwrap().data,
            up.data
        );
        let mut big = Writer::new();
        big.varint(300).varint(1).i32(1);
        assert!(convert_mount_screen(768, 767, &big.buf).is_err());
    }

    #[test]
    fn missing_menus_get_stand_ins_and_bad_input_fails() {
        assert_eq!(stand_in("crafter_3x3"), "generic_3x3");
        assert_eq!(stand_in("loom"), "generic_9x1");
        let mut shared = Shared::new(735, 736);
        let same = convert_open_screen(
            &mut shared,
            Direction::Clientbound,
            &modern(736, 1, "smithing"),
            raw_title,
        )
        .unwrap();
        assert_eq!(same.data, modern(735, 1, "smithing"));
        let down = convert(578, 735, &modern(735, 1, "smithing")).unwrap();
        assert_eq!(down.data, modern(578, 1, "anvil"));
        assert!(convert(578, 735, &[1, 100]).is_err());
        let mut long = modern(735, 1, "anvil");
        long.push(0);
        assert!(convert(578, 735, &long).is_err());
        assert!(convert(404, 477, &modern(477, 256, "anvil")).is_err());
        assert!(open_needed(404, 477));
        assert!(!open_needed(498, 490));
        assert!(mount_needed(767, 768));
        assert!(!mount_needed(766, 767));
    }

    #[test]
    fn corpus_screens_convert_to_both_neighbours() {
        use crate::version::PROTOCOLS;
        let mut checked = 0;
        for (index, &(protocol, _)) in PROTOCOLS.iter().enumerate() {
            let packets: Vec<_> = crate::codec::misc::sessions(protocol)
                .into_iter()
                .flatten()
                .collect();
            for packet in packets.iter().filter(|packet| {
                matches!(packet.name(protocol), "open_screen" | "mount_screen_open")
            }) {
                for neighbour in [index.checked_sub(1), Some(index + 1)]
                    .into_iter()
                    .flatten()
                {
                    let Some(&(other, _)) = PROTOCOLS.get(neighbour) else {
                        continue;
                    };
                    if titles_change(protocol, other) {
                        continue;
                    }
                    let converted = if packet.name(protocol) == "open_screen" {
                        convert(other, protocol, &packet.data).unwrap()
                    } else {
                        convert_mount_screen(protocol, other, &packet.data).unwrap()
                    };
                    let back = if converted.name == "open_screen" {
                        convert(protocol, other, &converted.data).unwrap()
                    } else {
                        convert_mount_screen(other, protocol, &converted.data).unwrap()
                    };
                    assert_eq!(
                        back.data, packet.data,
                        "{protocol} -> {other} -> {protocol}"
                    );
                    checked += 1;
                }
            }
        }
        eprintln!("{checked} recorded screens converted there and back");
    }
}
