//! Packets whose layout alone differs between 1.13.2 and 1.14, written once
//! for both directions: each conversion reads the sender's protocol and
//! writes the receiver's.
use crate::{
    State,
    codec::HasShared,
    step::RouteBuilder,
    version,
    wire::{Position, Reader, Rewriter, Writer, invalid},
};
use std::io;

pub(super) fn read_position(r: &mut Reader<'_>, protocol: i32) -> io::Result<Position> {
    if version::modern_positions(protocol) {
        r.position()
    } else {
        r.position_legacy()
    }
}

pub(super) fn write_position(w: &mut Writer, protocol: i32, position: Position) {
    if version::modern_positions(protocol) {
        w.position(position);
    } else {
        w.position_legacy(position);
    }
}

fn position(rw: &mut Rewriter<'_>, from: i32, to: i32) -> io::Result<()> {
    let value = read_position(&mut rw.r, from)?;
    write_position(&mut rw.w, to, value);
    Ok(())
}

/// Sign lines are at most 384 characters in both protocols.
const SIGN_LINE: usize = 384;
/// Level types (`default`, `flat`, ...) are at most 16 characters.
pub(super) const LEVEL_TYPE: usize = 16;

pub(super) fn block_destruction(from: i32, to: i32, data: &[u8]) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(data);
    rw.varint()?;
    position(&mut rw, from, to)?;
    rw.i8()?;
    rw.finish()
}

pub(super) fn position_only(from: i32, to: i32, data: &[u8]) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(data);
    position(&mut rw, from, to)?;
    rw.finish()
}

pub(super) fn player_action(from: i32, to: i32, data: &[u8]) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(data);
    rw.varint()?;
    position(&mut rw, from, to)?;
    rw.i8()?;
    rw.finish()
}

pub(super) fn block_entity_tag_query(from: i32, to: i32, data: &[u8]) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(data);
    rw.varint()?;
    position(&mut rw, from, to)?;
    rw.finish()
}

pub(super) fn set_command_block(from: i32, to: i32, data: &[u8]) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(data);
    position(&mut rw, from, to)?;
    rw.str()?;
    rw.varint()?;
    rw.u8()?;
    rw.finish()
}

pub(super) fn set_structure_block(from: i32, to: i32, data: &[u8]) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(data);
    position(&mut rw, from, to)?;
    rw.varint()?; // Action.
    rw.varint()?; // Mode.
    rw.str()?;
    rw.copy(6)?; // Offset and size, a byte per axis.
    rw.varint()?; // Mirror.
    rw.varint()?; // Rotation.
    rw.str()?;
    rw.f32()?;
    rw.varlong()?;
    rw.u8()?;
    rw.finish()
}

pub(super) fn sign_update(from: i32, to: i32, data: &[u8]) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(data);
    position(&mut rw, from, to)?;
    for _ in 0..4 {
        rw.string(SIGN_LINE)?;
    }
    rw.finish()
}

/// 1.14 sends the hand first and says whether the player's head is inside
/// the clicked block, which a 1.13 server never needs.
pub(super) fn use_item_on(from: i32, to: i32, data: &[u8]) -> io::Result<Vec<u8>> {
    let mut r = Reader::new(data);
    let modern = version::modern_positions(from);
    let hand = if modern { r.varint()? } else { 0 };
    let target = read_position(&mut r, from)?;
    let face = r.varint()?;
    let hand = if modern { hand } else { r.varint()? };
    let cursor = [r.f32()?, r.f32()?, r.f32()?];
    let inside = modern && r.bool()?;
    r.finish()?;
    let mut w = Writer::with_capacity(data.len() + 1);
    if version::modern_positions(to) {
        w.varint(hand);
        write_position(&mut w, to, target);
        w.varint(face);
    } else {
        write_position(&mut w, to, target);
        w.varint(face).varint(hand);
    }
    for value in cursor {
        w.f32(value);
    }
    if version::modern_positions(to) {
        w.bool(inside);
    }
    Ok(w.buf)
}

/// Recipe book settings: open and filtering flags for the crafting and
/// furnace books, and from 1.14 for the blast furnace and smoker books too,
/// which a 1.13 player leaves closed.
pub(super) fn recipe_book_update(from: i32, to: i32, data: &[u8]) -> io::Result<Vec<u8>> {
    let books = |protocol: i32| if protocol >= 477 { 8 } else { 4 };
    let mut rw = Rewriter::new(data);
    match rw.varint()? {
        0 => {
            rw.str()?;
        }
        1 => {
            let mut flags = [false; 8];
            for flag in flags.iter_mut().take(books(from)) {
                *flag = rw.r.bool()?;
            }
            for flag in &flags[..books(to)] {
                rw.w.bool(*flag);
            }
        }
        _ => return Err(invalid("unknown recipe book update")),
    }
    rw.finish()
}

/// Route handlers for the conversions, for either orientation of the pair.
mod handle {
    use super::super::send;
    use crate::{Direction, Packet, State, codec::HasShared, step::Cx};
    use std::io;

    macro_rules! handlers {
        ($($name:ident: $direction:ident => $convert:ident;)*) => {$(
            pub(in super::super) fn $name<S: HasShared>(
                step: &mut S,
                cx: &mut Cx<'_>,
                state: State,
                packet: Packet,
            ) -> io::Result<()> {
                let shared = step.shared();
                let direction = Direction::$direction;
                let (from, to) = (shared.source(direction), shared.target(direction));
                let data = super::$convert(from, to, &packet.data)?;
                send(cx, direction, state, Packet::new(packet.id, data));
                Ok(())
            }
        )*};
    }

    handlers! {
        block_destruction: Clientbound => block_destruction;
        open_sign_editor: Clientbound => position_only;
        set_default_spawn_position: Clientbound => position_only;
        player_action: Serverbound => player_action;
        block_entity_tag_query: Serverbound => block_entity_tag_query;
        set_command_block: Serverbound => set_command_block;
        set_structure_block: Serverbound => set_structure_block;
        sign_update: Serverbound => sign_update;
        use_item_on: Serverbound => use_item_on;
        recipe_book_update: Serverbound => recipe_book_update;
    }
}

/// The routes both steps of the pair share: every packet whose layout alone
/// changed. Names are the sender's.
pub(super) fn install<S: HasShared + 'static>(builder: RouteBuilder<S>) -> RouteBuilder<S> {
    use State::Play;
    builder
        .clientbound(Play, "block_destruction", handle::block_destruction::<S>)
        .clientbound(Play, "open_sign_editor", handle::open_sign_editor::<S>)
        .clientbound(
            Play,
            "set_default_spawn_position",
            handle::set_default_spawn_position::<S>,
        )
        .serverbound(Play, "player_action", handle::player_action::<S>)
        .serverbound(
            Play,
            "block_entity_tag_query",
            handle::block_entity_tag_query::<S>,
        )
        .serverbound(Play, "set_command_block", handle::set_command_block::<S>)
        .serverbound(
            Play,
            "set_structure_block",
            handle::set_structure_block::<S>,
        )
        .serverbound(Play, "sign_update", handle::sign_update::<S>)
        .serverbound(Play, "use_item_on", handle::use_item_on::<S>)
        .serverbound(Play, "recipe_book_update", handle::recipe_book_update::<S>)
}
