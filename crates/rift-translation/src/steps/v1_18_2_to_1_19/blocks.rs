//! Block change acknowledgements between 1.18.2 and 1.19.
//!
//! A 1.19 client numbers the actions it predicts (digging, using an item on
//! a block, using an item) and holds back server block updates for the
//! affected positions until the server acknowledges that sequence number
//! with `block_changed_ack`; acknowledgements are cumulative. A 1.18.2
//! server acknowledges digging with `block_break_ack` (position, the block
//! state there, the action) and answers using an item on a block with block
//! updates of the clicked block and then its neighbour on the clicked face.
//! A 1.18.2 client in turn remembers every digging action until
//! `block_break_ack` arrives, which a 1.19 server never sends.
use crate::wire::{Position, Reader, Writer, invalid};
use std::{collections::VecDeque, io};

/// Predictions a 1.19 client keeps open at most before the step gives up
/// waiting for the backend's answer and acknowledges the oldest.
const MAX_PENDING: usize = 64;

/// Digging actions of `player_action`; the other actions predict nothing.
const START_DIGGING: i32 = 0;
const ABORT_DIGGING: i32 = 1;
const STOP_DIGGING: i32 = 2;

fn is_digging(action: i32) -> bool {
    (START_DIGGING..=STOP_DIGGING).contains(&action)
}

/// The block next to `position` on `face` (0 down, 1 up, 2 north, 3 south,
/// 4 west, 5 east).
fn neighbour(position: Position, face: i32) -> Position {
    let Position { x, y, z } = position;
    match face {
        0 => Position { x, y: y - 1, z },
        1 => Position { x, y: y + 1, z },
        2 => Position { x, y, z: z - 1 },
        3 => Position { x, y, z: z + 1 },
        4 => Position { x: x - 1, y, z },
        5 => Position { x: x + 1, y, z },
        _ => position,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Awaiting {
    /// `block_break_ack` for this position and action.
    Dig(Position, i32),
    /// A block update at this position (the clicked block's neighbour).
    Update(Position),
}

/// For a 1.19 client on a 1.18.2 backend: which answers acknowledge which
/// sequence numbers.
#[derive(Debug, Default)]
pub struct Sequences {
    pending: VecDeque<(i32, Awaiting)>,
    acknowledged: i32,
}

impl Sequences {
    /// Acknowledge everything up to `sequence`: the body of a
    /// `block_changed_ack`, or `None` when that was acknowledged already.
    fn acknowledge(&mut self, sequence: i32) -> Option<Vec<u8>> {
        self.pending.retain(|(pending, _)| *pending > sequence);
        if sequence <= self.acknowledged {
            return None;
        }
        self.acknowledged = sequence;
        let mut w = Writer::new();
        w.varint(sequence);
        Some(w.buf)
    }

    fn expect(&mut self, sequence: i32, awaiting: Awaiting) -> Option<Vec<u8>> {
        let overflow = if self.pending.len() >= MAX_PENDING {
            let (oldest, _) = self.pending[0];
            self.acknowledge(oldest)
        } else {
            None
        };
        self.pending.push_back((sequence, awaiting));
        overflow
    }

    /// A 1.19 serverbound `player_action`: the 1.18.2 packet, and an
    /// acknowledgement to send the client now, if any.
    pub fn player_action(&mut self, data: &[u8]) -> io::Result<(Vec<u8>, Option<Vec<u8>>)> {
        let mut r = Reader::new(data);
        let action = r.varint()?;
        let position = r.position()?;
        let face = r.u8()?;
        let sequence = r.varint()?;
        r.finish()?;
        if sequence < 0 {
            return Err(invalid("negative block action sequence"));
        }
        let mut w = Writer::new();
        w.varint(action).position(position).u8(face);
        let ack = match sequence {
            0 => None,
            _ if is_digging(action) => self.expect(sequence, Awaiting::Dig(position, action)),
            _ => self.acknowledge(sequence),
        };
        Ok((w.buf, ack))
    }

    /// A 1.19 serverbound `use_item_on`.
    pub fn use_item_on(&mut self, data: &[u8]) -> io::Result<(Vec<u8>, Option<Vec<u8>>)> {
        let mut r = Reader::new(data);
        let hand = r.varint()?;
        let position = r.position()?;
        let face = r.varint()?;
        let cursor = r.take(3 * 4 + 1)?; // Cursor position, inside a block.
        let sequence = r.varint()?;
        r.finish()?;
        if sequence < 0 {
            return Err(invalid("negative block action sequence"));
        }
        let mut w = Writer::new();
        w.varint(hand).position(position).varint(face).bytes(cursor);
        let ack = match sequence {
            0 => None,
            _ => self.expect(sequence, Awaiting::Update(neighbour(position, face))),
        };
        Ok((w.buf, ack))
    }

    /// A 1.19 serverbound `use_item`: 1.18.2 answers nothing in particular,
    /// so it is acknowledged at once.
    pub fn use_item(&mut self, data: &[u8]) -> io::Result<(Vec<u8>, Option<Vec<u8>>)> {
        let mut r = Reader::new(data);
        let hand = r.varint()?;
        let sequence = r.varint()?;
        r.finish()?;
        if sequence < 0 {
            return Err(invalid("negative block action sequence"));
        }
        let mut w = Writer::new();
        w.varint(hand);
        let ack = (sequence > 0).then(|| self.acknowledge(sequence)).flatten();
        Ok((w.buf, ack))
    }

    /// A 1.18.2 `block_break_ack`: the position and backend block state to
    /// update the client with, and the acknowledgement to follow it.
    pub fn block_break_ack(&mut self, data: &[u8]) -> io::Result<(Position, i32, Option<Vec<u8>>)> {
        let mut r = Reader::new(data);
        let position = r.position()?;
        let state = r.varint()?;
        let action = r.varint()?;
        r.bool()?; // Whether the backend agreed: the state says what is there.
        r.finish()?;
        let sequence = self
            .pending
            .iter()
            .find(|(_, awaiting)| *awaiting == Awaiting::Dig(position, action))
            .map(|(sequence, _)| *sequence);
        Ok((
            position,
            state,
            sequence.and_then(|sequence| self.acknowledge(sequence)),
        ))
    }

    /// A 1.18.2 block update at `position` went to the client: the
    /// acknowledgement it completes, if any.
    pub fn block_updated(&mut self, position: Position) -> Option<Vec<u8>> {
        let sequence = self
            .pending
            .iter()
            .find(|(_, awaiting)| *awaiting == Awaiting::Update(position))
            .map(|(sequence, _)| *sequence)?;
        self.acknowledge(sequence)
    }
}

/// For a 1.18.2 client on a 1.19 backend: digging actions the client waits
/// to see acknowledged.
#[derive(Debug, Default)]
pub struct Digging {
    pending: VecDeque<(Position, i32)>,
}

/// A 1.18.2 `block_break_ack` that agrees with the state the client was
/// just told.
fn break_ack(position: Position, state: i32, action: i32) -> Vec<u8> {
    let mut w = Writer::new();
    w.position(position).varint(state).varint(action).bool(true);
    w.buf
}

impl Digging {
    /// A 1.18.2 serverbound `player_action`: the 1.19 packet, and an
    /// acknowledgement for the client now, if any. A started dig changes
    /// nothing on the client, so it is acknowledged at once (the client
    /// then ignores the state); stopping or aborting waits for the block
    /// update that says what is there.
    pub fn player_action(&mut self, data: &[u8]) -> io::Result<(Vec<u8>, Option<Vec<u8>>)> {
        let mut r = Reader::new(data);
        let action = r.varint()?;
        let position = r.position()?;
        let face = r.u8()?;
        r.finish()?;
        let mut w = Writer::new();
        // The 1.19 backend only echoes sequence numbers; 0 asks for none.
        w.varint(action).position(position).u8(face).varint(0);
        let ack = match action {
            START_DIGGING => Some(break_ack(position, 0, action)),
            ABORT_DIGGING | STOP_DIGGING => {
                if !self.pending.contains(&(position, action)) {
                    if self.pending.len() >= MAX_PENDING {
                        self.pending.pop_front();
                    }
                    self.pending.push_back((position, action));
                }
                None
            }
            _ => None,
        };
        Ok((w.buf, ack))
    }

    /// A block update with the client's (1.18.2) `state` at `position` went
    /// to the client: acknowledgements for the digging there.
    pub fn block_updated(&mut self, position: Position, state: i32, out: &mut Vec<Vec<u8>>) {
        self.pending.retain(|(pending, action)| {
            if *pending == position {
                out.push(break_ack(position, state, *action));
                false
            } else {
                true
            }
        });
    }

    pub fn is_waiting(&self) -> bool {
        !self.pending.is_empty()
    }
}

/// The positions and states of a 1.19 `block_update`.
pub fn block_update(data: &[u8]) -> io::Result<(Position, i32)> {
    let mut r = Reader::new(data);
    let update = (r.position()?, r.varint()?);
    r.finish()?;
    Ok(update)
}

/// The positions and states of a 1.18.2 or 1.19 `section_blocks_update`.
pub fn section_blocks_update(data: &[u8]) -> io::Result<Vec<(Position, i32)>> {
    let mut r = Reader::new(data);
    let section = r.i64()?;
    let (x, y, z) = (
        (section >> 42) as i32,
        ((section << 44) >> 44) as i32,
        ((section << 22) >> 42) as i32,
    );
    r.bool()?;
    let updates = (0..r.count()?)
        .map(|_| {
            let entry = r.varlong()?;
            let local = (entry & 0xfff) as i32;
            Ok((
                Position {
                    x: x * 16 + (local >> 8 & 15),
                    y: y * 16 + (local & 15),
                    z: z * 16 + (local >> 4 & 15),
                },
                (entry >> 12) as i32,
            ))
        })
        .collect::<io::Result<_>>()?;
    r.finish()?;
    Ok(updates)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(write: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut writer = Writer::new();
        write(&mut writer);
        writer.buf
    }

    const AT: Position = Position {
        x: 10,
        y: -60,
        z: -3,
    };

    #[test]
    fn digging_is_acknowledged_when_the_backend_answers() {
        let mut sequences = Sequences::default();
        let dig = |action: i32, sequence: i32| {
            bytes(|w| {
                w.varint(action).position(AT).u8(1).varint(sequence);
            })
        };
        let (old, ack) = sequences.player_action(&dig(START_DIGGING, 3)).unwrap();
        assert_eq!(
            old,
            bytes(|w| {
                w.varint(0).position(AT).u8(1);
            })
        );
        assert_eq!(ack, None);
        let (_, ack) = sequences.player_action(&dig(STOP_DIGGING, 4)).unwrap();
        assert_eq!(ack, None);
        // The stop is answered first: it acknowledges the start too.
        let answer = bytes(|w| {
            w.position(AT).varint(0).varint(STOP_DIGGING).bool(true);
        });
        assert_eq!(
            sequences.block_break_ack(&answer).unwrap(),
            (AT, 0, Some(vec![4]))
        );
        let answer = bytes(|w| {
            w.position(AT).varint(1).varint(START_DIGGING).bool(true);
        });
        assert_eq!(sequences.block_break_ack(&answer).unwrap(), (AT, 1, None));
        // Dropping an item predicts nothing but carries a sequence anyway.
        let (_, ack) = sequences.player_action(&dig(4, 5)).unwrap();
        assert_eq!(ack, Some(vec![5]));
        assert!(sequences.player_action(&dig(0, -1)).is_err());
    }

    #[test]
    fn using_an_item_on_a_block_waits_for_the_neighbour_update() {
        let mut sequences = Sequences::default();
        let use_on = bytes(|w| {
            w.varint(0)
                .position(AT)
                .varint(1)
                .f32(0.5)
                .f32(1.0)
                .f32(0.5)
                .bool(false)
                .varint(7);
        });
        let (old, ack) = sequences.use_item_on(&use_on).unwrap();
        assert_eq!(old, use_on[..use_on.len() - 1]);
        assert_eq!(ack, None);
        assert_eq!(sequences.block_updated(AT), None);
        assert_eq!(
            sequences.block_updated(Position { y: -59, ..AT }),
            Some(vec![7])
        );
        assert_eq!(
            sequences.use_item(&[1, 8]).unwrap(),
            (vec![1], Some(vec![8]))
        );
        assert_eq!(sequences.use_item(&[1, 0]).unwrap(), (vec![1], None));
        // Unanswered predictions are acknowledged before they pile up.
        let mut sequences = Sequences::default();
        let mut acks = 0;
        for sequence in 1..=MAX_PENDING as i32 + 1 {
            let mut data = use_on.clone();
            *data.last_mut().unwrap() = sequence as u8;
            acks += usize::from(sequences.use_item_on(&data).unwrap().1.is_some());
        }
        assert_eq!(acks, 1);
    }

    #[test]
    fn older_clients_see_their_digging_acknowledged() {
        let mut digging = Digging::default();
        let dig = |action: i32| {
            bytes(|w| {
                w.varint(action).position(AT).u8(1);
            })
        };
        let (new, ack) = digging.player_action(&dig(START_DIGGING)).unwrap();
        assert_eq!(new, [&dig(START_DIGGING)[..], &[0]].concat());
        assert_eq!(ack, Some(break_ack(AT, 0, START_DIGGING)));
        assert_eq!(digging.player_action(&dig(STOP_DIGGING)).unwrap().1, None);
        assert_eq!(digging.player_action(&dig(STOP_DIGGING)).unwrap().1, None);
        let mut out = Vec::new();
        digging.block_updated(Position { x: 0, ..AT }, 0, &mut out);
        assert!(out.is_empty() && digging.is_waiting());
        digging.block_updated(AT, 0, &mut out);
        assert_eq!(out, vec![break_ack(AT, 0, STOP_DIGGING)]);
        assert!(!digging.is_waiting());
    }

    #[test]
    fn section_updates_name_their_blocks() {
        // A 1.19 server's update of section (0, -4, -1).
        let data = bytes(|w| {
            w.i64((0x3f_ffff << 20) | (-4i64 & 0xf_ffff)) // x 0, z -1, y -4.
                .bool(false)
                .varint(1)
                .varlong((9 << 12) | (0xa << 8) | (0xd << 4) | 4);
        });
        assert_eq!(
            section_blocks_update(&data).unwrap(),
            vec![(
                Position {
                    x: 10,
                    y: -60,
                    z: -3
                },
                9
            )]
        );
    }
}
