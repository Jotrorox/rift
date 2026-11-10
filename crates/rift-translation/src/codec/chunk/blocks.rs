//! Single block packets: `block_update`, `section_blocks_update` (and the
//! older `chunk_blocks_update`), `block_event`, `level_event` and
//! `block_entity_data`.
use super::{
    block_entity, flattening,
    model::Chunk,
    packets::{block_fallback, ids, read_position, write_position},
};
use crate::{
    Direction, Packet, State as Phase,
    codec::{HasShared, Shared},
    data,
    mapping::Mapping,
    nbt::{Compound, Nbt},
    step::Cx,
    version,
    wire::{Position, Reader, Writer, invalid},
};
use std::io;

/// A block state ID as a level event or 1.12 block update carries it.
/// Before 1.13 a state is `id << 4 | metadata` in chunks and block
/// updates, but `id | metadata << 12` in level event 2001.
fn map_state(mapping: Option<&Mapping>, id: i32, fallback: i32) -> i32 {
    match mapping {
        Some(mapping) => mapping.get(id).unwrap_or(fallback),
        None => id,
    }
}

/// `block_update`: a position and a block state.
pub fn block_update<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: Phase,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let (source, target) = (shared.backend, shared.client);
    let mut reader = Reader::new(&packet.data);
    let position = read_position(&mut reader, source)?;
    let block = reader.varint()?;
    reader.finish()?;
    let mapping = ids(source, target, "block_state")?;
    let mapped = map_state(mapping, block, block_fallback(target));
    let mut writer = Writer::with_capacity(12);
    write_position(&mut writer, target, position);
    writer.varint(mapped);
    cx.to_client(state, Packet::new(packet.id, writer.buf));
    if (source < 393) != (target < 393) {
        let modern = if source >= 393 { block } else { mapped };
        track(shared, position, modern);
        if source >= 393 {
            // A 1.12 client sees the colour, kind or plant only through the
            // block entity.
            send_legacy_entity(shared, cx, state, position, block, None)?;
        }
    }
    Ok(())
}

/// At most this many flattened blocks are followed per connection.
const TRACKED: usize = 1 << 16;

/// Remember (or forget) the 1.13 state at `position` for the flattening.
pub fn track(shared: &mut Shared, position: Position, modern: i32) {
    let states = &data::version(393).block_states;
    let key = (position.x, position.y, position.z);
    let tracked = &mut shared.chunk.flattened;
    if flattening::relevant(states, modern) {
        if tracked.len() >= TRACKED && !tracked.contains_key(&key) {
            tracked.clear();
        }
        tracked.insert(key, modern);
    } else {
        tracked.remove(&key);
    }
}

/// A 1.12 `block_entity_data` for the block entity 1.13 block `modern`
/// needs, merged into `tag` when the server sent one.
fn send_legacy_entity(
    shared: &mut Shared,
    cx: &mut Cx<'_>,
    state: Phase,
    position: Position,
    modern: i32,
    tag: Option<Compound>,
) -> io::Result<()> {
    let target = shared.client;
    let states = &data::version(shared.backend).block_states;
    let Some((id, fields)) = flattening::fields_down(states, modern) else {
        if let Some(tag) = tag {
            return send_entity(cx, state, target, position, &tag);
        }
        return Ok(());
    };
    let mut tag = tag.unwrap_or_else(|| {
        super::convert::with_metadata(None, id, position.x, position.y, position.z)
    });
    if id == "banner"
        && let Some(Nbt::List(patterns)) = tag.get_mut("Patterns")
    {
        for pattern in patterns.iter_mut() {
            if let Some(pattern) = pattern.as_compound_mut()
                && let Some(color) = pattern.int("Color")
            {
                pattern.insert("Color", 15 - color.clamp(0, 15));
            }
        }
    }
    for (key, value) in fields.0 {
        tag.insert(key, value);
    }
    send_entity(cx, state, target, position, &tag)
}

fn send_entity(
    cx: &mut Cx<'_>,
    state: Phase,
    target: i32,
    position: Position,
    tag: &Compound,
) -> io::Result<()> {
    let name = data::strip(tag.str("id").unwrap_or(""));
    let Some(action) = block_entity::action(target, name) else {
        return Ok(());
    };
    let id = data::version(target)
        .packet_id(Phase::Play, Direction::Clientbound, "block_entity_data")
        .ok_or_else(|| invalid("no block entity packet"))?;
    let mut writer = Writer::new();
    write_position(&mut writer, target, position);
    writer.u8(action as u8);
    Nbt::Compound(tag.clone()).write(&mut writer, true);
    cx.to_client(state, Packet::new(id, writer.buf));
    Ok(())
}

/// One changed block: absolute position and state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub state: i32,
}

/// The changes of a `section_blocks_update` / `chunk_blocks_update`.
pub fn read_changes(data: &[u8], protocol: i32) -> io::Result<Vec<Change>> {
    let mut reader = Reader::new(data);
    let mut changes = Vec::new();
    if protocol < 751 {
        let (chunk_x, chunk_z) = (reader.i32()?, reader.i32()?);
        let count = reader.count()?;
        changes.reserve(count);
        for _ in 0..count {
            let horizontal = reader.u8()?;
            let y = i32::from(reader.u8()?);
            changes.push(Change {
                x: chunk_x.wrapping_mul(16) + i32::from(horizontal >> 4),
                y,
                z: chunk_z.wrapping_mul(16) + i32::from(horizontal & 15),
                state: reader.varint()?,
            });
        }
    } else {
        let packed = reader.i64()?;
        let (section_x, section_z, section_y) = (
            (packed >> 42) as i32,
            ((packed << 22) >> 42) as i32,
            ((packed << 44) >> 44) as i32,
        );
        if protocol < 763 {
            reader.bool()?;
        }
        let count = reader.count()?;
        changes.reserve(count);
        for _ in 0..count {
            let entry = reader.varlong()?;
            let local = (entry & 0xfff) as i32;
            changes.push(Change {
                x: section_x.wrapping_mul(16) + (local >> 8 & 15),
                y: section_y.wrapping_mul(16) + (local & 15),
                z: section_z.wrapping_mul(16) + (local >> 4 & 15),
                state: i32::try_from(entry >> 12)
                    .map_err(|_| invalid("block state out of range"))?,
            });
        }
    }
    reader.finish()?;
    Ok(changes)
}

/// Write `changes` (all in one chunk column, and for 1.16.2 and newer in
/// one section) in `protocol`'s layout.
pub fn write_changes(changes: &[Change], protocol: i32) -> Vec<u8> {
    let mut writer = Writer::with_capacity(16 + changes.len() * 4);
    let first = changes.first().copied().unwrap_or(Change {
        x: 0,
        y: 0,
        z: 0,
        state: 0,
    });
    if protocol < 751 {
        writer
            .i32(first.x >> 4)
            .i32(first.z >> 4)
            .varint(changes.len() as i32);
        for change in changes {
            writer
                .u8(((change.x & 15) << 4 | (change.z & 15)) as u8)
                .u8(change.y as u8)
                .varint(change.state);
        }
    } else {
        let packed = (i64::from(first.x >> 4) & 0x3f_ffff) << 42
            | (i64::from(first.z >> 4) & 0x3f_ffff) << 20
            | (i64::from(first.y >> 4) & 0xf_ffff);
        writer.i64(packed);
        if protocol < 763 {
            writer.bool(false); // Light updates are not suppressed.
        }
        writer.varint(changes.len() as i32);
        for change in changes {
            let local = (change.x & 15) << 8 | (change.z & 15) << 4 | (change.y & 15);
            writer.varlong(i64::from(change.state) << 12 | i64::from(local));
        }
    }
    writer.buf
}

fn section_packet(protocol: i32) -> &'static str {
    if protocol >= 751 {
        "section_blocks_update"
    } else {
        "chunk_blocks_update"
    }
}

fn send_changes(
    cx: &mut Cx<'_>,
    state: Phase,
    target: i32,
    mut changes: Vec<Change>,
) -> io::Result<()> {
    let id = data::version(target)
        .packet_id(Phase::Play, Direction::Clientbound, section_packet(target))
        .ok_or_else(|| invalid("no section update packet"))?;
    if target < 755 {
        // Before 1.17 the world is 0 to 255.
        changes.retain(|change| (0..256).contains(&change.y));
    }
    if target < 751 {
        if !changes.is_empty() {
            cx.to_client(state, Packet::new(id, write_changes(&changes, target)));
        }
        return Ok(());
    }
    changes.sort_by_key(|change| change.y >> 4);
    for section in changes.chunk_by(|a, b| a.y >> 4 == b.y >> 4) {
        cx.to_client(state, Packet::new(id, write_changes(section, target)));
    }
    Ok(())
}

/// `section_blocks_update` and `chunk_blocks_update`.
pub fn section_blocks_update<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: Phase,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let (source, target) = (shared.backend, shared.client);
    let mut changes = read_changes(&packet.data, source)?;
    let mapping = ids(source, target, "block_state")?;
    let fallback = block_fallback(target);
    for change in &mut changes {
        change.state = map_state(mapping, change.state, fallback);
    }
    send_changes(cx, state, target, changes)
}

/// The sections of a partial column (before 1.17), already in the client's
/// terms, as section updates and block entity packets for 1.17 clients.
pub fn sections_as_updates(
    shared: &mut Shared,
    cx: &mut Cx<'_>,
    state: Phase,
    chunk: &Chunk,
) -> io::Result<()> {
    let target = shared.client;
    let mut scratch = std::mem::take(&mut shared.chunk.scratch);
    let mut changes = Vec::new();
    for (index, section) in chunk.sections.iter().enumerate() {
        let Some(section) = section else { continue };
        section
            .blocks
            .values(&mut scratch.values, &mut scratch.raw)?;
        let base_y = (chunk.min_section + index as i32) * 16;
        changes.extend(
            scratch
                .values
                .iter()
                .enumerate()
                .map(|(entry, state)| Change {
                    x: chunk.x.wrapping_mul(16) + (entry & 15) as i32,
                    y: base_y + (entry >> 8) as i32,
                    z: chunk.z.wrapping_mul(16) + (entry >> 4 & 15) as i32,
                    state: *state,
                }),
        );
    }
    shared.chunk.scratch = scratch;
    send_changes(cx, state, target, changes)?;
    let id = data::version(target)
        .packet_id(Phase::Play, Direction::Clientbound, "block_entity_data")
        .ok_or_else(|| invalid("no block entity packet"))?;
    for entity in &chunk.block_entities {
        let Some(Nbt::Compound(tag)) = &entity.nbt else {
            continue;
        };
        let Some(action) = tag
            .str("id")
            .and_then(|name| block_entity::action(target, crate::data::strip(name)))
        else {
            continue;
        };
        let mut writer = Writer::new();
        write_position(
            &mut writer,
            target,
            Position {
                x: entity.x,
                y: entity.y,
                z: entity.z,
            },
        );
        writer.u8(action as u8);
        Nbt::Compound(tag.clone()).write(&mut writer, true);
        cx.to_client(state, Packet::new(id, writer.buf));
    }
    Ok(())
}

/// `block_event`: a block's animation (chests, pistons, note blocks); the
/// block is a `block` registry ID (a numeric block ID before 1.13).
pub fn block_event<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: Phase,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let (source, target) = (shared.backend, shared.client);
    let mut reader = Reader::new(&packet.data);
    let position = read_position(&mut reader, source)?;
    let (action, parameter) = (reader.u8()?, reader.u8()?);
    let block = reader.varint()?;
    reader.finish()?;
    let block = match ids(source, target, "block")? {
        Some(mapping) => match mapping.get(block) {
            Some(block) => block,
            // The receiver cannot animate a block it does not know.
            None => return Ok(()),
        },
        None => block,
    };
    let mut writer = Writer::with_capacity(16);
    write_position(&mut writer, target, position);
    writer.u8(action).u8(parameter).varint(block);
    cx.to_client(state, Packet::new(packet.id, writer.buf));
    Ok(())
}

/// 1.13's smoke directions (a 3x3 grid, 4 is the centre) as 1.13.1
/// directions (down, up, north, south, west, east), and back.
fn smoke_up(data: i32) -> i32 {
    match data {
        1 => 2,
        0 | 3 | 6 => 4,
        2 | 5 | 8 => 5,
        7 => 3,
        _ => 0,
    }
}

fn smoke_down(data: i32) -> i32 {
    match data {
        2 => 1,
        3 => 7,
        4 => 3,
        5 => 5,
        _ => 4,
    }
}

/// Level events 1.19.3 dropped in favour of plain sounds: event, sound.
const SOUND_EVENTS: &[(i32, &str)] = &[
    (1005, "block.iron_door.open"),
    (1006, "block.wooden_door.open"),
    (1007, "block.wooden_trapdoor.open"),
    (1008, "block.fence_gate.open"),
    (1011, "block.iron_door.close"),
    (1012, "block.wooden_door.close"),
    (1013, "block.wooden_trapdoor.close"),
    (1014, "block.fence_gate.close"),
    (1036, "block.iron_trapdoor.close"),
    (1037, "block.iron_trapdoor.open"),
];

/// From 26.3 the splash potion events no longer play their sound; these
/// events do: particles event, sound event.
const SPLASH_SOUNDS: &[(i32, i32)] = &[(2002, 1053), (2007, 1054)];

/// `level_event`: an event ID, a position, data whose meaning depends on
/// the event, and whether it is heard everywhere.
pub fn level_event<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: Phase,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let (source, target) = (shared.backend, shared.client);
    let mut reader = Reader::new(&packet.data);
    let mut event = reader.i32()?;
    let position = read_position(&mut reader, source)?;
    let mut data = reader.i32()?;
    let global = reader.bool()?;
    reader.finish()?;
    match event {
        2001 => {
            // Block break particles and sound: a block state.
            let legacy = |protocol| !version::flattened(protocol);
            let mut id = if legacy(source) {
                (data & 0xfff) << 4 | (data >> 12 & 15)
            } else {
                data
            };
            id = map_state(
                ids(source, target, "block_state")?,
                id,
                block_fallback(target),
            );
            data = if legacy(target) {
                (id >> 4) | (id & 15) << 12
            } else {
                id
            };
        }
        1010 if source < 767 && target < 767 => {
            // A record starts playing: an item ID.
            if let Some(mapping) = ids(source, target, "item")? {
                match mapping.get(data) {
                    Some(item) => data = item,
                    None => {
                        event = 1011; // Stop the record instead.
                        data = 0;
                    }
                }
            }
        }
        1010 if (source < 767) != (target < 767) => {
            // INTEGRATION: from 1.21 the data is a `jukebox_song` registry
            // ID, a registry the server sends; the registries codec will map
            // between record items and songs. Until then the record stops.
            event = 1011;
            data = 0;
        }
        2000 if source == 393 && target == 401 => data = smoke_up(data),
        2000 if source == 401 && target == 393 => data = smoke_down(data),
        _ => {}
    }
    if source < 761
        && target >= 761
        && let Some((_, sound)) = SOUND_EVENTS.iter().find(|(known, _)| *known == event)
    {
        return send_sound(cx, state, target, sound, position);
    }
    if source >= 777 && target < 777 && SPLASH_SOUNDS.iter().any(|(_, sound)| *sound == event) {
        return Ok(()); // Older clients play it with the particles.
    }
    let mut writer = Writer::with_capacity(17);
    writer.i32(event);
    write_position(&mut writer, target, position);
    writer.i32(data).bool(global);
    cx.to_client(state, Packet::new(packet.id, writer.buf));
    if source < 777
        && target >= 777
        && let Some((_, sound)) = SPLASH_SOUNDS.iter().find(|(known, _)| *known == event)
    {
        let mut writer = Writer::with_capacity(17);
        writer.i32(*sound);
        write_position(&mut writer, target, position);
        writer.i32(0).bool(false);
        cx.to_client(state, Packet::new(packet.id, writer.buf));
    }
    Ok(())
}

/// A block sound at `position` as a 1.19.3 `sound` packet.
fn send_sound(
    cx: &mut Cx<'_>,
    state: Phase,
    target: i32,
    sound: &str,
    position: Position,
) -> io::Result<()> {
    let data = data::version(target);
    let (Some(id), Some(packet)) = (
        data.registry("sound_event")
            .and_then(|registry| registry.id(sound)),
        data.packet_id(Phase::Play, Direction::Clientbound, "sound"),
    ) else {
        return Ok(());
    };
    // The game varies the pitch a little; a fixed seed keeps it stable.
    let mut writer = Writer::with_capacity(32);
    writer
        .varint(id + 1)
        .varint(4) // Blocks.
        .i32(position.x * 8 + 4)
        .i32(position.y * 8 + 4)
        .i32(position.z * 8 + 4)
        .f32(1.0)
        .f32(0.95)
        .i64(0);
    cx.to_client(state, Packet::new(packet, writer.buf));
    Ok(())
}

/// `block_entity_data`: a position, an action (before 1.18) or block entity
/// type, and the block entity's tag.
pub fn block_entity_data<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: Phase,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let (source, target) = (shared.backend, shared.client);
    let mut reader = Reader::new(&packet.data);
    let position = read_position(&mut reader, source)?;
    let kind = if source >= 757 {
        reader.varint()?
    } else {
        i32::from(reader.u8()?)
    };
    let mut nbt = Nbt::read(&mut reader, !version::nameless_nbt(source))?;
    reader.finish()?;
    let key = (position.x, position.y, position.z);
    if source < 393 && target >= 393 {
        // The block entity decides which 1.13 block this is.
        if let (Some(&modern), Some(Nbt::Compound(tag))) =
            (shared.chunk.flattened.get(&key), &mut nbt)
        {
            let states = &data::version(target).block_states;
            let id = data::strip(tag.str("id").unwrap_or("")).to_owned();
            let (new, keep) = flattening::fix_up(states, modern, &id, tag);
            if let Some(new) = new {
                shared.chunk.flattened.insert(key, new);
                let id = data::version(target)
                    .packet_id(Phase::Play, Direction::Clientbound, "block_update")
                    .ok_or_else(|| invalid("no block update packet"))?;
                let mut writer = Writer::with_capacity(12);
                write_position(&mut writer, target, position);
                writer.varint(new);
                cx.to_client(state, Packet::new(id, writer.buf));
            }
            if !keep {
                return Ok(());
            }
        }
    }
    if source >= 393
        && target < 393
        && let Some(&modern) = shared.chunk.flattened.get(&key)
    {
        let tag = match nbt {
            Some(Nbt::Compound(tag)) => Some(tag),
            _ => None,
        };
        return send_legacy_entity(shared, cx, state, position, modern, tag);
    }
    let Some((kind, nbt)) =
        block_entity::convert_packet(shared, Direction::Clientbound, position, kind, nbt)?
    else {
        return Ok(());
    };
    let mut writer = Writer::new();
    write_position(&mut writer, target, position);
    if target >= 757 {
        writer.varint(kind);
    } else {
        writer.u8(kind as u8);
    }
    Nbt::write_optional(nbt.as_ref(), &mut writer, !version::nameless_nbt(target));
    cx.to_client(state, Packet::new(packet.id, writer.buf));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_changes_round_trip_in_both_layouts() {
        let changes = vec![
            Change {
                x: -17,
                y: 70,
                z: 33,
                state: 9,
            },
            Change {
                x: -32,
                y: 79,
                z: 47,
                state: 30000,
            },
        ];
        for protocol in [736, 751, 763] {
            let bytes = write_changes(&changes, protocol);
            assert_eq!(
                read_changes(&bytes, protocol).unwrap(),
                changes,
                "{protocol}"
            );
        }
        let negative = vec![Change {
            x: 5,
            y: -60,
            z: -5,
            state: 1,
        }];
        assert_eq!(
            read_changes(&write_changes(&negative, 770), 770).unwrap(),
            negative
        );
    }

    #[test]
    fn smoke_directions_survive_a_round_trip() {
        // 1.13 has no upward smoke.
        for direction in [0, 2, 3, 4, 5] {
            assert_eq!(smoke_up(smoke_down(direction)), direction);
        }
    }
}
