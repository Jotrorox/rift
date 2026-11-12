//! 26.2 removed the bed block entity: a bed is only its block state. A 26.1
//! client draws a bed through its block entity (empty, type `bed`) and shows
//! nothing without one, so the downgrade adds one wherever a bed appears:
//! in chunk columns, and after block updates that place a bed. Going up,
//! the shared codecs drop a 26.1 server's bed block entities, which 26.2
//! has no type for.
use super::OLDER;
use crate::{
    Packet, State,
    codec::{
        Shared,
        chunk::{
            blocks::read_changes,
            model::{BlockEntity, Chunk},
            packets::write_position,
        },
    },
    data,
    nbt::{Compound, Nbt},
    step::Cx,
    wire::{Position, Writer},
};
use std::{io, sync::OnceLock};

/// 26.1's bed states, as ranges `first..end` (the colours are adjacent).
fn bed_states() -> &'static [(i32, i32)] {
    static STATES: OnceLock<Vec<(i32, i32)>> = OnceLock::new();
    STATES.get_or_init(|| {
        let mut ranges: Vec<(i32, i32)> = Vec::new();
        for block in &data::version(OLDER).block_states.blocks {
            if !block.name.ends_with("_bed") {
                continue;
            }
            let (first, end) = (block.first_state, block.first_state + block.state_count());
            match ranges.last_mut() {
                Some(last) if last.1 == first => last.1 = end,
                _ => ranges.push((first, end)),
            }
        }
        ranges
    })
}

/// Whether `state` (a 26.1 block state) is a bed.
pub fn is_bed(state: i32) -> bool {
    bed_states()
        .iter()
        .any(|(first, end)| (*first..*end).contains(&state))
}

fn bed_type() -> io::Result<i32> {
    data::version(OLDER)
        .registry("block_entity_type")
        .and_then(|registry| registry.id("bed"))
        .ok_or_else(|| crate::wire::invalid("26.1 has no bed block entity"))
}

/// Give every bed of a column already in 26.1 terms its block entity.
pub fn add_to_chunk(chunk: &mut Chunk, scratch: &mut Vec<u32>) -> io::Result<()> {
    let kind = bed_type()?;
    let mut values = Vec::new();
    for (index, section) in chunk.sections.iter().enumerate() {
        let Some(section) = section else { continue };
        if section.blocks.count(is_bed, scratch)? == 0 {
            continue;
        }
        section.blocks.values(&mut values, scratch)?;
        let base_y = (chunk.min_section + index as i32) * 16;
        for (entry, state) in values.iter().enumerate() {
            if !is_bed(*state) {
                continue;
            }
            let (x, y, z) = (entry & 15, entry >> 8, (entry >> 4) & 15);
            let (x, y, z) = (
                chunk.x.wrapping_mul(16) + x as i32,
                base_y + y as i32,
                chunk.z.wrapping_mul(16) + z as i32,
            );
            if !chunk
                .block_entities
                .iter()
                .any(|known| (known.x, known.y, known.z) == (x, y, z))
            {
                chunk.block_entities.push(BlockEntity {
                    x,
                    y,
                    z,
                    kind,
                    nbt: None,
                });
            }
        }
    }
    Ok(())
}

/// A bed's block entity, as the official 26.1 server sends it when a bed
/// is placed.
fn send(cx: &mut Cx<'_>, state: State, position: Position) -> io::Result<()> {
    let id = data::version(OLDER).expect_packet(
        State::Play,
        crate::Direction::Clientbound,
        "block_entity_data",
    );
    let mut writer = Writer::with_capacity(12);
    write_position(&mut writer, OLDER, position);
    writer.varint(bed_type()?);
    Nbt::Compound(Compound::new()).write(&mut writer, false);
    cx.to_client(state, Packet::new(id, writer.buf));
    Ok(())
}

/// After a `block_update` from the 26.2 backend (`position` and its 26.2
/// `block` state).
pub fn after_block_update(
    shared: &mut Shared,
    cx: &mut Cx<'_>,
    state: State,
    position: Position,
    block: i32,
) -> io::Result<()> {
    let mapping = shared.mapping(crate::Direction::Clientbound, "block_state");
    if mapping.get(block).is_some_and(is_bed) {
        send(cx, state, position)?;
    }
    Ok(())
}

/// After a `section_blocks_update` from the 26.2 backend (its body).
pub fn after_section_update(
    shared: &mut Shared,
    cx: &mut Cx<'_>,
    state: State,
    body: &[u8],
    source: i32,
) -> io::Result<()> {
    let mapping = shared.mapping(crate::Direction::Clientbound, "block_state");
    for change in read_changes(body, source)? {
        if mapping.get(change.state).is_some_and(is_bed) {
            let position = Position {
                x: change.x,
                y: change.y,
                z: change.z,
            };
            send(cx, state, position)?;
        }
    }
    Ok(())
}
