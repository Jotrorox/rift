//! Serverbound packets 1.13 added for what 1.12 clients sent as `MC|...`
//! plugin messages. Both carry the same information; enums that were names
//! became numbers and flag bytes replaced booleans.
use super::{Up, packets};
use crate::{
    Direction, Packet, State,
    codec::item,
    step::{Cx, RouteBuilder},
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::io;

pub fn install(builder: RouteBuilder<Up>) -> RouteBuilder<Up> {
    builder
        .serverbound(State::Play, "edit_book", edit_book)
        .serverbound(State::Play, "pick_item", pick_item)
        .serverbound(State::Play, "rename_item", rename_item)
        .serverbound(State::Play, "select_trade", select_trade)
        .serverbound(State::Play, "set_beacon", set_beacon)
        .serverbound(State::Play, "set_command_block", set_command_block)
        .serverbound(State::Play, "set_command_minecart", set_command_minecart)
        .serverbound(State::Play, "set_structure_block", set_structure_block)
        // Block and entity NBT queries (debug screen copies, `/data` hints)
        // have no 1.12 counterpart; the client simply gets no answer.
        .drop_serverbound(State::Play, "block_entity_tag_query")
        .drop_serverbound(State::Play, "entity_tag_query")
}

/// A 1.12 plugin message on `channel` whose payload `body` writes.
fn plugin_message(
    cx: &mut Cx<'_>,
    state: State,
    channel: &str,
    body: impl FnOnce(&mut Writer) -> io::Result<()>,
) -> io::Result<()> {
    let mut writer = Writer::new();
    writer.string(channel);
    body(&mut writer)?;
    cx.to_backend(
        state,
        Packet::new(packets::ids().custom_payload, writer.buf),
    );
    Ok(())
}

/// The name of entry `index` of a 1.12 enum that 1.13 numbers.
fn name(names: &'static [&'static str], index: i32) -> io::Result<&'static str> {
    usize::try_from(index)
        .ok()
        .and_then(|index| names.get(index))
        .copied()
        .ok_or_else(|| invalid("enum value out of range"))
}

/// Book and quill contents: the stack and whether it is being signed.
fn edit_book(step: &mut Up, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
    let mut rw = Rewriter::new(&packet.data);
    // INTEGRATION: the item codec converts 1.13 stacks to 1.12 ones; a stub
    // that fails until it lands.
    item::transcode(&mut step.shared, Direction::Serverbound, &mut rw)?;
    let signing = rw.r.bool()?;
    let stack = rw.finish()?;
    let channel = if signing { "MC|BSign" } else { "MC|BEdit" };
    plugin_message(cx, state, channel, |writer| {
        writer.bytes(&stack);
        Ok(())
    })
}

fn pick_item(_: &mut Up, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let slot = reader.varint()?;
    reader.finish()?;
    plugin_message(cx, state, "MC|PickItem", |writer| {
        writer.varint(slot);
        Ok(())
    })
}

/// The anvil's name field; 1.12 reads at most 32767 characters too.
fn rename_item(_: &mut Up, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let name = reader.str()?;
    reader.finish()?;
    plugin_message(cx, state, "MC|ItemName", |writer| {
        writer.string(name);
        Ok(())
    })
}

fn select_trade(_: &mut Up, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let trade = reader.varint()?;
    reader.finish()?;
    plugin_message(cx, state, "MC|TrSel", |writer| {
        writer.i32(trade);
        Ok(())
    })
}

/// Primary and secondary effect IDs; the effect registry is the same.
fn set_beacon(step: &mut Up, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let primary = reader.varint()?;
    let secondary = reader.varint()?;
    reader.finish()?;
    let mapping = step.shared.mapping(Direction::Serverbound, "mob_effect");
    // 0 (or an unknown effect) is "none" in both versions.
    let effect = |id: i32| mapping.get(id).unwrap_or(0);
    plugin_message(cx, state, "MC|Beacon", |writer| {
        writer.i32(effect(primary)).i32(effect(secondary));
        Ok(())
    })
}

const COMMAND_BLOCK_MODES: &[&str] = &["SEQUENCE", "AUTO", "REDSTONE"];

/// 1.13: position, command, mode, flags (track output, conditional,
/// automatic). 1.12 `MC|AutoCmd`: x, y, z as ints, command, track output,
/// mode name, conditional, automatic.
fn set_command_block(_: &mut Up, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let position = reader.position_legacy()?;
    let command = reader.str()?;
    let mode = name(COMMAND_BLOCK_MODES, reader.varint()?)?;
    let flags = reader.u8()?;
    reader.finish()?;
    plugin_message(cx, state, "MC|AutoCmd", |writer| {
        writer
            .i32(position.x)
            .i32(position.y)
            .i32(position.z)
            .string(command)
            .bool(flags & 1 != 0)
            .string(mode)
            .bool(flags & 2 != 0)
            .bool(flags & 4 != 0);
        Ok(())
    })
}

/// 1.12 `MC|AdvCmd` serves command blocks (type 0) and minecarts (type 1).
fn set_command_minecart(
    _: &mut Up,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let entity = reader.varint()?;
    let command = reader.str()?;
    let track_output = reader.bool()?;
    reader.finish()?;
    plugin_message(cx, state, "MC|AdvCmd", |writer| {
        writer.u8(1).i32(entity).string(command).bool(track_output);
        Ok(())
    })
}

const STRUCTURE_MODES: &[&str] = &["SAVE", "LOAD", "CORNER", "DATA"];
const MIRRORS: &[&str] = &["NONE", "LEFT_RIGHT", "FRONT_BACK"];
const ROTATIONS: &[&str] = &[
    "NONE",
    "CLOCKWISE_90",
    "CLOCKWISE_180",
    "COUNTERCLOCKWISE_90",
];

/// 1.13 numbers the action from 0 (update data, save, load, detect size),
/// 1.12 from 1; modes, mirrors and rotations went from names to numbers;
/// offsets and sizes from ints to bytes; three booleans into a flags byte.
fn set_structure_block(
    _: &mut Up,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let position = reader.position_legacy()?;
    let action = reader.varint()?;
    if !(0..4).contains(&action) {
        return Err(invalid("structure block action out of range"));
    }
    let mode = name(STRUCTURE_MODES, reader.varint()?)?;
    let structure = reader.str()?;
    let mut bounds = [0i8; 6];
    for value in &mut bounds {
        *value = reader.i8()?;
    }
    let mirror = name(MIRRORS, reader.varint()?)?;
    let rotation = name(ROTATIONS, reader.varint()?)?;
    let metadata = reader.str()?;
    let integrity = reader.f32()?;
    let seed = reader.varlong()?;
    let flags = reader.u8()?;
    reader.finish()?;
    plugin_message(cx, state, "MC|Struct", |writer| {
        writer
            .i32(position.x)
            .i32(position.y)
            .i32(position.z)
            .u8(action as u8 + 1)
            .string(mode)
            .string(structure);
        for value in bounds {
            writer.i32(i32::from(value));
        }
        writer
            .string(mirror)
            .string(rotation)
            .string(metadata)
            .bool(flags & 1 != 0)
            .bool(flags & 2 != 0)
            .bool(flags & 4 != 0)
            .f32(integrity)
            .varlong(seed);
        Ok(())
    })
}
