//! `explode` in all its layouts:
//!
//! - to 1.16.5: float position and radius, an int count of byte offsets of
//!   the destroyed blocks, float player knockback;
//! - 1.17: the count is a VarInt;
//! - 1.19.3: the position is double;
//! - 1.20.3: then the block interaction (keep, destroy, destroy with decay,
//!   trigger), small and large particles and a sound (written out: a
//!   sound event name and an optional fixed range);
//! - 1.20.5: the sound is a registry holder;
//! - 1.21.2: position, optional double knockback, one particle, sound;
//! - 1.21.9: radius and block count are back (an int count, not a list),
//!   followed by a weighted list of block particles;
//! - 26.3: a flag whether to play the sound.
//!
//! Clients before 1.21.2 remove the listed blocks themselves; going up the
//! removals are sent as block updates, going down the list stays empty and
//! the server's own block updates do the work.
use super::packets::write_position;
use crate::{
    Direction, Packet, State as Phase,
    codec::{HasShared, Shared, entity::particle},
    data,
    step::Cx,
    wire::{Position, Reader, Rewriter, Writer, invalid},
};
use std::io;

#[derive(Clone, Debug, Default, PartialEq)]
struct Explosion {
    x: f64,
    y: f64,
    z: f64,
    radius: f32,
    blocks: Vec<[i8; 3]>,
    block_count: i32,
    knockback: Option<[f64; 3]>,
    interaction: i32,
    /// Particles and sounds already in the receiver's terms (or `None`).
    small: Option<Vec<u8>>,
    large: Option<Vec<u8>>,
    sound: Option<Vec<u8>>,
    block_particles: Option<Vec<u8>>,
    play_sound: bool,
}

/// One particle, converted by the entity codec, as bytes; `None` when the
/// receiver has no such particle.
fn particle(shared: &mut Shared, reader: &mut Reader<'_>) -> io::Result<Option<Vec<u8>>> {
    let mut rw = Rewriter::new(reader.peek());
    let known = particle::transcode(shared, Direction::Clientbound, &mut rw)?;
    let used = reader.remaining() - rw.r.remaining();
    reader.take(used)?;
    Ok(known.then_some(rw.w.buf))
}

/// The explosion sound in the receiver's terms: inline (a sound event name
/// and an optional fixed range) in 1.20.3/1.20.4, whose packet writes the
/// sound event itself; a holder (registry ID + 1, or 0 and an inline sound
/// event) from 1.20.5. A sound the receiver lacks is sent inline by name.
fn sound(shared: &mut Shared, reader: &mut Reader<'_>) -> io::Result<Vec<u8>> {
    let (source, target) = (shared.backend, shared.client);
    let id = if source == 765 { 0 } else { reader.varint()? };
    if id == 0 {
        let name = reader.string(32767)?;
        let range = if reader.bool()? {
            Some(reader.f32()?)
        } else {
            None
        };
        return Ok(inline_sound(target, name, range));
    }
    let id = id - 1;
    if target != 765
        && let Some(mapped) = match super::packets::ids(source, target, "sound_event")? {
            None => Some(id),
            Some(mapping) => mapping.get(id),
        }
    {
        let mut writer = Writer::new();
        writer.varint(mapped + 1);
        return Ok(writer.buf);
    }
    let name = data::version(source)
        .registry("sound_event")
        .and_then(|registry| registry.name(id))
        .ok_or_else(|| invalid("unknown sound event"))?;
    Ok(inline_sound(target, &format!("minecraft:{name}"), None))
}

/// A sound event in `protocol`'s explosion layout: by registry ID when
/// the layout has holders and the registry has it without a fixed range,
/// as a server of that version sends a vanilla sound, else written out.
fn inline_sound(protocol: i32, name: &str, range: Option<f32>) -> Vec<u8> {
    let mut writer = Writer::new();
    if protocol != 765 {
        let id = data::version(protocol)
            .registry("sound_event")
            .and_then(|registry| registry.id(crate::data::strip(name)));
        if let (Some(id), None) = (id, range) {
            writer.varint(id + 1);
            return writer.buf;
        }
        writer.varint(0);
    }
    writer.string(name);
    match range {
        Some(range) => writer.bool(true).f32(range),
        None => writer.bool(false),
    };
    writer.buf
}

fn read(shared: &mut Shared, data: &[u8]) -> io::Result<Explosion> {
    let source = shared.backend;
    let mut reader = Reader::new(data);
    let mut explosion = Explosion {
        play_sound: true,
        ..Explosion::default()
    };
    if source >= 761 {
        explosion.x = reader.f64()?;
        explosion.y = reader.f64()?;
        explosion.z = reader.f64()?;
    } else {
        explosion.x = f64::from(reader.f32()?);
        explosion.y = f64::from(reader.f32()?);
        explosion.z = f64::from(reader.f32()?);
    }
    if !(768..773).contains(&source) {
        explosion.radius = reader.f32()?;
    }
    if source < 768 {
        let count = if source >= 755 {
            reader.count()?
        } else {
            let count = reader.i32()?;
            usize::try_from(count)
                .ok()
                .filter(|count| count.saturating_mul(3) <= reader.remaining())
                .ok_or_else(|| invalid("invalid block count"))?
        };
        for _ in 0..count {
            explosion
                .blocks
                .push([reader.i8()?, reader.i8()?, reader.i8()?]);
        }
        let knockback = [
            f64::from(reader.f32()?),
            f64::from(reader.f32()?),
            f64::from(reader.f32()?),
        ];
        explosion.knockback = Some(knockback);
        if source >= 765 {
            explosion.interaction = reader.varint()?;
            explosion.small = particle(shared, &mut reader)?;
            explosion.large = particle(shared, &mut reader)?;
            explosion.sound = Some(sound(shared, &mut reader)?);
        }
    } else {
        if source >= 773 {
            explosion.block_count = reader.i32()?;
        }
        if reader.bool()? {
            explosion.knockback = Some([reader.f64()?, reader.f64()?, reader.f64()?]);
        }
        explosion.large = particle(shared, &mut reader)?;
        explosion.sound = Some(sound(shared, &mut reader)?);
        if source >= 773 {
            let mut list = Writer::new();
            let count = reader.count()?;
            let mut kept = Writer::new();
            let mut written = 0;
            for _ in 0..count {
                let particle = particle(shared, &mut reader)?;
                let (scaling, speed, weight) = (reader.f32()?, reader.f32()?, reader.varint()?);
                if let Some(particle) = particle {
                    kept.bytes(&particle).f32(scaling).f32(speed).varint(weight);
                    written += 1;
                }
            }
            list.varint(written).bytes(&kept.buf);
            explosion.block_particles = Some(list.buf);
        }
        if source >= 777 {
            explosion.play_sound = reader.bool()?;
        }
    }
    reader.finish()?;
    Ok(explosion)
}

/// A particle without options, by name, in `protocol`'s particle registry.
fn named_particle(protocol: i32, name: &str) -> Option<Vec<u8>> {
    let id = data::version(protocol)
        .registry("particle_type")?
        .id(name)?;
    let mut writer = Writer::new();
    writer.varint(id);
    Some(writer.buf)
}

fn explode_sound(protocol: i32) -> io::Result<Vec<u8>> {
    if protocol == 765 {
        return Ok(inline_sound(
            protocol,
            "minecraft:entity.generic.explode",
            None,
        ));
    }
    let id = data::version(protocol)
        .registry("sound_event")
        .and_then(|registry| registry.id("entity.generic.explode"))
        .ok_or_else(|| invalid("no explosion sound"))?;
    let mut writer = Writer::new();
    writer.varint(id + 1);
    Ok(writer.buf)
}

fn write(explosion: &Explosion, protocol: i32) -> io::Result<Vec<u8>> {
    let mut writer = Writer::with_capacity(64 + explosion.blocks.len() * 3);
    if protocol >= 761 {
        writer.f64(explosion.x).f64(explosion.y).f64(explosion.z);
    } else {
        writer
            .f32(explosion.x as f32)
            .f32(explosion.y as f32)
            .f32(explosion.z as f32);
    }
    if !(768..773).contains(&protocol) {
        writer.f32(explosion.radius);
    }
    let missing = || invalid("explosion particle has no counterpart");
    if protocol < 768 {
        if protocol >= 755 {
            writer.varint(explosion.blocks.len() as i32);
        } else {
            writer.i32(explosion.blocks.len() as i32);
        }
        for block in &explosion.blocks {
            writer.i8(block[0]).i8(block[1]).i8(block[2]);
        }
        let knockback = explosion.knockback.unwrap_or_default();
        writer
            .f32(knockback[0] as f32)
            .f32(knockback[1] as f32)
            .f32(knockback[2] as f32);
        if protocol >= 765 {
            writer.varint(explosion.interaction);
            writer.bytes(explosion.small.as_deref().ok_or_else(missing)?);
            writer.bytes(explosion.large.as_deref().ok_or_else(missing)?);
            writer.bytes(explosion.sound.as_deref().ok_or_else(missing)?);
        }
    } else {
        if protocol >= 773 {
            writer.i32(explosion.block_count);
        }
        match explosion.knockback {
            Some([x, y, z]) => writer.bool(true).f64(x).f64(y).f64(z),
            None => writer.bool(false),
        };
        writer.bytes(explosion.large.as_deref().ok_or_else(missing)?);
        writer.bytes(explosion.sound.as_deref().ok_or_else(missing)?);
        if protocol >= 773 {
            writer.bytes(explosion.block_particles.as_deref().unwrap_or(&[0]));
        }
        if protocol >= 777 {
            writer.bool(explosion.play_sound);
        }
    }
    Ok(writer.buf)
}

/// `explode`.
pub fn explode<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: Phase,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let (source, target) = (shared.backend, shared.client);
    let mut explosion = read(shared, &packet.data)?;
    let mut removed = Vec::new();
    if source < 765 && target >= 765 {
        // Older explosions always break their blocks and look the same.
        explosion.interaction = 1;
        explosion.small = named_particle(target, "explosion");
        explosion.large = named_particle(target, "explosion_emitter");
        explosion.sound = Some(explode_sound(target)?);
    }
    if source >= 765 && target < 765 && matches!(explosion.interaction, 0 | 3) {
        explosion.blocks.clear(); // Older clients would break them.
    }
    if source < 768 && target >= 768 {
        if matches!(explosion.interaction, 1 | 2) {
            let center = [
                explosion.x.floor(),
                explosion.y.floor(),
                explosion.z.floor(),
            ];
            removed = explosion
                .blocks
                .iter()
                .map(|[x, y, z]| Position {
                    x: center[0] as i32 + i32::from(*x),
                    y: center[1] as i32 + i32::from(*y),
                    z: center[2] as i32 + i32::from(*z),
                })
                .collect();
        }
        if explosion.radius < 2.0 || explosion.interaction == 0 {
            explosion.large = explosion.small.take();
        }
        if explosion.knockback == Some([0.0; 3]) {
            explosion.knockback = None;
        }
        explosion.blocks.clear();
    }
    if source >= 768 && target < 768 {
        // The one particle shows whatever the radius and interaction.
        explosion.radius = 4.0;
        explosion.interaction = 0;
        explosion.small = explosion.large.clone();
    }
    if source < 773 && target >= 773 {
        explosion.block_count = 0;
        explosion.block_particles = None;
    }
    cx.to_client(state, Packet::new(packet.id, write(&explosion, target)?));
    if !removed.is_empty() {
        let id = data::version(target)
            .packet_id(Phase::Play, Direction::Clientbound, "block_update")
            .ok_or_else(|| invalid("no block update packet"))?;
        for position in removed {
            let mut writer = Writer::with_capacity(9);
            write_position(&mut writer, target, position);
            writer.varint(0); // Air.
            cx.to_client(state, Packet::new(id, writer.buf));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn particle_free_layouts_round_trip() {
        let explosion = Explosion {
            x: 1.5,
            y: 64.0,
            z: -3.25,
            radius: 4.0,
            blocks: vec![[1, -1, 0], [0, 0, 2]],
            knockback: Some([0.5, 0.25, 0.0]),
            play_sound: true,
            ..Explosion::default()
        };
        for protocol in [340, 754, 755, 760, 761, 764] {
            let bytes = write(&explosion, protocol).unwrap();
            let mut shared = Shared::new(protocol, protocol);
            assert_eq!(read(&mut shared, &bytes).unwrap(), explosion, "{protocol}");
        }
    }

    /// 1.20.3/1.20.4 write the sound event itself; 1.20.2 has none and
    /// 1.20.5 a holder.
    #[test]
    fn the_1_20_3_sound_is_written_out() {
        use crate::codec::testing::Defaults;
        let mut body = Writer::new();
        body.f64(0.5).f64(64.0).f64(0.5).f32(4.0).varint(0);
        body.f32(0.0).f32(0.0).f32(0.0);
        let older = body.buf.clone();
        let mut up = Defaults::new(765, 764);
        let out = up.clientbound(Phase::Play, "explode", &older).unwrap();
        let mut r = Reader::new(&out[0].data);
        r.take(older.len()).unwrap();
        assert_eq!(r.varint().unwrap(), 1); // Destroy.
        let particles = data::version(765).expect_registry("particle_type");
        assert_eq!(r.varint().unwrap(), particles.id("explosion").unwrap());
        assert_eq!(
            r.varint().unwrap(),
            particles.id("explosion_emitter").unwrap()
        );
        assert_eq!(r.str().unwrap(), "minecraft:entity.generic.explode");
        assert!(!r.bool().unwrap());
        r.finish().unwrap();

        let mut up = Defaults::new(766, 765);
        let out = up
            .clientbound(Phase::Play, "explode", &out[0].data)
            .unwrap();
        let mut r = Reader::new(&out[0].data);
        r.take(older.len() + 3).unwrap();
        let sounds = data::version(766).expect_registry("sound_event");
        assert_eq!(
            r.varint().unwrap(),
            sounds.id("entity.generic.explode").unwrap() + 1
        );
        r.finish().unwrap();

        let mut down = Defaults::new(765, 766);
        let back = down
            .clientbound(Phase::Play, "explode", &out[0].data)
            .unwrap();
        let mut r = Reader::new(&back[0].data);
        r.take(older.len() + 3).unwrap();
        assert_eq!(r.str().unwrap(), "minecraft:entity.generic.explode");
        assert!(!r.bool().unwrap());
        r.finish().unwrap();
    }
}
