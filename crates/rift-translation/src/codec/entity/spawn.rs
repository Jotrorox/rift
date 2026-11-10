//! The spawn packets: `add_entity` and the older `add_mob`, `add_player`,
//! `add_painting`, `add_experience_orb` and `add_global_entity`.
//!
//! Every spawn is read into one model whatever packet carried it, then
//! written as the packet the receiver uses for that entity type: 1.14
//! unified object and mob type IDs, 1.15 moved entity data out of the spawn
//! packets, 1.16 dropped `add_global_entity`, 1.19 merged `add_mob` and
//! `add_painting` into `add_entity` (the painting's variant moved to entity
//! data), 1.20.2 merged `add_player`, 1.21.5 merged `add_experience_orb`
//! (its value moved to entity data), 1.21.6 put hanging entities' facing in
//! entity data and 1.21.9 packed velocity as an `LpVec3`.
use super::{
    Tracked, common, emit, map_type, metadata,
    tables::{self, Kind},
    tracks, type_name,
};
use crate::{
    Direction, Packet, State as Phase,
    codec::{HasShared, Shared},
    data,
    step::{Cx, RouteBuilder},
    version,
    wire::{Position, Reader, Rewriter, Writer, invalid},
};
use std::io;

/// The 1.12 and 1.13 object types `add_entity` uses, with their 1.13 entity
/// type names. Minecarts share one object type; their kind is the data.
const OBJECTS: &[(i8, &str)] = &[
    (1, "boat"),
    (2, "item"),
    (3, "area_effect_cloud"),
    (10, "minecart"),
    (50, "tnt"),
    (51, "end_crystal"),
    (60, "arrow"),
    (61, "snowball"),
    (62, "egg"),
    (63, "fireball"),
    (64, "small_fireball"),
    (65, "ender_pearl"),
    (66, "wither_skull"),
    (67, "shulker_bullet"),
    (68, "llama_spit"),
    (70, "falling_block"),
    (71, "item_frame"),
    (72, "eye_of_ender"),
    (73, "potion"),
    (75, "experience_bottle"),
    (76, "firework_rocket"),
    (77, "leash_knot"),
    (78, "armor_stand"),
    (79, "evoker_fangs"),
    (90, "fishing_bobber"),
    (91, "spectral_arrow"),
    (93, "dragon_fireball"),
    (94, "trident"),
];

/// Minecart kinds by object data, as 1.13 names them.
const MINECARTS: &[&str] = &[
    "minecart",
    "chest_minecart",
    "furnace_minecart",
    "tnt_minecart",
    "spawner_minecart",
    "hopper_minecart",
    "command_block_minecart",
];

/// A 1.13 entity type name in `protocol`'s terms (1.12's older names).
fn object_name(protocol: i32, modern: &'static str) -> &'static str {
    if protocol >= 393 {
        if modern == "command_block_minecart"
            && data::version(protocol)
                .expect_registry("entity_type")
                .id(modern)
                .is_none()
        {
            return "commandblock_minecart";
        }
        return modern;
    }
    tables::tables(393)
        .renamed
        .iter()
        .find(|(new, _)| *new == modern)
        .map_or(modern, |(_, old)| *old)
}

fn modern_object_name(protocol: i32, name: &str) -> &str {
    if protocol >= 393 {
        return if name == "commandblock_minecart" {
            "command_block_minecart"
        } else {
            name
        };
    }
    tables::tables(393)
        .renamed
        .iter()
        .find(|(_, old)| *old == name)
        .map_or(name, |(new, _)| *new)
}

/// 1.12 painting titles by motive, in the 1.13 motive order.
const MOTIVES: &[(&str, &str)] = &[
    ("Kebab", "kebab"),
    ("Aztec", "aztec"),
    ("Alban", "alban"),
    ("Aztec2", "aztec2"),
    ("Bomb", "bomb"),
    ("Plant", "plant"),
    ("Wasteland", "wasteland"),
    ("Pool", "pool"),
    ("Courbet", "courbet"),
    ("Sea", "sea"),
    ("Sunset", "sunset"),
    ("Creebet", "creebet"),
    ("Wanderer", "wanderer"),
    ("Graham", "graham"),
    ("Match", "match"),
    ("Bust", "bust"),
    ("Stage", "stage"),
    ("Void", "void"),
    ("SkullAndRoses", "skull_and_roses"),
    ("Wither", "wither"),
    ("Fighters", "fighters"),
    ("Pointer", "pointer"),
    ("Pig", "pigscene"),
    ("BurningSkull", "burning_skull"),
    ("Skeleton", "skeleton"),
    ("DonkeyKong", "donkey_kong"),
];

/// A painting motive's ID in `protocol` (1.13 to 1.20.6), by name.
fn motive_id(protocol: i32, name: &str) -> Option<i32> {
    match data::version(protocol).registry("painting_variant") {
        Some(registry) => registry.id(name),
        None => MOTIVES
            .iter()
            .position(|(_, known)| *known == name)
            .map(|id| id as i32),
    }
}

fn motive_name(protocol: i32, id: i32) -> Option<&'static str> {
    match data::version(protocol).registry("painting_variant") {
        Some(registry) => registry.name(id),
        None => MOTIVES
            .get(usize::try_from(id).ok()?)
            .map(|(_, name)| *name),
    }
}

/// Horizontal facings by 2D index (south, west, north, east) as 3D IDs.
const HORIZONTAL: [i32; 4] = [3, 4, 2, 5];

fn horizontal_to_3d(index: i32) -> i32 {
    HORIZONTAL[index.rem_euclid(4) as usize]
}

pub(super) fn facing_to_2d(id: i32) -> i32 {
    HORIZONTAL
        .iter()
        .position(|known| *known == id)
        .unwrap_or(0) as i32
}

/// Velocity as the pre-1.21.9 packets send it: 1/8000 block per tick.
fn legacy_velocity(velocity: [f64; 3]) -> [i16; 3] {
    velocity.map(|axis| (axis * 8000.0).round().clamp(-32768.0, 32767.0) as i16)
}

/// Read a 1.21.9 low-precision vector.
pub fn read_lp_vec3(reader: &mut Reader<'_>) -> io::Result<[f64; 3]> {
    let first = reader.u8()?;
    if first == 0 {
        return Ok([0.0; 3]);
    }
    let second = reader.u8()?;
    let rest = u64::from(reader.i32()? as u32);
    let packed = (rest << 16) | (u64::from(second) << 8) | u64::from(first);
    let mut scale = u64::from(first & 3);
    if first & 4 != 0 {
        scale |= (u64::from(reader.varint()? as u32)) << 2;
    }
    let unpack = |value: u64| ((value & 32767) as f64).min(32766.0) * 2.0 / 32766.0 - 1.0;
    let scale = scale as f64;
    Ok([
        unpack(packed >> 3) * scale,
        unpack(packed >> 18) * scale,
        unpack(packed >> 33) * scale,
    ])
}

/// Write a 1.21.9 low-precision vector, as the game does.
pub fn write_lp_vec3(vector: [f64; 3], writer: &mut Writer) {
    let vector = vector.map(|axis| {
        if axis.is_nan() {
            0.0
        } else {
            axis.clamp(-1.717_986_918_3e10, 1.717_986_918_3e10)
        }
    });
    let largest = vector.iter().fold(0.0f64, |max, axis| max.max(axis.abs()));
    if largest < 3.051_944_088_384_301e-5 {
        writer.u8(0);
        return;
    }
    let scale = largest.ceil() as i64;
    let continued = scale & 3 != scale;
    let flags = if continued { (scale & 3) | 4 } else { scale } as u64;
    let pack = |value: f64| ((value * 0.5 + 0.5) * 32766.0).round() as i64 as u64;
    let packed = flags
        | (pack(vector[0] / scale as f64) << 3)
        | (pack(vector[1] / scale as f64) << 18)
        | (pack(vector[2] / scale as f64) << 33);
    writer
        .u8(packed as u8)
        .u8((packed >> 8) as u8)
        .i32((packed >> 16) as u32 as i32);
    if continued {
        writer.varint((scale >> 2) as i32);
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Extra {
    None,
    /// A painting from `add_painting`: its motive name, block and 3D facing.
    Painting {
        motive: Option<&'static str>,
        position: Position,
        facing: i32,
    },
    /// An experience orb's value from `add_experience_orb`.
    Orb(i16),
}

/// One spawned entity in the sender's terms.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Spawn {
    pub(super) id: i32,
    uuid: u128,
    /// The type name in the sender's protocol.
    pub(super) kind: &'static str,
    position: [f64; 3],
    pitch: i8,
    yaw: i8,
    head_yaw: i8,
    data: i32,
    velocity: [f64; 3],
    /// The entity data list of a pre-1.15 `add_mob` or `add_player`, raw.
    pub(super) metadata: Option<Vec<u8>>,
    extra: Extra,
}

impl Spawn {
    fn new(id: i32, kind: &'static str) -> Self {
        Self {
            id,
            uuid: 0,
            kind,
            position: [0.0; 3],
            pitch: 0,
            yaw: 0,
            head_yaw: 0,
            data: 0,
            velocity: [0.0; 3],
            metadata: None,
            extra: Extra::None,
        }
    }
}

fn read_xyz(reader: &mut Reader<'_>) -> io::Result<[f64; 3]> {
    Ok([reader.f64()?, reader.f64()?, reader.f64()?])
}

fn read_velocity(reader: &mut Reader<'_>) -> io::Result<[f64; 3]> {
    Ok([reader.i16()?, reader.i16()?, reader.i16()?].map(|axis| f64::from(axis) / 8000.0))
}

/// Skip an entity data list, returning its raw bytes.
fn read_metadata(protocol: i32, reader: &mut Reader<'_>) -> io::Result<Vec<u8>> {
    let start = *reader;
    let tables = tables::tables(protocol);
    loop {
        if reader.u8()? == 0xff {
            break;
        }
        let kind = tables
            .serializer(reader.varint()?)
            .ok_or_else(|| invalid("unknown entity data serializer"))?;
        metadata::skip_value(protocol, kind, reader)?;
    }
    Ok(start.peek()[..start.remaining() - reader.remaining()].to_vec())
}

/// Read a spawn packet named `name` in `protocol`.
pub(super) fn read(protocol: i32, name: &str, body: &[u8]) -> io::Result<Spawn> {
    let mut reader = Reader::new(body);
    let r = &mut reader;
    let spawn = match name {
        "add_entity" => {
            let id = r.varint()?;
            let uuid = r.uuid()?;
            let mut spawn;
            if protocol < 477 {
                let object = r.i8()?;
                let position = read_xyz(r)?;
                let (pitch, yaw) = (r.i8()?, r.i8()?);
                let data = r.i32()?;
                let modern = match OBJECTS.iter().find(|(known, _)| *known == object) {
                    Some((10, _)) => MINECARTS
                        .get(usize::try_from(data).unwrap_or(0))
                        .copied()
                        .unwrap_or("minecart"),
                    Some((_, name)) => name,
                    None => return Err(invalid(&format!("unknown object type {object}"))),
                };
                spawn = Spawn::new(id, object_name(protocol, modern));
                spawn.position = position;
                spawn.pitch = pitch;
                spawn.yaw = yaw;
                spawn.head_yaw = yaw;
                spawn.data = data;
                spawn.velocity = read_velocity(r)?;
            } else {
                let kind = r.varint()?;
                let kind = type_name(protocol, kind)
                    .ok_or_else(|| invalid(&format!("unknown entity type {kind}")))?;
                spawn = Spawn::new(id, kind);
                spawn.position = read_xyz(r)?;
                if protocol >= 773 {
                    spawn.velocity = read_lp_vec3(r)?;
                }
                spawn.pitch = r.i8()?;
                spawn.yaw = r.i8()?;
                if protocol >= 759 {
                    spawn.head_yaw = r.i8()?;
                    spawn.data = r.varint()?;
                } else {
                    spawn.head_yaw = spawn.yaw;
                    spawn.data = r.i32()?;
                }
                if protocol < 773 {
                    spawn.velocity = read_velocity(r)?;
                }
            }
            spawn.uuid = uuid;
            spawn
        }
        "add_mob" => {
            let id = r.varint()?;
            let uuid = r.uuid()?;
            let kind = r.varint()?;
            let kind = type_name(protocol, kind)
                .ok_or_else(|| invalid(&format!("unknown entity type {kind}")))?;
            let mut spawn = Spawn::new(id, kind);
            spawn.uuid = uuid;
            spawn.position = read_xyz(r)?;
            spawn.yaw = r.i8()?;
            spawn.pitch = r.i8()?;
            spawn.head_yaw = r.i8()?;
            spawn.velocity = read_velocity(r)?;
            if protocol < 573 {
                spawn.metadata = Some(read_metadata(protocol, r)?);
            }
            spawn
        }
        "add_player" => {
            let id = r.varint()?;
            let mut spawn = Spawn::new(id, "player");
            spawn.uuid = r.uuid()?;
            spawn.position = read_xyz(r)?;
            spawn.yaw = r.i8()?;
            spawn.pitch = r.i8()?;
            spawn.head_yaw = spawn.yaw;
            if protocol < 573 {
                spawn.metadata = Some(read_metadata(protocol, r)?);
            }
            spawn
        }
        "add_painting" => {
            let id = r.varint()?;
            let uuid = r.uuid()?;
            let motive = if protocol < 393 {
                let title = r.string(13)?;
                MOTIVES
                    .iter()
                    .find(|(known, _)| *known == title)
                    .map(|(_, name)| *name)
            } else {
                motive_name(protocol, r.varint()?)
            };
            let position = common::read_position(protocol, r)?;
            let facing = horizontal_to_3d(i32::from(r.u8()?));
            let mut spawn = Spawn::new(id, "painting");
            spawn.uuid = uuid;
            spawn.position = [position.x, position.y, position.z].map(f64::from);
            spawn.data = facing;
            spawn.extra = Extra::Painting {
                motive,
                position,
                facing,
            };
            spawn
        }
        "add_experience_orb" => {
            let id = r.varint()?;
            let mut spawn = Spawn::new(id, object_name(protocol, "experience_orb"));
            spawn.position = read_xyz(r)?;
            spawn.extra = Extra::Orb(r.i16()?);
            spawn
        }
        "add_global_entity" => {
            let id = r.varint()?;
            if r.i8()? != 1 {
                return Err(invalid("unknown global entity type"));
            }
            let mut spawn = Spawn::new(id, "lightning_bolt");
            spawn.position = read_xyz(r)?;
            spawn
        }
        _ => return Err(invalid("not a spawn packet")),
    };
    reader.finish()?;
    Ok(spawn)
}

/// Whether `kind` (a type name in `protocol`) is spawned with `add_mob`
/// before 1.19: living entities, except armor stands, which 1.12 and 1.13
/// spawn as objects.
fn is_mob(protocol: i32, kind: &str) -> bool {
    tables::tables(protocol).fields(kind).is_some_and(|fields| {
        fields
            .iter()
            .any(|field| field.key == "LivingEntity.DATA_HEALTH_ID")
    }) && !(protocol < 477 && kind == "armor_stand")
}

/// The packet `protocol` spawns an entity of type `kind` with.
fn packet_for(protocol: i32, kind: &str) -> &'static str {
    match kind {
        "player" if protocol < 764 => "add_player",
        "painting" if protocol < 759 => "add_painting",
        "experience_orb" | "xp_orb" if protocol < 770 => "add_experience_orb",
        "lightning_bolt" if protocol < 735 => "add_global_entity",
        _ if protocol < 759 && is_mob(protocol, kind) => "add_mob",
        _ => "add_entity",
    }
}

/// Convert the spawn's object data, which some types give a meaning.
fn convert_data(shared: &Shared, direction: Direction, spawn: &Spawn, target: &str) -> i32 {
    let (from, to) = (shared.source(direction), shared.target(direction));
    let modern = modern_object_name(from, spawn.kind);
    match modern {
        "item_frame" | "glow_item_frame" if (from < 393) != (to < 393) => {
            if from < 393 {
                horizontal_to_3d(spawn.data)
            } else {
                facing_to_2d(spawn.data)
            }
        }
        "falling_block" if version::flattened(from) != version::flattened(to) => {
            // INTEGRATION: 1.12 numeric blocks need the legacy tables; stone
            // stands in meanwhile.
            if version::flattened(to) {
                data::version(to).block_states.parse("stone").unwrap_or(1)
            } else {
                1
            }
        }
        "falling_block" => common::block_state(shared, direction, spawn.data),
        _ if to < 477 && MINECARTS.contains(&modern_object_name(to, target)) => MINECARTS
            .iter()
            .position(|known| *known == modern_object_name(to, target))
            .unwrap_or(0)
            as i32,
        _ if from < 477 && MINECARTS.contains(&modern) => 0,
        _ => spawn.data,
    }
}

/// An entity data entry for a field the receiver has: its index and
/// serializer in `protocol` for entity type `kind`, by field key.
fn entry(protocol: i32, kind: &str, key: &str) -> Option<(u8, i32, Kind)> {
    let tables = tables::tables(protocol);
    let fields = tables.fields(kind)?;
    let index = fields.iter().position(|field| field.key == key)?;
    let field = fields[index];
    Some((index as u8, tables.serializer_id(field.kind)?, field.kind))
}

/// The packets that spawn `spawn` for the receiver, by name, and track
/// it. Empty when the receiver has no such entity and no stand-in.
pub(super) fn encode(
    shared: &mut Shared,
    direction: Direction,
    spawn: Spawn,
) -> io::Result<Vec<(&'static str, Vec<u8>)>> {
    let (from, to) = (shared.source(direction), shared.target(direction));
    let Some(target) = map_type(shared, direction, spawn.kind) else {
        return Ok(Vec::new());
    };
    let packet = packet_for(to, target);
    let mut w = Writer::new();
    // Entity data the receiver gets in a separate packet: converted from a
    // pre-1.15 spawn's list, plus fields that moved out of spawn packets.
    let mut entries = Writer::new();
    let mut list = None;
    if let Some(raw) = &spawn.metadata {
        let mut rw = Rewriter::new(raw);
        metadata::transcode(shared, direction, Some(spawn.kind), Some(target), &mut rw)?;
        let mut converted = rw.finish()?;
        converted.pop(); // The end marker.
        list = Some(converted);
    }
    let synthesize = |entries: &mut Writer, key: &str, value: &dyn Fn(&mut Writer)| {
        if let Some((index, serializer, _)) = entry(to, target, key) {
            entries.u8(index).varint(serializer);
            value(entries);
        }
    };
    match (&spawn.extra, packet) {
        (Extra::Painting { motive, .. }, "add_entity") => {
            if let Some((index, serializer, kind)) =
                entry(to, target, "Painting.DATA_PAINTING_VARIANT_ID")
                && let Some(id) = motive.and_then(|motive| motive_id(to.min(766), motive))
            {
                entries.u8(index).varint(serializer);
                // INTEGRATION: from 1.21 painting variants are data-driven;
                // their IDs follow the backend's synchronised registry.
                entries.varint(if kind == Kind::PaintingVariantHolder {
                    id + 1
                } else {
                    id
                });
            }
        }
        (Extra::Orb(value), "add_entity") => {
            synthesize(&mut entries, "ExperienceOrb.DATA_VALUE", &|w| {
                w.varint(i32::from(*value));
            });
        }
        _ => {}
    }
    if from >= 768
        && to < 768
        && let Some((wood, _)) = super::boat_wood(spawn.kind)
    {
        // Before 1.21.2 a boat's wood is entity data.
        synthesize(&mut entries, "AbstractBoat.DATA_ID_TYPE", &|w| {
            w.varint(wood as i32);
        });
    }
    let data = convert_data(shared, direction, &spawn, target);
    if from < 771 && to >= 771 && packet == "add_entity" {
        // 1.21.6 sends a hanging entity's facing as entity data too.
        if matches!(target, "painting" | "item_frame" | "glow_item_frame") {
            synthesize(&mut entries, "HangingEntity.DATA_DIRECTION", &|w| {
                w.varint(data);
            });
        }
    }
    let velocity = legacy_velocity(spawn.velocity);
    let target_id = |name: &str| -> io::Result<i32> {
        data::version(to)
            .expect_registry("entity_type")
            .id(name)
            .ok_or_else(|| invalid("entity type without an ID"))
    };
    match packet {
        "add_entity" => {
            w.varint(spawn.id).uuid(spawn.uuid);
            if to < 477 {
                let modern = modern_object_name(to, target);
                let object = if MINECARTS.contains(&modern) {
                    10
                } else {
                    match OBJECTS.iter().find(|(_, name)| *name == modern) {
                        Some((object, _)) => *object,
                        None => return Ok(Vec::new()),
                    }
                };
                w.i8(object);
            } else {
                w.varint(target_id(target)?);
            }
            for axis in spawn.position {
                w.f64(axis);
            }
            if to >= 773 {
                write_lp_vec3(spawn.velocity, &mut w);
            }
            w.i8(spawn.pitch).i8(spawn.yaw);
            if to >= 759 {
                w.i8(spawn.head_yaw).varint(data);
            } else {
                w.i32(data);
            }
            if to < 773 {
                for axis in velocity {
                    w.i16(axis);
                }
            }
        }
        "add_mob" => {
            w.varint(spawn.id)
                .uuid(spawn.uuid)
                .varint(target_id(target)?);
            for axis in spawn.position {
                w.f64(axis);
            }
            w.i8(spawn.yaw).i8(spawn.pitch).i8(spawn.head_yaw);
            for axis in velocity {
                w.i16(axis);
            }
        }
        "add_player" => {
            w.varint(spawn.id).uuid(spawn.uuid);
            for axis in spawn.position {
                w.f64(axis);
            }
            w.i8(spawn.yaw).i8(spawn.pitch);
        }
        "add_painting" => {
            let (motive, position, facing) = match spawn.extra {
                Extra::Painting {
                    motive,
                    position,
                    facing,
                } => (motive, position, facing),
                _ => (
                    None,
                    Position {
                        x: spawn.position[0].floor() as i32,
                        y: spawn.position[1].floor() as i32,
                        z: spawn.position[2].floor() as i32,
                    },
                    spawn.data,
                ),
            };
            w.varint(spawn.id).uuid(spawn.uuid);
            // A motive the receiver lacks, or one only entity data names
            // later, shows as the first motive until that data arrives.
            if to < 393 {
                let title = motive
                    .and_then(|name| MOTIVES.iter().find(|(_, known)| *known == name))
                    .map_or("Kebab", |(title, _)| *title);
                w.string(title);
            } else {
                w.varint(motive.and_then(|name| motive_id(to, name)).unwrap_or(0));
            }
            common::write_position(to, position, &mut w);
            w.u8(facing_to_2d(facing) as u8);
            shared.entity.paintings.insert(spawn.id, (position, facing));
        }
        "add_experience_orb" => {
            w.varint(spawn.id);
            for axis in spawn.position {
                w.f64(axis);
            }
            w.i16(match spawn.extra {
                Extra::Orb(value) => value,
                _ => 1,
            });
        }
        "add_global_entity" => {
            w.varint(spawn.id).i8(1);
            for axis in spawn.position {
                w.f64(axis);
            }
        }
        _ => unreachable!("packet_for names spawn packets"),
    }
    if matches!(packet, "add_mob" | "add_player") && to < 573 {
        // Pre-1.15 spawns carry entity data themselves.
        w.bytes(list.as_deref().unwrap_or_default());
        w.bytes(&entries.buf);
        w.u8(0xff);
        entries = Writer::new();
        list = None;
    }
    shared.entity.track(
        spawn.id,
        Tracked {
            backend: if direction == Direction::Clientbound {
                spawn.kind
            } else {
                target
            },
            client: if direction == Direction::Clientbound {
                target
            } else {
                spawn.kind
            },
        },
    );
    if from < 768 && to >= 768 && matches!(spawn.kind, "boat" | "chest_boat") {
        shared.entity.boats.insert(spawn.id, w.buf.clone());
    }
    let mut packets = vec![(packet, w.buf)];
    let list = list.unwrap_or_default();
    if !list.is_empty() || !entries.is_empty() {
        let mut body = Writer::new();
        body.varint(spawn.id)
            .bytes(&list)
            .bytes(&entries.buf)
            .u8(0xff);
        packets.push(("set_entity_data", body.buf));
    }
    Ok(packets)
}

fn handle<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    direction: Direction,
    name: &str,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let spawn = read(shared.source(direction), name, &packet.data)?;
    for (name, body) in encode(shared, direction, spawn)? {
        emit(cx, shared, direction, name, body)?;
    }
    Ok(())
}

macro_rules! handlers {
    ($($name:ident),*) => {$(
        fn $name<S: HasShared>(step: &mut S, cx: &mut Cx<'_>, _: Phase, packet: Packet) -> io::Result<()> {
            handle(step, cx, Direction::Clientbound, stringify!($name), packet)
        }
    )*};
}
handlers!(
    add_entity,
    add_mob,
    add_player,
    add_painting,
    add_experience_orb,
    add_global_entity
);

/// Track a spawn passing through untouched.
fn tap<S: HasShared>(step: &mut S, name: &str, packet: &Packet) -> io::Result<()> {
    let shared = step.shared();
    let spawn = read(shared.backend, name, &packet.data)?;
    let client = map_type(shared, Direction::Clientbound, spawn.kind).unwrap_or(spawn.kind);
    shared.entity.track(
        spawn.id,
        Tracked {
            backend: spawn.kind,
            client,
        },
    );
    Ok(())
}

macro_rules! taps {
    ($($name:ident => $tap:ident),*) => {$(
        fn $tap<S: HasShared>(step: &mut S, _: Phase, packet: &Packet) -> io::Result<()> {
            tap(step, stringify!($name), packet)
        }
    )*};
}
taps!(
    add_entity => tap_add_entity,
    add_mob => tap_add_mob,
    add_player => tap_add_player,
    add_painting => tap_add_painting,
    add_experience_orb => tap_add_experience_orb,
    add_global_entity => tap_add_global_entity
);

/// Protocols where a spawn packet's layout or the choice of packet changed.
const LAYOUTS: &[i32] = &[393, 477, 573, 735, 759, 764, 770, 771, 773];

/// Whether spawns sent in `from` must be rewritten for `to`.
pub(super) fn changes(from: i32, to: i32) -> bool {
    let (low, high) = (from.min(to), from.max(to));
    LAYOUTS
        .iter()
        .any(|boundary| low < *boundary && *boundary <= high)
        || !common::registry_identity(from, to, "entity_type")
        || (metadata::differs(from, to) && (from < 573 || to < 573))
        || !common::block_states_identity(from, to)
        || (from < 759 && to < 759 && !common::registry_identity(from, to, "painting_variant"))
}

type Handler<S> = fn(&mut S, &mut Cx<'_>, Phase, Packet) -> io::Result<()>;
type Tap<S> = fn(&mut S, Phase, &Packet) -> io::Result<()>;

pub fn install<S: HasShared + 'static>(
    builder: RouteBuilder<S>,
    client: i32,
    backend: i32,
) -> RouteBuilder<S> {
    let packets: [(&'static str, Handler<S>, Tap<S>); 6] = [
        ("add_entity", add_entity::<S>, tap_add_entity::<S>),
        ("add_mob", add_mob::<S>, tap_add_mob::<S>),
        ("add_player", add_player::<S>, tap_add_player::<S>),
        ("add_painting", add_painting::<S>, tap_add_painting::<S>),
        (
            "add_experience_orb",
            add_experience_orb::<S>,
            tap_add_experience_orb::<S>,
        ),
        (
            "add_global_entity",
            add_global_entity::<S>,
            tap_add_global_entity::<S>,
        ),
    ];
    let mut builder = builder;
    if changes(backend, client) {
        for (name, handler, _) in packets {
            builder = builder.default_handler(Direction::Clientbound, Phase::Play, name, handler);
        }
    } else if tracks(client, backend) {
        for (name, _, tap) in packets {
            builder = builder.tap(Direction::Clientbound, Phase::Play, name, tap);
        }
    }
    builder
}
