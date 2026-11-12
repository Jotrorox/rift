//! Entity movement between 26.2's fixed deltas and 26.3's position paths.
//!
//! 26.3 `move_entity_pos`/`move_entity_pos_rot` pack the on-ground flag and
//! a step count into one VarInt; with no steps the delta is one linear move
//! (three shorts in 1/4096 blocks), otherwise each step has a tick count and
//! a delta from the previous step. `move_entity_rot` moved its on-ground
//! flag in front of the angles. `entity_position_sync` sends a position
//! path (linear: one position; stepped: positions with tick counts) and no
//! velocity.
//!
//! The 26.3 client interpolates a linear move over the entity type's own
//! number of steps, exactly what a 26.2 client does with every move, so
//! upgrading sends linear moves. Downgrading sums the steps of a stepped
//! move into one delta (the old client interpolates on its own) and splits
//! a sum too large for one delta into several moves.
use crate::wire::{Reader, Writer, invalid};
use std::io;

/// The three movement packets that carry a delta or angles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Move {
    Pos,
    PosRot,
    Rot,
}

/// Bytes per step at least: a one-byte VarInt and three shorts.
const MIN_STEP_BYTES: usize = 7;

/// A 26.2 movement packet in 26.3's layout.
pub(super) fn upgrade(kind: Move, data: &[u8]) -> io::Result<Vec<u8>> {
    let mut r = Reader::new(data);
    let mut w = Writer::with_capacity(data.len() + 1);
    w.varint(r.varint()?);
    match kind {
        Move::Pos | Move::PosRot => {
            let delta = [r.i16()?, r.i16()?, r.i16()?];
            let angles = if kind == Move::PosRot {
                Some([r.u8()?, r.u8()?])
            } else {
                None
            };
            let on_ground = r.bool()?;
            w.varint(i32::from(on_ground));
            for value in delta {
                w.i16(value);
            }
            if let Some([y_rot, x_rot]) = angles {
                w.u8(y_rot).u8(x_rot);
            }
        }
        Move::Rot => {
            let (y_rot, x_rot) = (r.u8()?, r.u8()?);
            w.bool(r.bool()?).u8(y_rot).u8(x_rot);
        }
    }
    r.finish()?;
    Ok(w.into_inner())
}

/// A 26.3 movement packet as one or more 26.2 packets, each with its kind:
/// a summed delta beyond one packet's range becomes several position moves
/// followed by the original kind with the rest.
pub(super) fn downgrade(kind: Move, data: &[u8]) -> io::Result<Vec<(Move, Vec<u8>)>> {
    let mut r = Reader::new(data);
    let entity = r.varint()?;
    if kind == Move::Rot {
        let on_ground = r.bool()?;
        let (y_rot, x_rot) = (r.u8()?, r.u8()?);
        r.finish()?;
        let mut w = Writer::with_capacity(data.len());
        w.varint(entity).u8(y_rot).u8(x_rot).bool(on_ground);
        return Ok(vec![(kind, w.into_inner())]);
    }
    let properties = r.varint()?;
    let on_ground = properties & 1 != 0;
    let steps = (properties as u32 >> 1) as usize;
    let mut total = [0i64; 3];
    if steps == 0 {
        for value in &mut total {
            *value = i64::from(r.i16()?);
        }
    } else {
        if steps > r.remaining() / MIN_STEP_BYTES {
            return Err(invalid("too many movement steps"));
        }
        for _ in 0..steps {
            r.varint()?;
            for value in &mut total {
                *value += i64::from(r.i16()?);
            }
        }
    }
    let angles = if kind == Move::PosRot {
        Some([r.u8()?, r.u8()?])
    } else {
        None
    };
    r.finish()?;
    let largest = total
        .iter()
        .map(|value| value.unsigned_abs())
        .max()
        .unwrap_or(0);
    let parts = largest.div_ceil(i16::MAX as u64).max(1) as i64;
    let mut sent = [0i64; 3];
    let mut packets = Vec::with_capacity(parts as usize);
    for part in 1..=parts {
        let last = part == parts;
        let mut w = Writer::with_capacity(12);
        w.varint(entity);
        for axis in 0..3 {
            let target = total[axis] * part / parts;
            w.i16((target - sent[axis]) as i16);
            sent[axis] = target;
        }
        let kind = match angles {
            Some([y_rot, x_rot]) if last => {
                w.u8(y_rot).u8(x_rot);
                Move::PosRot
            }
            _ => Move::Pos,
        };
        w.bool(on_ground);
        packets.push((kind, w.into_inner()));
    }
    Ok(packets)
}

/// 26.2 `entity_position_sync` (position, velocity, angles, on ground) as
/// 26.3's (linear path, angles, on ground). 26.3 sends velocity separately.
pub(super) fn upgrade_sync(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut r = Reader::new(data);
    let mut w = Writer::with_capacity(data.len());
    w.varint(r.varint()?).varint(LINEAR);
    for _ in 0..3 {
        w.f64(r.f64()?);
    }
    for _ in 0..3 {
        r.f64()?;
    }
    w.f32(r.f32()?).f32(r.f32()?).bool(r.bool()?);
    r.finish()?;
    Ok(w.into_inner())
}

/// 26.3 `entity_position_sync` as 26.2's: the path's end position, and as
/// velocity the last step's movement per tick (none for a linear path).
pub(super) fn downgrade_sync(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut r = Reader::new(data);
    let entity = r.varint()?;
    let mut position = [0.0; 3];
    let mut velocity = [0.0; 3];
    match r.varint()? {
        LINEAR => position = [r.f64()?, r.f64()?, r.f64()?],
        STEPPED => {
            let steps = r.count()?;
            if steps == 0 {
                return Err(invalid("empty position path"));
            }
            let mut previous = None;
            let mut ticks = 0;
            for _ in 0..steps {
                previous = Some(position);
                position = [r.f64()?, r.f64()?, r.f64()?];
                ticks = r.varint()?;
            }
            if let (Some(previous), true) = (previous.filter(|_| steps > 1), ticks > 0) {
                for axis in 0..3 {
                    velocity[axis] = (position[axis] - previous[axis]) / f64::from(ticks);
                }
            }
        }
        _ => return Err(invalid("invalid position path type")),
    }
    let (y_rot, x_rot, on_ground) = (r.f32()?, r.f32()?, r.bool()?);
    r.finish()?;
    let mut w = Writer::with_capacity(data.len() + 24);
    w.varint(entity);
    for value in position.into_iter().chain(velocity) {
        w.f64(value);
    }
    w.f32(y_rot).f32(x_rot).bool(on_ground);
    Ok(w.into_inner())
}

/// `PositionPath.Type` IDs.
const LINEAR: i32 = 0;
const STEPPED: i32 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn linear_moves_round_trip() {
        // The official 26.2 server's move of entity 2 by nothing, not on
        // ground, is byte for byte the same in 26.3.
        let recorded = hex("0200000000000000");
        assert_eq!(upgrade(Move::Pos, &recorded).unwrap(), recorded);
        assert_eq!(
            downgrade(Move::Pos, &recorded).unwrap(),
            [(Move::Pos, recorded)]
        );
        // Entity 25, delta (-51, 0, 0), angles, on ground.
        let old = hex("19ffcd00000000a0f001");
        let new = upgrade(Move::PosRot, &old).unwrap();
        assert_eq!(new, hex("1901ffcd00000000a0f0"));
        assert_eq!(
            downgrade(Move::PosRot, &new).unwrap(),
            [(Move::PosRot, old)]
        );
        let old = hex("19a0f000");
        let new = upgrade(Move::Rot, &old).unwrap();
        assert_eq!(new, hex("1900a0f0"));
        assert_eq!(downgrade(Move::Rot, &new).unwrap(), [(Move::Rot, old)]);
    }

    #[test]
    fn stepped_moves_sum_and_split() {
        // A move the official 26.3 server sent: entity 0x27, one step of
        // three ticks, no movement.
        let recorded = hex("270203000000000000");
        assert_eq!(
            downgrade(Move::Pos, &recorded).unwrap(),
            [(Move::Pos, hex("2700000000000000"))]
        );
        // Three steps of 30000 each on x: 90000 needs three packets, angles
        // on the last.
        let mut w = Writer::new();
        w.varint(7).varint(3 << 1 | 1);
        for _ in 0..3 {
            w.varint(2).i16(30000).i16(-1).i16(0);
        }
        w.u8(9).u8(8);
        let parts = downgrade(Move::PosRot, &w.buf).unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(
            parts.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
            [Move::Pos, Move::Pos, Move::PosRot]
        );
        let mut sum = [0i64; 3];
        for (kind, data) in &parts {
            let mut r = Reader::new(data);
            assert_eq!(r.varint().unwrap(), 7);
            for value in &mut sum {
                *value += i64::from(r.i16().unwrap());
            }
            if *kind == Move::PosRot {
                assert_eq!([r.u8().unwrap(), r.u8().unwrap()], [9, 8]);
            }
            assert!(r.bool().unwrap());
            r.finish().unwrap();
        }
        assert_eq!(sum, [90000, -3, 0]);
        // A step count the body cannot hold fails before allocating.
        assert!(downgrade(Move::Pos, &hex("27ffffffff0f00")).is_err());
    }

    #[test]
    fn position_syncs_convert_both_ways() {
        // Recorded from the official 26.3 server: a linear path.
        let recorded =
            hex("1800400c000000000000c04e0000000000004012000000000000000000000000000001");
        let old = downgrade_sync(&recorded).unwrap();
        assert_eq!(old.len(), recorded.len() - 1 + 24);
        assert_eq!(upgrade_sync(&old).unwrap(), recorded);
        // From the official 26.2 server, with a velocity 26.3 cannot carry.
        let recorded = hex(concat!(
            "19c004000000000000c04e0000000000004027199999a0000000000000000000",
            "0000000000000000003fa99999a0000000c2b400000000000001"
        ));
        let new = upgrade_sync(&recorded).unwrap();
        assert_eq!(new.len(), recorded.len() + 1 - 24);
        let mut back = downgrade_sync(&new).unwrap();
        back[25..49].copy_from_slice(&recorded[25..49]);
        assert_eq!(back, recorded);
        // From the official 26.3 server: one step, so no velocity.
        let recorded =
            hex("120101c004000000000000c04e000000000000400c00000000000003000000000000000001");
        let old = downgrade_sync(&recorded).unwrap();
        assert_eq!(&old[25..49], &[0; 24]);
        let mut w = Writer::new();
        w.varint(5).varint(STEPPED).varint(2);
        w.f64(0.0).f64(64.0).f64(0.0).varint(1);
        w.f64(3.0).f64(64.0).f64(0.0).varint(3);
        w.f32(90.0).f32(0.0).bool(false);
        let old = downgrade_sync(&w.buf).unwrap();
        let mut r = Reader::new(&old);
        r.varint().unwrap();
        let values: Vec<f64> = (0..6).map(|_| r.f64().unwrap()).collect();
        assert_eq!(values, [3.0, 64.0, 0.0, 1.0, 0.0, 0.0]);
        let mut empty = Writer::new();
        empty
            .varint(5)
            .varint(STEPPED)
            .varint(0)
            .f32(0.0)
            .f32(0.0)
            .bool(false);
        assert!(downgrade_sync(&empty.buf).is_err());
    }
}
