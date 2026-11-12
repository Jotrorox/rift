//! Spawning entities between 1.18.2 and 1.19. 1.19 spawns mobs and
//! paintings with `add_entity` (which gains the head rotation and a VarInt
//! data field) instead of `add_mob` and `add_painting`, sends a painting's
//! variant in its entity data, and shows vibrations as particles instead of
//! `add_vibration_signal`.
use crate::{
    data,
    mapping::Mapping,
    wire::{Position, Reader, Writer, invalid},
};
use std::io;

/// Entity types 1.18.2 spawns with `add_entity` (or `add_painting` and
/// `add_experience_orb`): everything that is not a living entity.
const NOT_LIVING: &[&str] = &[
    "area_effect_cloud",
    "arrow",
    "boat",
    "chest_minecart",
    "command_block_minecart",
    "dragon_fireball",
    "egg",
    "end_crystal",
    "ender_pearl",
    "evoker_fangs",
    "experience_bottle",
    "experience_orb",
    "eye_of_ender",
    "falling_block",
    "fireball",
    "firework_rocket",
    "fishing_bobber",
    "furnace_minecart",
    "glow_item_frame",
    "hopper_minecart",
    "item",
    "item_frame",
    "leash_knot",
    "lightning_bolt",
    "llama_spit",
    "marker",
    "minecart",
    "painting",
    "potion",
    "shulker_bullet",
    "small_fireball",
    "snowball",
    "spawner_minecart",
    "spectral_arrow",
    "tnt",
    "tnt_minecart",
    "trident",
    "wither_skull",
];

/// The 1.19 `painting_variant` entity data serializer.
const PAINTING_VARIANT: i32 = 22;
/// A painting's variant is entity data index 8.
const VARIANT_INDEX: u8 = 8;

fn entity_name(protocol: i32, id: i32) -> Option<&'static str> {
    data::version(protocol).registry("entity_type")?.name(id)
}

fn entity_id(protocol: i32, name: &str) -> i32 {
    data::version(protocol)
        .expect_registry("entity_type")
        .id(name)
        .unwrap_or_else(|| panic!("protocol {protocol} has no {name} entity"))
}

/// The common start of every spawn packet.
struct Spawn {
    entity: i32,
    uuid: u128,
    kind: i32,
    x: f64,
    y: f64,
    z: f64,
}

impl Spawn {
    fn read(r: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self {
            entity: r.varint()?,
            uuid: r.uuid()?,
            kind: r.varint()?,
            x: r.f64()?,
            y: r.f64()?,
            z: r.f64()?,
        })
    }

    fn write(&self, kind: i32, w: &mut Writer) {
        w.varint(self.entity)
            .uuid(self.uuid)
            .varint(kind)
            .f64(self.x)
            .f64(self.y)
            .f64(self.z);
    }
}

/// What a 1.19 `add_entity` becomes for 1.18.2.
pub enum Spawned {
    /// `add_entity` or `add_mob`, by name.
    Packet(&'static str, Vec<u8>),
    /// A painting, whose variant follows in its entity data.
    Painting(Painting),
    /// An entity type 1.18.2 has no stand-in for.
    Dropped,
}

/// A painting 1.18.2 can only spawn once it knows the variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Painting {
    pub entity: i32,
    pub uuid: u128,
    pub position: Position,
    /// 1.18.2's horizontal direction: 0 south, 1 west, 2 north, 3 east.
    pub direction: i8,
}

impl Painting {
    /// 1.18.2's `add_painting` with a 1.18.2 variant (motive) ID.
    pub fn write(&self, motive: i32) -> Vec<u8> {
        let mut w = Writer::new();
        w.varint(self.entity)
            .uuid(self.uuid)
            .varint(motive)
            .position(self.position)
            .i8(self.direction);
        w.buf
    }
}

/// 1.19 3D direction to 1.18.2's horizontal one; paintings never face up
/// or down.
fn horizontal(direction: i32) -> i8 {
    match direction {
        3 => 0,
        4 => 1,
        2 => 2,
        5 => 3,
        _ => 0,
    }
}

fn three_d(direction: i8) -> io::Result<i32> {
    Ok(match direction {
        0 => 3,
        1 => 4,
        2 => 2,
        3 => 5,
        _ => return Err(invalid("invalid painting direction")),
    })
}

/// 1.19 `add_entity` for 1.18.2. `entities` and `blocks` map 1.19 entity
/// types and block states (falling blocks' data) to 1.18.2's.
pub fn add_entity_down(data: &[u8], entities: &Mapping, blocks: &Mapping) -> io::Result<Spawned> {
    let mut r = Reader::new(data);
    let spawn = Spawn::read(&mut r)?;
    let (pitch, yaw, head_yaw) = (r.i8()?, r.i8()?, r.i8()?);
    let mut data = r.varint()?;
    let velocity = r.take(6)?;
    r.finish()?;
    let Some(kind) = entities.get(spawn.kind) else {
        return Ok(Spawned::Dropped);
    };
    let name = entity_name(758, kind).ok_or_else(|| invalid("unknown entity type"))?;
    let mut w = Writer::with_capacity(64);
    if name == "painting" {
        return Ok(Spawned::Painting(Painting {
            entity: spawn.entity,
            uuid: spawn.uuid,
            position: Position {
                x: spawn.x.floor() as i32,
                y: spawn.y.floor() as i32,
                z: spawn.z.floor() as i32,
            },
            direction: horizontal(data),
        }));
    }
    spawn.write(kind, &mut w);
    if NOT_LIVING.contains(&name) {
        if name == "falling_block" {
            data = blocks.get_or(data, 0);
        }
        w.i8(pitch).i8(yaw).i32(data).bytes(velocity);
        Ok(Spawned::Packet("add_entity", w.buf))
    } else {
        w.i8(yaw).i8(pitch).i8(head_yaw).bytes(velocity);
        Ok(Spawned::Packet("add_mob", w.buf))
    }
}

/// 1.18.2 `add_entity` for 1.19: the head faces where the body does.
pub fn add_entity_up(
    data: &[u8],
    entities: &Mapping,
    blocks: &Mapping,
) -> io::Result<Option<Vec<u8>>> {
    let mut r = Reader::new(data);
    let spawn = Spawn::read(&mut r)?;
    let (pitch, yaw) = (r.i8()?, r.i8()?);
    let mut data = r.i32()?;
    let velocity = r.take(6)?;
    r.finish()?;
    let Some(kind) = entities.get(spawn.kind) else {
        return Ok(None);
    };
    if entity_name(759, kind) == Some("falling_block") {
        data = blocks.get_or(data, 0);
    }
    let mut w = Writer::with_capacity(64);
    spawn.write(kind, &mut w);
    w.i8(pitch).i8(yaw).i8(yaw).varint(data).bytes(velocity);
    Ok(Some(w.buf))
}

/// 1.18.2 `add_mob` for 1.19's `add_entity`.
pub fn add_mob_up(data: &[u8], entities: &Mapping) -> io::Result<Option<Vec<u8>>> {
    let mut r = Reader::new(data);
    let spawn = Spawn::read(&mut r)?;
    let (yaw, pitch, head_yaw) = (r.i8()?, r.i8()?, r.i8()?);
    let velocity = r.take(6)?;
    r.finish()?;
    let Some(kind) = entities.get(spawn.kind) else {
        return Ok(None);
    };
    let mut w = Writer::with_capacity(64);
    spawn.write(kind, &mut w);
    w.i8(pitch).i8(yaw).i8(head_yaw).varint(0).bytes(velocity);
    Ok(Some(w.buf))
}

/// 1.18.2 `add_painting` for 1.19: `add_entity` and the entity data that
/// carries the variant. `variants` maps 1.18.2 motives to 1.19 variants.
pub fn add_painting_up(data: &[u8], variants: &Mapping) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut r = Reader::new(data);
    let entity = r.varint()?;
    let uuid = r.uuid()?;
    let motive = r.varint()?;
    let position = r.position()?;
    let direction = three_d(r.i8()?)?;
    r.finish()?;
    let mut spawn = Writer::with_capacity(64);
    Spawn {
        entity,
        uuid,
        kind: 0,
        x: f64::from(position.x),
        y: f64::from(position.y),
        z: f64::from(position.z),
    }
    .write(entity_id(759, "painting"), &mut spawn);
    spawn.i8(0).i8(0).i8(0).varint(direction).bytes(&[0; 6]);
    let mut entity_data = Writer::new();
    entity_data
        .varint(entity)
        .u8(VARIANT_INDEX)
        .varint(PAINTING_VARIANT)
        .varint(variants.get_or(motive, 0))
        .u8(0xff);
    Ok((spawn.buf, entity_data.buf))
}

/// The painting variant in a 1.19 `set_entity_data` for `entity`, if the
/// packet is about that entity and names one. Entries this scan cannot
/// skip (item stacks, particles) end it without a variant.
pub fn painting_variant(data: &[u8], entity: i32) -> Option<i32> {
    let mut r = Reader::new(data);
    if r.varint().ok()? != entity {
        return None;
    }
    loop {
        let index = r.u8().ok()?;
        if index == 0xff {
            return None;
        }
        let serializer = r.varint().ok()?;
        if index == VARIANT_INDEX && serializer == PAINTING_VARIANT {
            return r.varint().ok();
        }
        skip_entity_data(&mut r, serializer).ok()?;
    }
}

fn skip_entity_data(r: &mut Reader<'_>, serializer: i32) -> io::Result<()> {
    match serializer {
        0 => {
            r.u8()?;
        }
        1 | 11 | 13 | 18 | 19 | 20 | 22 => {
            r.varint()?;
        }
        2 => {
            r.f32()?;
        }
        3 | 4 => {
            r.str()?;
        }
        5 => {
            if r.bool()? {
                r.str()?;
            }
        }
        7 => {
            r.bool()?;
        }
        8 => {
            r.take(12)?;
        }
        9 => {
            r.i64()?;
        }
        10 => {
            if r.bool()? {
                r.i64()?;
            }
        }
        12 => {
            if r.bool()? {
                r.uuid()?;
            }
        }
        14 => {
            r.nbt_bytes(true)?;
        }
        16 => {
            r.varint()?;
            r.varint()?;
            r.varint()?;
        }
        17 => {
            r.varint()?;
        }
        21 => {
            if r.bool()? {
                r.str()?;
                r.i64()?;
            }
        }
        _ => return Err(invalid("entity data the painting scan cannot skip")),
    }
    Ok(())
}

/// 1.18.2's `add_vibration_signal` as the 1.19 vibration particle it shows:
/// from the centre of the origin block to a block or an entity.
pub fn vibration_up(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut r = Reader::new(data);
    let origin = r.position()?;
    let source = r.str()?;
    let mut destination = Writer::new();
    destination.string(source);
    match source.strip_prefix("minecraft:").unwrap_or(source) {
        "block" => {
            destination.i64(r.i64()?);
        }
        "entity" => {
            destination.varint(r.varint()?).f32(0.0);
        }
        _ => return Err(invalid("unknown vibration destination")),
    }
    destination.varint(r.varint()?); // Arrival ticks.
    r.finish()?;
    let particle = data::version(759)
        .expect_registry("particle_type")
        .id("vibration")
        .ok_or_else(|| invalid("no vibration particle"))?;
    let mut w = Writer::with_capacity(64);
    w.varint(particle)
        .bool(false)
        .f64(f64::from(origin.x) + 0.5)
        .f64(f64::from(origin.y) + 0.5)
        .f64(f64::from(origin.z) + 0.5)
        .f32(0.0)
        .f32(0.0)
        .f32(0.0)
        .f32(0.0)
        .i32(1)
        .bytes(&destination.buf);
    Ok(w.buf)
}
