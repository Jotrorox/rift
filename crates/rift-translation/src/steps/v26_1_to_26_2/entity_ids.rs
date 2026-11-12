//! Entity ID 0 for 26.2 clients.
//!
//! A 26.2 client takes entity ID 0 for "not assigned yet" and fails on an
//! entity that has it (`Entity.getId`: "Tried to access entity ID before ID
//! assignment"), in Join Game for the player itself. Servers from 1.14 on
//! number entities from 1; 1.12 and 1.13 servers from 0, so their first
//! entity, often the first player to join, has ID 0. Going up, that entity
//! is renumbered to [`FAKE`] wherever a packet names an entity, and back in
//! what the client sends. Entity references inside entity data (a firework's
//! user, a fishing hook's catch, a guardian's target) keep 0: the client
//! only misses that link.
use crate::wire::{Rewriter, Writer};
use std::io;

/// Negative, so no server's own numbering reaches it, and far from -1 so
/// that `ID + 1` encodings stay distinct from "none".
pub const FAKE: i32 = -0x2000_0000;

/// Clientbound packets that start with an entity ID as a VarInt.
const LEADING: &[&str] = &[
    "add_entity",
    "animate",
    "block_destruction",
    "debug/entity_value",
    "entity_position_sync",
    "hurt_animation",
    "move_entity_pos",
    "move_entity_pos_rot",
    "move_entity_rot",
    "move_minecart_along_track",
    "player_combat_kill",
    "projectile_power",
    "remove_mob_effect",
    "rotate_head",
    "set_camera",
    "set_entity_data",
    "set_entity_motion",
    "set_equipment",
    "teleport_entity",
    "update_attributes",
    "update_mob_effect",
];

/// Clientbound packets that carry entity IDs elsewhere.
const OTHERS: &[&str] = &[
    "damage_event",
    "entity_event",
    "login",
    "mount_screen_open",
    "player_look_at",
    "remove_entities",
    "set_entity_link",
    "set_passengers",
    "sound_entity",
    "take_item_entity",
];

/// Serverbound packets that start with an entity ID as a VarInt.
const SERVERBOUND: &[&str] = &[
    "attack",
    "interact",
    "pick_item_from_entity",
    "player_command",
    "set_command_minecart",
];

/// Every clientbound packet [`clientbound`] renumbers in.
pub fn clientbound_packets() -> impl Iterator<Item = &'static str> {
    LEADING.iter().chain(OTHERS).copied()
}

/// Every serverbound packet [`serverbound`] renumbers in, besides
/// `entity_tag_query` and `spectator_action`.
pub fn serverbound_packets() -> impl Iterator<Item = &'static str> {
    SERVERBOUND
        .iter()
        .copied()
        .chain(["entity_tag_query", "spectator_action"])
}

fn up(id: i32) -> i32 {
    if id == 0 { FAKE } else { id }
}

fn down(id: i32) -> i32 {
    if id == FAKE { 0 } else { id }
}

/// An `ID + 1` field (0 is none).
fn up_plus_one(value: i32) -> i32 {
    if value == 1 { FAKE + 1 } else { value }
}

fn varint(rw: &mut Rewriter<'_>, map: fn(i32) -> i32) -> io::Result<()> {
    let id = rw.r.varint()?;
    rw.w.varint(map(id));
    Ok(())
}

fn int(rw: &mut Rewriter<'_>, map: fn(i32) -> i32) -> io::Result<()> {
    let id = rw.r.i32()?;
    rw.w.i32(map(id));
    Ok(())
}

/// Renumber entity 0 in a clientbound 26.1 packet. `None` when the packet
/// names no entity 0, which is nearly always: the packet is left as it is.
pub fn clientbound(name: &str, body: &[u8]) -> io::Result<Option<Vec<u8>>> {
    if LEADING.contains(&name) {
        // A VarInt 0 is the single byte 0.
        if body.first() != Some(&0) {
            return Ok(None);
        }
        let mut w = Writer::with_capacity(body.len() + 4);
        w.varint(FAKE).bytes(&body[1..]);
        return Ok(Some(w.into_inner()));
    }
    let mut rw = Rewriter::new(body);
    match name {
        "login" | "entity_event" => int(&mut rw, up)?,
        "damage_event" => {
            varint(&mut rw, up)?;
            rw.r.varint().map(|kind| rw.w.varint(kind))?;
            varint(&mut rw, up_plus_one)?;
            varint(&mut rw, up_plus_one)?;
        }
        "mount_screen_open" => {
            rw.r.varint().map(|window| rw.w.varint(window))?;
            rw.r.varint().map(|slots| rw.w.varint(slots))?;
            int(&mut rw, up)?;
        }
        "player_look_at" => {
            rw.r.varint().map(|anchor| rw.w.varint(anchor))?;
            let position = rw.r.take(24)?;
            rw.w.bytes(position);
            let entity = rw.r.bool()?;
            rw.w.bool(entity);
            if entity {
                varint(&mut rw, up)?;
            }
        }
        "remove_entities" => {
            let count = rw.r.count()?;
            rw.w.varint(count as i32);
            for _ in 0..count {
                varint(&mut rw, up)?;
            }
        }
        // The holder is 0 when the leash is dropped, as on every server.
        "set_entity_link" => int(&mut rw, up)?,
        "set_passengers" => {
            varint(&mut rw, up)?;
            let count = rw.r.count()?;
            rw.w.varint(count as i32);
            for _ in 0..count {
                varint(&mut rw, up)?;
            }
        }
        "sound_entity" => {
            // A registered sound as ID + 1, or 0 and an inline event.
            let sound = rw.r.varint()?;
            rw.w.varint(sound);
            if sound == 0 {
                let name = rw.r.string(32767)?;
                rw.w.string(name);
                let range = rw.r.bool()?;
                rw.w.bool(range);
                if range {
                    let range = rw.r.f32()?;
                    rw.w.f32(range);
                }
            }
            rw.r.varint().map(|source| rw.w.varint(source))?;
            varint(&mut rw, up)?;
        }
        "take_item_entity" => {
            varint(&mut rw, up)?;
            varint(&mut rw, up)?;
        }
        _ => return Ok(None),
    }
    rw.copy_rest();
    let out = rw.finish()?;
    Ok((out != body).then_some(out))
}

/// Renumber [`FAKE`] back to 0 in a serverbound 26.1 packet.
pub fn serverbound(name: &str, body: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let mut rw = Rewriter::new(body);
    match name {
        name if SERVERBOUND.contains(&name) => varint(&mut rw, down)?,
        "entity_tag_query" => {
            rw.r.varint().map(|transaction| rw.w.varint(transaction))?;
            varint(&mut rw, down)?;
        }
        _ => return Ok(None),
    }
    rw.copy_rest();
    let out = rw.finish()?;
    Ok((out != body).then_some(out))
}

/// The spectator target as 26.1 names it (26.2 sends `ID + 1`, 0 for none).
pub fn spectator_target(target: i32) -> i32 {
    down(target - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(write: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut writer = Writer::new();
        write(&mut writer);
        writer.into_inner()
    }

    #[test]
    fn entity_zero_is_renumbered_both_ways() {
        let join = bytes(|w| {
            w.i32(0).bool(false);
        });
        assert_eq!(
            clientbound("login", &join).unwrap().unwrap(),
            bytes(|w| {
                w.i32(FAKE).bool(false);
            })
        );
        let motion = bytes(|w| {
            w.varint(0).i16(1);
        });
        assert_eq!(
            clientbound("set_entity_motion", &motion).unwrap().unwrap(),
            bytes(|w| {
                w.varint(FAKE).i16(1);
            })
        );
        let passengers = bytes(|w| {
            w.varint(7).varint(2).varint(0).varint(9);
        });
        assert_eq!(
            clientbound("set_passengers", &passengers).unwrap().unwrap(),
            bytes(|w| {
                w.varint(7).varint(2).varint(FAKE).varint(9);
            })
        );
        let damage = bytes(|w| {
            w.varint(5).varint(3).varint(1).varint(0);
        });
        assert_eq!(
            clientbound("damage_event", &damage).unwrap().unwrap(),
            bytes(|w| {
                w.varint(5).varint(3).varint(FAKE + 1).varint(0);
            })
        );
        let sound = bytes(|w| {
            w.varint(0).string("minecraft:ui.toast").bool(false);
            w.varint(1).varint(0).f32(1.0).f32(1.0).i64(0);
        });
        let mut expected = Writer::new();
        expected
            .varint(0)
            .string("minecraft:ui.toast")
            .bool(false)
            .varint(1)
            .varint(FAKE)
            .f32(1.0)
            .f32(1.0)
            .i64(0);
        assert_eq!(
            clientbound("sound_entity", &sound).unwrap().unwrap(),
            expected.into_inner()
        );
        let command = bytes(|w| {
            w.varint(FAKE).varint(1).varint(0);
        });
        assert_eq!(
            serverbound("player_command", &command).unwrap().unwrap(),
            bytes(|w| {
                w.varint(0).varint(1).varint(0);
            })
        );
        assert_eq!(spectator_target(FAKE + 1), 0);
        assert_eq!(spectator_target(8), 7);
    }

    #[test]
    fn other_entities_are_left_alone() {
        let motion = bytes(|w| {
            w.varint(12).i16(1);
        });
        assert_eq!(clientbound("set_entity_motion", &motion).unwrap(), None);
        let join = bytes(|w| {
            w.i32(3).bool(false);
        });
        assert_eq!(clientbound("login", &join).unwrap(), None);
        let attack = bytes(|w| {
            w.varint(3);
        });
        assert_eq!(serverbound("attack", &attack).unwrap(), None);
        // A truncated packet fails rather than passing on half renumbered.
        assert!(clientbound("set_passengers", &[0, 2, 0]).is_err());
    }
}
