//! The entity codec's other packets: `set_entity_data`, `remove_entities`,
//! `level_particles`, `update_mob_effect`, `remove_mob_effect` and
//! `update_attributes`.
//!
//! `damage_event` (1.19.4+) keeps its layout; its damage type is an ID into
//! the data-driven `damage_type` registry the backend synchronises, which
//! the registries codec owns, so no handler is installed for it.
// INTEGRATION: should the registries codec ever renumber synchronised
// registry entries, `damage_event`'s damage type needs a handler mapping it.
use super::{
    common, emit, metadata,
    particle::{self, Options, Particle},
    tables, tracks,
};
use crate::{
    Direction, Packet, State as Phase,
    codec::{HasShared, Shared},
    data,
    step::{Cx, RouteBuilder},
    version,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::io;

fn crosses(from: i32, to: i32, boundaries: &[i32]) -> bool {
    let (low, high) = (from.min(to), from.max(to));
    boundaries
        .iter()
        .any(|boundary| low < *boundary && *boundary <= high)
}

// Entity data.

pub(super) fn set_entity_data<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    _: Phase,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let mut rw = Rewriter::new(&packet.data);
    let id = rw.varint()?;
    let tracked = shared.entity.get(id);
    if shared.target(direction) < 759 && shared.source(direction) >= 759 {
        repaint(shared, cx, id, rw.r)?;
    }
    if shared.target(direction) >= 768 && shared.source(direction) < 768 {
        reboat(shared, cx, id, rw.r)?;
    }
    let tracked = shared.entity.get(id).or(tracked);
    metadata::transcode(
        shared,
        direction,
        tracked.map(|tracked| tracked.backend),
        tracked.map(|tracked| tracked.client),
        &mut rw,
    )?;
    let body = rw.finish()?;
    // Nothing left but the entity ID and the end marker.
    if body.len() > 1 + varint_len(id) {
        emit(cx, shared, direction, "set_entity_data", body)?;
    }
    Ok(())
}

fn varint_len(value: i32) -> usize {
    let mut writer = Writer::new();
    writer.varint(value);
    writer.len()
}

/// Before 1.19 a painting's variant is part of its spawn packet. When 1.19+
/// entity data names the variant of a painting sent with a placeholder,
/// spawn it again with the right one.
fn repaint(
    shared: &mut Shared,
    cx: &mut Cx<'_>,
    id: i32,
    mut reader: Reader<'_>,
) -> io::Result<()> {
    let Some((position, facing)) = shared.entity.paintings.get(&id).copied() else {
        return Ok(());
    };
    let (from, to) = (shared.backend, shared.client);
    let source = tables::tables(from);
    let fields = source.fields("painting").unwrap_or_default();
    let mut variant = None;
    loop {
        let index = reader.u8()?;
        if index == 0xff {
            break;
        }
        let kind = source
            .serializer(reader.varint()?)
            .ok_or_else(|| invalid("unknown entity data serializer"))?;
        let field = fields.get(usize::from(index));
        if field.is_some_and(|field| {
            field.key == "Painting.DATA_PAINTING_VARIANT_ID" && field.kind == kind
        }) && kind == tables::Kind::PaintingVariant
        {
            variant = Some(reader.varint()?);
        } else {
            metadata::skip_value(from, kind, &mut reader)?;
        }
    }
    let Some(variant) = variant else {
        return Ok(());
    };
    let name = data::version(from)
        .registry("painting_variant")
        .and_then(|registry| registry.name(variant));
    let motive = name
        .and_then(|name| data::version(to).registry("painting_variant")?.id(name))
        .unwrap_or(0);
    let mut remove = Writer::new();
    remove.varint(1).varint(id);
    emit(
        cx,
        shared,
        Direction::Clientbound,
        "remove_entities",
        remove.buf,
    )?;
    let mut spawn = Writer::new();
    // The UUID is not known here; the client does not use a painting's.
    spawn.varint(id).uuid(0).varint(motive);
    common::write_position(to, position, &mut spawn);
    spawn.u8(super::spawn::facing_to_2d(facing) as u8);
    emit(
        cx,
        shared,
        Direction::Clientbound,
        "add_painting",
        spawn.buf,
    )?;
    Ok(())
}

/// The value of the entity data field `key` in a list of entity `kind`, if
/// the list sets it with an Int serializer.
fn find_int(
    protocol: i32,
    kind: &str,
    key: &str,
    mut reader: Reader<'_>,
) -> io::Result<Option<i32>> {
    let source = tables::tables(protocol);
    let fields = source.fields(kind).unwrap_or_default();
    let mut found = None;
    loop {
        let index = reader.u8()?;
        if index == 0xff {
            return Ok(found);
        }
        let serializer = source
            .serializer(reader.varint()?)
            .ok_or_else(|| invalid("unknown entity data serializer"))?;
        let field = fields.get(usize::from(index));
        if serializer == tables::Kind::Int
            && field.is_some_and(|field| field.key == key && field.kind == serializer)
        {
            found = Some(reader.varint()?);
        } else {
            metadata::skip_value(protocol, serializer, &mut reader)?;
        }
    }
}

/// From 1.21.2 each wood's boat is its own entity type. When pre-1.21.2
/// entity data names a boat's wood, spawn it again as the right type.
fn reboat(shared: &mut Shared, cx: &mut Cx<'_>, id: i32, reader: Reader<'_>) -> io::Result<()> {
    let Some(tracked) = shared.entity.get(id) else {
        return Ok(());
    };
    if !shared.entity.boats.contains_key(&id) {
        return Ok(());
    }
    let (from, to) = (shared.backend, shared.client);
    let Some(wood) = find_int(from, tracked.backend, "AbstractBoat.DATA_ID_TYPE", reader)? else {
        return Ok(());
    };
    let wood = super::BOAT_WOODS
        .get(usize::try_from(wood).unwrap_or(0))
        .copied()
        .unwrap_or("oak");
    let name = super::boat_type(wood, tracked.backend == "chest_boat");
    let registry = data::version(to).expect_registry("entity_type");
    let Some((kind, client)) = registry
        .id(&name)
        .and_then(|kind| Some((kind, registry.name(kind)?)))
    else {
        return Ok(());
    };
    if client == tracked.client {
        return Ok(());
    }
    let body = shared.entity.boats.get(&id).expect("checked above");
    let mut reader = Reader::new(body);
    let mut spawn = Writer::new();
    spawn.varint(reader.varint()?).uuid(reader.uuid()?);
    reader.varint()?;
    spawn.varint(kind).bytes(reader.rest());
    let spawn = spawn.buf;
    let mut remove = Writer::new();
    remove.varint(1).varint(id);
    emit(
        cx,
        shared,
        Direction::Clientbound,
        "remove_entities",
        remove.buf,
    )?;
    emit(
        cx,
        shared,
        Direction::Clientbound,
        "add_entity",
        spawn.clone(),
    )?;
    shared.entity.boats.insert(id, spawn);
    shared.entity.track(
        id,
        super::Tracked {
            backend: tracked.backend,
            client,
        },
    );
    Ok(())
}

// Removal.

fn read_ids(protocol: i32, name: &str, body: &[u8]) -> io::Result<Vec<i32>> {
    let mut reader = Reader::new(body);
    let ids = if name == "remove_entity" {
        vec![reader.varint()?]
    } else {
        let count = reader.count()?;
        (0..count)
            .map(|_| reader.varint())
            .collect::<io::Result<_>>()?
    };
    let _ = protocol;
    reader.finish()?;
    Ok(ids)
}

fn removed<S: HasShared>(step: &mut S, name: &str, packet: &Packet) -> io::Result<Vec<i32>> {
    let shared = step.shared();
    let ids = read_ids(shared.backend, name, &packet.data)?;
    for id in &ids {
        shared.entity.untrack(*id);
    }
    Ok(ids)
}

fn remove<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    name: &str,
    packet: Packet,
) -> io::Result<()> {
    let ids = removed(step, name, &packet)?;
    let shared = step.shared();
    let direction = Direction::Clientbound;
    // 1.17.0 alone removes one entity per packet.
    if shared.target(direction) == 755 {
        for id in ids {
            let mut body = Writer::new();
            body.varint(id);
            emit(cx, shared, direction, "remove_entity", body.buf)?;
        }
    } else {
        let mut body = Writer::new();
        body.varint(ids.len() as i32);
        for id in ids {
            body.varint(id);
        }
        emit(cx, shared, direction, "remove_entities", body.buf)?;
    }
    Ok(())
}

pub(super) fn remove_entities<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    _: Phase,
    packet: Packet,
) -> io::Result<()> {
    remove(step, cx, "remove_entities", packet)
}

pub(super) fn remove_entity<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    _: Phase,
    packet: Packet,
) -> io::Result<()> {
    remove(step, cx, "remove_entity", packet)
}

fn tap_remove_entities<S: HasShared>(step: &mut S, _: Phase, packet: &Packet) -> io::Result<()> {
    removed(step, "remove_entities", packet).map(drop)
}

fn tap_remove_entity<S: HasShared>(step: &mut S, _: Phase, packet: &Packet) -> io::Result<()> {
    removed(step, "remove_entity", packet).map(drop)
}

// Particles.

/// The particle of a `level_particles` packet.
#[derive(Clone, Debug, PartialEq)]
enum Sent {
    /// 1.12: a numeric ID and its int data.
    Legacy(i32, Vec<i32>),
    Modern(Particle),
}

#[derive(Clone, Debug, PartialEq)]
struct LevelParticles {
    particle: Sent,
    override_limiter: bool,
    always_show: bool,
    position: [f64; 3],
    offset: [f32; 3],
    /// One maximum speed before 26.3, one per axis from then on.
    speed: [f32; 3],
    count: i32,
    /// 26.3: how offsets and speeds are randomised; 0 is the old way.
    randomization: i32,
}

fn read_level_particles(protocol: i32, body: &[u8]) -> io::Result<LevelParticles> {
    let mut r = Reader::new(body);
    // 26.3 sends the particle first.
    let first = if protocol >= 777 {
        Some(particle::read(protocol, &mut r)?)
    } else {
        None
    };
    let id = if protocol < 766 {
        Some(if protocol < 759 {
            r.i32()?
        } else {
            r.varint()?
        })
    } else {
        None
    };
    let override_limiter = r.bool()?;
    let always_show = protocol >= 769 && r.bool()?;
    let position = if protocol < 573 {
        [r.f32()?, r.f32()?, r.f32()?].map(f64::from)
    } else {
        [r.f64()?, r.f64()?, r.f64()?]
    };
    let offset = [r.f32()?, r.f32()?, r.f32()?];
    let (speed, count, randomization) = if protocol >= 777 {
        let speed = [r.f32()?, r.f32()?, r.f32()?];
        (speed, r.varint()?, r.varint()?)
    } else {
        let speed = r.f32()?;
        ([speed; 3], r.i32()?, 0)
    };
    let particle = match id {
        _ if first.is_some() => Sent::Modern(first.expect("read first")),
        Some(id) if protocol < 393 => {
            let data = (0..particle::legacy_data_len(id))
                .map(|_| r.varint())
                .collect::<io::Result<_>>()?;
            Sent::Legacy(id, data)
        }
        Some(id) => Sent::Modern(Particle {
            id,
            options: particle::read_options(protocol, id, &mut r)?,
        }),
        None => Sent::Modern(particle::read(protocol, &mut r)?),
    };
    r.finish()?;
    Ok(LevelParticles {
        particle,
        override_limiter,
        always_show,
        position,
        offset,
        speed,
        count,
        randomization,
    })
}

fn write_level_particles(protocol: i32, packet: &LevelParticles) -> io::Result<Vec<u8>> {
    let mut w = Writer::new();
    match &packet.particle {
        Sent::Legacy(id, _) => drop(w.i32(*id)),
        Sent::Modern(particle) if protocol < 759 => drop(w.i32(particle.id)),
        Sent::Modern(particle) if protocol < 766 => drop(w.varint(particle.id)),
        Sent::Modern(particle) if protocol >= 777 => particle::write(protocol, particle, &mut w)?,
        Sent::Modern(_) => {}
    }
    w.bool(packet.override_limiter);
    if protocol >= 769 {
        w.bool(packet.always_show);
    }
    for axis in packet.position {
        if protocol < 573 {
            w.f32(axis as f32);
        } else {
            w.f64(axis);
        }
    }
    for axis in packet.offset {
        w.f32(axis);
    }
    if protocol >= 777 {
        for axis in packet.speed {
            w.f32(axis);
        }
        w.varint(packet.count.max(0)).varint(packet.randomization);
    } else {
        let [x, y, z] = packet.speed;
        w.f32(if x == y && y == z { x } else { x.max(y).max(z) })
            .i32(packet.count);
    }
    match &packet.particle {
        Sent::Legacy(_, data) => {
            for value in data {
                w.varint(*value);
            }
        }
        Sent::Modern(particle) if protocol < 766 => {
            let mut options = Writer::new();
            particle::write(protocol, particle, &mut options)?;
            // The ID went first; the options follow the other fields.
            let mut reader = Reader::new(&options.buf);
            reader.varint()?;
            w.bytes(reader.rest());
        }
        Sent::Modern(_) if protocol >= 777 => {}
        Sent::Modern(particle) => particle::write(protocol, particle, &mut w)?,
    }
    Ok(w.buf)
}

pub(super) fn level_particles<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    _: Phase,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let (from, to) = (shared.source(direction), shared.target(direction));
    let mut sent = read_level_particles(from, &packet.data)?;
    let particle = match std::mem::replace(&mut sent.particle, Sent::Legacy(0, Vec::new())) {
        Sent::Legacy(id, data) if to < 393 => Some(Sent::Legacy(id, data)),
        Sent::Legacy(id, data) => {
            // 1.12 colours redstone dust with the offsets.
            let color = sent.offset;
            let upgraded = particle::upgrade_legacy(shared, direction, id, &data, color)?;
            if matches!(
                upgraded,
                Some(Particle {
                    options: Options::Dust { .. },
                    ..
                })
            ) && sent.count == 0
            {
                sent.offset = [0.0; 3];
            }
            upgraded.map(Sent::Modern)
        }
        Sent::Modern(particle) if to < 393 => {
            particle::downgrade_legacy(shared, direction, &particle)?.map(|(id, data, color)| {
                if let Some(color) = color {
                    sent.offset = color;
                    sent.count = 0;
                    sent.speed = [1.0; 3];
                }
                Sent::Legacy(id, data)
            })
        }
        Sent::Modern(particle) => {
            let effect = from < 766 && to >= 766 && is_named(from, particle.id, "entity_effect");
            let converted = particle::convert(shared, direction, particle)?;
            converted.map(|mut particle| {
                // Before 1.20.5 an entity effect's colour is its offset.
                if effect && let Options::Color(color) = &mut particle.options {
                    let [r, g, b] = sent
                        .offset
                        .map(|channel| (channel.clamp(0.0, 1.0) * 255.0).round() as i32);
                    *color = (0xff << 24) | (r << 16) | (g << 8) | b;
                    if sent.count == 0 {
                        sent.offset = [0.0; 3];
                    }
                }
                Sent::Modern(particle)
            })
        }
    };
    let Some(particle) = particle else {
        return Ok(());
    };
    sent.particle = particle;
    let body = write_level_particles(to, &sent)?;
    emit(cx, shared, direction, "level_particles", body)
}

fn is_named(protocol: i32, id: i32, name: &str) -> bool {
    data::version(protocol)
        .expect_registry("particle_type")
        .name(id)
        == Some(name)
}

// Mob effects.

pub(super) fn update_mob_effect<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    _: Phase,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let (from, to) = (shared.source(direction), shared.target(direction));
    let mut r = Reader::new(&packet.data);
    let entity = r.varint()?;
    let effect = if from < 758 {
        i32::from(r.u8()?)
    } else {
        r.varint()?
    };
    let amplifier = if from < 766 {
        i32::from(r.u8()?)
    } else {
        r.varint()?
    };
    let duration = r.varint()?;
    let flags = r.u8()?;
    let mut factor = None;
    if (759..766).contains(&from) && r.bool()? {
        factor = Some(r.nbt_bytes(!version::nameless_nbt(from))?);
    }
    r.finish()?;
    let Some(effect) = shared.mapping(direction, "mob_effect").get(effect) else {
        return Ok(());
    };
    let mut w = Writer::new();
    w.varint(entity);
    if to < 758 {
        w.u8(effect as u8);
    } else {
        w.varint(effect);
    }
    if to < 766 {
        w.u8(amplifier.clamp(0, 255) as u8);
    } else {
        w.varint(amplifier);
    }
    w.varint(duration);
    w.u8(if to < 766 { flags & 7 } else { flags });
    if (759..766).contains(&to) {
        match factor {
            Some(tag) => {
                w.bool(true);
                common::convert_nbt(from, to, &mut Reader::new(tag), &mut w)?;
            }
            None => drop(w.bool(false)),
        }
    }
    emit(cx, shared, direction, "update_mob_effect", w.buf)
}

pub(super) fn remove_mob_effect<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    _: Phase,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let (from, to) = (shared.source(direction), shared.target(direction));
    let mut r = Reader::new(&packet.data);
    let entity = r.varint()?;
    let effect = if from < 758 {
        i32::from(r.u8()?)
    } else {
        r.varint()?
    };
    r.finish()?;
    let Some(effect) = shared.mapping(direction, "mob_effect").get(effect) else {
        return Ok(());
    };
    let mut w = Writer::new();
    w.varint(entity);
    if to < 758 {
        w.u8(effect as u8);
    } else {
        w.varint(effect);
    }
    emit(cx, shared, direction, "remove_mob_effect", w.buf)
}

// Attributes.

/// Pre-1.16 attribute keys with their newest names.
const LEGACY_ATTRIBUTES: &[(&str, &str)] = &[
    ("generic.maxHealth", "max_health"),
    ("generic.followRange", "follow_range"),
    ("generic.knockbackResistance", "knockback_resistance"),
    ("generic.movementSpeed", "movement_speed"),
    ("generic.attackDamage", "attack_damage"),
    ("generic.attackSpeed", "attack_speed"),
    ("generic.armor", "armor"),
    ("generic.armorToughness", "armor_toughness"),
    ("generic.luck", "luck"),
    ("generic.flyingSpeed", "flying_speed"),
    ("horse.jumpStrength", "jump_strength"),
    ("zombie.spawnReinforcements", "spawn_reinforcements"),
];

/// An attribute's newest name: no namespace, no `generic.`, `player.`,
/// `horse.` or `zombie.` prefix (1.21.2 dropped them).
pub(super) fn canonical(key: &str) -> String {
    let key = data::strip(key);
    if let Some((_, name)) = LEGACY_ATTRIBUTES.iter().find(|(legacy, _)| *legacy == key) {
        return (*name).to_owned();
    }
    key.split_once('.')
        .filter(|(prefix, _)| matches!(*prefix, "generic" | "player" | "horse" | "zombie"))
        .map_or(key, |(_, name)| name)
        .to_owned()
}

/// The attribute's registry name in `protocol` (1.16+), or its key.
fn registry_name(protocol: i32, name: &str) -> Option<&'static str> {
    let registry = data::version(protocol).registry("attribute")?;
    [
        name.to_owned(),
        format!("generic.{name}"),
        format!("player.{name}"),
        format!("zombie.{name}"),
        format!("horse.{name}"),
    ]
    .iter()
    .find_map(|candidate| registry.id(candidate))
    .and_then(|id| registry.name(id))
}

/// Modifier UUIDs (before 1.21) Mojang gave identifiers in 1.21.
const MODIFIERS: &[(u128, &str)] = &[
    (
        0x662a6b8d_da3e_4c1c_8813_96ea6097278d,
        "minecraft:sprinting",
    ),
    (
        0xcb3f55d3_645c_4f38_a497_9c13a33db5cf,
        "minecraft:base_attack_damage",
    ),
    (
        0xfa233e1c_4180_4865_b01b_bcce9785aca3,
        "minecraft:base_attack_speed",
    ),
    (
        0x1eaf83ff_7207_4596_b37a_d7a07b3ec4ce,
        "minecraft:powder_snow",
    ),
    (0x766bfa64_11f3_11ea_8d71_362b9e155667, "minecraft:baby"),
    (0x020e0dfb_87ae_4653_9556_831010e291a0, "minecraft:covered"),
    (
        0x49455a49_7ec5_45ba_b886_3b90b23a1718,
        "minecraft:attacking",
    ),
];

/// A modifier identifier for a pre-1.21 UUID.
pub(super) fn modifier_id(uuid: u128) -> String {
    MODIFIERS
        .iter()
        .find(|(known, _)| *known == uuid)
        .map_or_else(
            || format!("rift:uuid_{uuid:032x}"),
            |(_, id)| (*id).to_owned(),
        )
}

/// A pre-1.21 modifier UUID for an identifier: the UUID it came from, or one
/// derived from the identifier so that the same modifier keeps one UUID.
pub(super) fn modifier_uuid(id: &str) -> u128 {
    if let Some((uuid, _)) = MODIFIERS.iter().find(|(_, known)| *known == id) {
        return *uuid;
    }
    if let Some(hex) = id.strip_prefix("rift:uuid_")
        && let Ok(uuid) = u128::from_str_radix(hex, 16)
    {
        return uuid;
    }
    // FNV-1a over the identifier, twice with different seeds.
    let hash = |seed: u64| {
        id.bytes().fold(seed, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    };
    let uuid =
        (u128::from(hash(0xcbf2_9ce4_8422_2325)) << 64) | u128::from(hash(0x8422_2325_cbf2_9ce4));
    // A version 3 (name-based) UUID.
    (uuid & !(0xf000 << 64) & !(0xc000 << 48)) | (0x3000 << 64) | (0x8000 << 48)
}

struct Modifier {
    uuid: u128,
    id: Option<String>,
    amount: f64,
    operation: u8,
}

pub(super) fn update_attributes<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    _: Phase,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let (from, to) = (shared.source(direction), shared.target(direction));
    let mut r = Reader::new(&packet.data);
    let entity = r.varint()?;
    let count = if from < 755 {
        let count = r.i32()?;
        usize::try_from(count)
            .ok()
            .filter(|count| *count <= r.remaining())
            .ok_or_else(|| invalid("invalid attribute count"))?
    } else {
        r.count()?
    };
    let source_registry = data::version(from).registry("attribute");
    let mut attributes = Vec::with_capacity(count);
    for _ in 0..count {
        let key = if from >= 766 {
            let id = r.varint()?;
            source_registry
                .and_then(|registry| registry.name(id))
                .ok_or_else(|| invalid("unknown attribute"))?
                .to_owned()
        } else {
            r.str()?.to_owned()
        };
        let value = r.f64()?;
        let mut modifiers = Vec::new();
        for _ in 0..r.count()? {
            let (uuid, id) = if from >= 767 {
                let id = r.str()?.to_owned();
                (modifier_uuid(&id), Some(id))
            } else {
                (r.uuid()?, None)
            };
            modifiers.push(Modifier {
                uuid,
                id,
                amount: r.f64()?,
                operation: r.u8()?,
            });
        }
        attributes.push((canonical(&key), value, modifiers));
    }
    r.finish()?;
    let mut w = Writer::new();
    w.varint(entity);
    let mut written = Writer::new();
    let mut kept = 0;
    for (name, value, modifiers) in &attributes {
        if to >= 766 {
            let Some(id) = registry_name(to, name)
                .and_then(|name| data::version(to).expect_registry("attribute").id(name))
            else {
                continue;
            };
            written.varint(id);
        } else if to >= 735 {
            let Some(name) = registry_name(to, name) else {
                continue;
            };
            written.string(&format!("minecraft:{name}"));
        } else {
            let Some((legacy, _)) = LEGACY_ATTRIBUTES.iter().find(|(_, known)| known == name)
            else {
                continue;
            };
            written.string(legacy);
        }
        written.f64(*value).varint(modifiers.len() as i32);
        for modifier in modifiers {
            if to >= 767 {
                let id = modifier
                    .id
                    .clone()
                    .unwrap_or_else(|| modifier_id(modifier.uuid));
                written.string(&id);
            } else {
                written.uuid(modifier.uuid);
            }
            written.f64(modifier.amount).u8(modifier.operation);
        }
        kept += 1;
    }
    if to < 755 {
        w.i32(kept);
    } else {
        w.varint(kept);
    }
    w.bytes(&written.buf);
    emit(cx, shared, direction, "update_attributes", w.buf)
}

// Installation.

type Handler<S> = fn(&mut S, &mut Cx<'_>, Phase, Packet) -> io::Result<()>;

pub fn install<S: HasShared + 'static>(
    builder: RouteBuilder<S>,
    client: i32,
    backend: i32,
) -> RouteBuilder<S> {
    let (from, to) = (backend, client);
    let mut builder = builder;
    let handle = |builder: RouteBuilder<S>, name: &'static str, handler: Handler<S>| {
        builder.default_handler(Direction::Clientbound, Phase::Play, name, handler)
    };
    if metadata::differs(from, to) {
        builder = handle(builder, "set_entity_data", set_entity_data::<S>);
    }
    if crosses(from, to, &[755, 756]) {
        builder = handle(builder, "remove_entities", remove_entities::<S>);
        builder = handle(builder, "remove_entity", remove_entity::<S>);
    } else if tracks(client, backend) {
        builder = builder
            .tap(
                Direction::Clientbound,
                Phase::Play,
                "remove_entities",
                tap_remove_entities::<S>,
            )
            .tap(
                Direction::Clientbound,
                Phase::Play,
                "remove_entity",
                tap_remove_entity::<S>,
            );
    }
    if crosses(from, to, &[393, 573, 759, 766, 769, 777]) || !particle::unchanged(from, to) {
        builder = handle(builder, "level_particles", level_particles::<S>);
    }
    if crosses(from, to, &[758, 759, 764, 766])
        || !common::registry_identity(from, to, "mob_effect")
    {
        builder = handle(builder, "update_mob_effect", update_mob_effect::<S>);
        builder = handle(builder, "remove_mob_effect", remove_mob_effect::<S>);
    }
    if crosses(from, to, &[735, 755, 766, 767, 768])
        || !common::registry_identity(from, to, "attribute")
    {
        builder = handle(builder, "update_attributes", update_attributes::<S>);
    }
    builder
}
