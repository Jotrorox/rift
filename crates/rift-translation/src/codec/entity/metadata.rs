//! Entity data: lists of (index, serializer, value) entries ending in 0xff,
//! as `set_entity_data` and the pre-1.15 spawn packets carry them.
//!
//! Converting a list maps each entry's index through the entity type's
//! tables (see [`super::tables`]), its serializer ID through the receiver's
//! serializer table, and its value through whatever changed in between.
//! Entries the receiver has no field for are dropped; clients default the
//! fields they are not sent.
use super::{
    common::{self, block_state, copy_item, items_change, text_changes},
    particle,
    tables::{self, Kind, Target, remap},
};
use crate::{
    Direction,
    codec::{Shared, item, text},
    data, version,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::io;

const END: u8 = 0xff;

/// Skip one value of `kind` in `protocol`'s layout.
pub fn skip_value(protocol: i32, kind: Kind, reader: &mut Reader<'_>) -> io::Result<()> {
    use Kind::*;
    match kind {
        Byte => drop(reader.u8()?),
        Int
        | Direction
        | OptionalUnsignedInt
        | BlockState
        | OptionalBlockState
        | Pose
        | CatVariant
        | CatSoundVariant
        | CowVariant
        | CowSoundVariant
        | WolfVariant
        | WolfSoundVariant
        | FrogVariant
        | PigVariant
        | PigSoundVariant
        | ChickenVariant
        | ChickenSoundVariant
        | ZombieNautilusVariant
        | PaintingVariant
        | SnifferState
        | ArmadilloState
        | CopperGolemState
        | WeatheringCopperState
        | HumanoidArm
        | DyeColor => {
            reader.varint()?;
        }
        Long => drop(reader.varlong()?),
        Float => drop(reader.f32()?),
        String => drop(reader.str()?),
        Component => skip_component(protocol, reader)?,
        OptionalComponent => {
            if reader.bool()? {
                skip_component(protocol, reader)?;
            }
        }
        ItemStack => copy_item(protocol, reader, &mut Writer::new())?,
        Boolean => drop(reader.bool()?),
        Rotations | Vector3 => drop(reader.take(12)?),
        Quaternion => drop(reader.take(16)?),
        BlockPos => drop(reader.i64()?),
        OptionalBlockPos => {
            if reader.bool()? {
                reader.i64()?;
            }
        }
        OptionalUuid => {
            if reader.bool()? {
                reader.uuid()?;
            }
        }
        CompoundTag => drop(reader.nbt_bytes(!version::nameless_nbt(protocol))?),
        Particle => drop(particle::read(protocol, reader)?),
        Particles => {
            for _ in 0..reader.count()? {
                particle::read(protocol, reader)?;
            }
        }
        VillagerData => {
            for _ in 0..3 {
                reader.varint()?;
            }
        }
        WolfVariantHolder | PaintingVariantHolder => {
            if reader.varint()? == 0 {
                skip_inline(protocol, kind, reader)?;
            }
        }
        OptionalGlobalPos => {
            if reader.bool()? {
                reader.str()?;
                reader.i64()?;
            }
        }
        ResolvableProfile => skip_profile(reader)?,
    }
    Ok(())
}

fn skip_component(protocol: i32, reader: &mut Reader<'_>) -> io::Result<()> {
    if version::nbt_components(protocol) {
        reader.nbt_bytes(false)?;
    } else {
        reader.string(262_144)?;
    }
    Ok(())
}

/// A registry entry sent inline in a holder.
fn skip_inline(protocol: i32, kind: Kind, reader: &mut Reader<'_>) -> io::Result<()> {
    if kind == Kind::PaintingVariantHolder {
        reader.i32()?; // Width.
        reader.i32()?; // Height.
        reader.str()?; // Asset.
        if protocol >= 768 {
            for _ in 0..2 {
                // Title and author.
                if reader.bool()? {
                    skip_component(protocol, reader)?;
                }
            }
        }
    } else {
        for _ in 0..3 {
            reader.str()?; // Wild, tame and angry textures.
        }
        // Biomes: a tag (0, then its name) or a list of IDs (count + 1).
        match reader.varint()? {
            0 => drop(reader.str()?),
            count if count > 0 => {
                for _ in 1..count {
                    reader.varint()?;
                }
            }
            _ => return Err(invalid("invalid holder set")),
        }
    }
    Ok(())
}

fn skip_profile(reader: &mut Reader<'_>) -> io::Result<()> {
    match reader.varint()? {
        0 => {
            if reader.bool()? {
                reader.str()?;
            }
            if reader.bool()? {
                reader.uuid()?;
            }
        }
        1 => {
            reader.uuid()?;
            reader.str()?;
        }
        _ => return Err(invalid("invalid resolvable profile")),
    }
    for _ in 0..reader.count()? {
        reader.str()?; // Name.
        reader.str()?; // Value.
        if reader.bool()? {
            reader.str()?; // Signature.
        }
    }
    // Skin patch: body, cape and elytra textures, then the model.
    for _ in 0..3 {
        if reader.bool()? {
            reader.str()?;
        }
    }
    if reader.bool()? {
        reader.varint()?;
    }
    Ok(())
}

/// Copy one value unchanged.
fn copy_value(protocol: i32, kind: Kind, rw: &mut Rewriter<'_>) -> io::Result<()> {
    let start = rw.r;
    skip_value(protocol, kind, &mut rw.r)?;
    rw.w.bytes(&start.peek()[..start.remaining() - rw.r.remaining()]);
    Ok(())
}

/// Whether a value of `kind` reads the same in both protocols and needs no
/// conversion, so it can be copied.
fn unchanged(from: i32, to: i32, kind: Kind) -> bool {
    use Kind::*;
    if from == to {
        return true;
    }
    match kind {
        Component | OptionalComponent => !text_changes(from, to),
        ItemStack => !items_change(from, to),
        BlockPos | OptionalBlockPos => {
            version::modern_positions(from) == version::modern_positions(to)
        }
        BlockState | OptionalBlockState => common::block_states_identity(from, to),
        CompoundTag => version::nameless_nbt(from) == version::nameless_nbt(to),
        Particle | Particles => particle::unchanged(from, to),
        VillagerData => {
            common::registry_identity(from, to, "villager_type")
                && common::registry_identity(from, to, "villager_profession")
        }
        PaintingVariantHolder | WolfVariantHolder => {
            (from >= 768) == (to >= 768) && registry_kind_unchanged(from, to, kind)
        }
        kind if kind.is_enum() => {
            tables::tables(from).constants(kind) == tables::tables(to).constants(kind)
        }
        kind if kind.registry().is_some() => registry_kind_unchanged(from, to, kind),
        _ => true,
    }
}

fn registry_kind_unchanged(from: i32, to: i32, kind: Kind) -> bool {
    kind.registry()
        .is_none_or(|registry| common::registry_unchanged(from, to, registry))
}

/// Constants Mojang renamed without changing their meaning.
const ENUM_ALIASES: &[(&str, &str)] = &[("SNEAKING", "CROUCHING")];

fn convert_enum(from: i32, to: i32, kind: Kind, value: i32) -> Option<i32> {
    let name = *tables::tables(from)
        .constants(kind)
        .get(usize::try_from(value).ok()?)?;
    let constants = tables::tables(to).constants(kind);
    let find = |name: &str| constants.iter().position(|known| *known == name);
    find(name)
        .or_else(|| {
            ENUM_ALIASES.iter().find_map(|(a, b)| {
                (*a == name)
                    .then(|| find(b))
                    .or_else(|| (*b == name).then(|| find(a)))?
            })
        })
        // A pose the receiver lacks reads as standing.
        .or((kind == Kind::Pose).then_some(0))
        .map(|index| index as i32)
}

/// String constants some fields used before they became enum IDs (salmon
/// size in 1.21.4, mooshroom type in 1.21.5), by field key.
const STRING_ENUMS: &[(&str, &[&str])] = &[
    ("Salmon.DATA_TYPE", &["small", "medium", "large"]),
    ("MushroomCow.DATA_TYPE", &["red", "brown"]),
];

/// 1.13 villager professions by ID, with the 1.14 profession each became.
const PROFESSIONS: &[(&str, &[&str])] = &[
    (
        "farmer",
        &["farmer", "fisherman", "shepherd", "fletcher", "none"],
    ),
    ("librarian", &["librarian", "cartographer"]),
    ("cleric", &["cleric"]),
    ("armorer", &["armorer", "weaponsmith", "toolsmith", "mason"]),
    ("butcher", &["butcher", "leatherworker"]),
    ("nitwit", &["nitwit"]),
];

/// Convert one value of a field from the sender's layout and terms to the
/// receiver's. `Ok(false)` means the receiver cannot express it: the caller
/// drops the entry and discards whatever was written.
fn convert_value(
    shared: &mut Shared,
    direction: Direction,
    target: Target,
    rw: &mut Rewriter<'_>,
) -> io::Result<bool> {
    use Kind::*;
    let (from, to) = (shared.source(direction), shared.target(direction));
    if target.key == "AbstractMinecart.DATA_ID_CUSTOM_DISPLAY_BLOCK"
        && (target.from, target.to) == (Int, Int)
        && !common::block_states_identity(from, to)
    {
        // A block state sent as a plain int until 1.21.5.
        let state = rw.r.varint()?;
        rw.w.varint(block_state(shared, direction, state));
        return Ok(true);
    }
    if target.from == target.to && unchanged(from, to, target.from) {
        copy_value(from, target.from, rw)?;
        return Ok(true);
    }
    match (target.from, target.to) {
        (Component, Component) => text::transcode(shared, direction, rw)?,
        (OptionalComponent, OptionalComponent) => {
            if rw.bool()? {
                text::transcode(shared, direction, rw)?;
            }
        }
        (ItemStack, ItemStack) => item::transcode(shared, direction, rw)?,
        (BlockPos, BlockPos) => {
            let position = common::read_position(from, &mut rw.r)?;
            common::write_position(to, position, &mut rw.w);
        }
        (OptionalBlockPos, OptionalBlockPos) => {
            if rw.bool()? {
                let position = common::read_position(from, &mut rw.r)?;
                common::write_position(to, position, &mut rw.w);
            }
        }
        (BlockState, BlockState) => {
            let state = rw.r.varint()?;
            rw.w.varint(block_state(shared, direction, state));
        }
        (OptionalBlockState, OptionalBlockState) => {
            let state = rw.r.varint()?;
            rw.w.varint(if state == 0 {
                0
            } else {
                block_state(shared, direction, state)
            });
        }
        (CompoundTag, CompoundTag) => common::convert_nbt(from, to, &mut rw.r, &mut rw.w)?,
        (Particle, Particle) => return particle::transcode(shared, direction, rw),
        (Particles, Particles) => {
            let mut converted = Vec::new();
            for _ in 0..rw.r.count()? {
                let read = particle::read(from, &mut rw.r)?;
                converted.extend(particle::convert(shared, direction, read)?);
            }
            rw.w.varint(converted.len() as i32);
            for particle in &converted {
                particle::write(to, particle, &mut rw.w)?;
            }
        }
        (VillagerData, VillagerData) => {
            let (kind, profession, level) = (rw.r.varint()?, rw.r.varint()?, rw.r.varint()?);
            let kind = common::registry(shared, direction, "villager_type")
                .map_or(Some(kind), |mapping| mapping.get(kind));
            let profession = common::registry(shared, direction, "villager_profession")
                .map_or(Some(profession), |mapping| mapping.get(profession));
            let (Some(kind), Some(profession)) = (kind, profession) else {
                return Ok(false);
            };
            rw.w.varint(kind).varint(profession).varint(level);
        }
        (kind, other) if kind == other && kind.is_enum() => {
            let value = rw.r.varint()?;
            match convert_enum(from, to, kind, value) {
                Some(value) => drop(rw.w.varint(value)),
                None => return Ok(false),
            }
        }
        (PaintingVariantHolder, PaintingVariantHolder) | (WolfVariantHolder, WolfVariantHolder) => {
            let holder = rw.r.varint()?;
            if holder == 0 {
                // An inline variant whose layout changed: nothing to send.
                skip_inline(from, target.from, &mut rw.r)?;
                return Ok(false);
            }
            match map_registry(shared, direction, target.from, holder - 1) {
                Some(id) => drop(rw.w.varint(id + 1)),
                None => return Ok(false),
            }
        }
        (kind, other) if kind == other && kind.registry().is_some() => {
            let id = rw.r.varint()?;
            match map_registry(shared, direction, kind, id) {
                Some(id) => drop(rw.w.varint(id)),
                None => return Ok(false),
            }
        }
        (kind, other) if kind == other => copy_value(from, kind, rw)?,
        (String, OptionalComponent) => {
            // Custom names became components in 1.13.
            let name = rw.r.str()?;
            if name.is_empty() {
                rw.w.bool(false);
            } else {
                rw.w.bool(true).string(&common::literal_json(name));
            }
        }
        (OptionalComponent, String) => {
            let name = if rw.r.bool()? {
                common::flatten_json(rw.r.string(262_144)?)
            } else {
                std::string::String::new()
            };
            rw.w.string(&name);
        }
        (Int, OptionalUnsignedInt) => {
            // An entity ID with 0 for none became an optional ID.
            let id = rw.r.varint()?;
            rw.w.varint(if id > 0 { id + 1 } else { 0 });
        }
        (OptionalUnsignedInt, Int) => {
            let id = rw.r.varint()?;
            rw.w.varint(if id > 0 { id - 1 } else { 0 });
        }
        (Int, VillagerData) => {
            let profession = rw.r.varint()?;
            let name = PROFESSIONS
                .get(usize::try_from(profession).unwrap_or(0))
                .unwrap_or(&PROFESSIONS[0])
                .0;
            let target_data = data::version(to);
            let kind = target_data
                .expect_registry("villager_type")
                .id("plains")
                .unwrap_or(0);
            let profession = target_data
                .expect_registry("villager_profession")
                .id(name)
                .unwrap_or(0);
            rw.w.varint(kind).varint(profession).varint(1);
        }
        (VillagerData, Int) => {
            rw.r.varint()?;
            let profession = rw.r.varint()?;
            rw.r.varint()?;
            let name = data::version(from)
                .expect_registry("villager_profession")
                .name(profession)
                .unwrap_or("farmer");
            let legacy = PROFESSIONS
                .iter()
                .position(|(_, names)| names.contains(&name))
                .unwrap_or(0);
            rw.w.varint(legacy as i32);
        }
        (Int, CatVariant) | (CatVariant, Int) => {
            // 1.19 made cat types a registry in the same order.
            let id = rw.r.varint()?;
            let size = data::version(to)
                .registry("cat_variant")
                .map_or(11, |registry| registry.len());
            rw.w.varint(if (id as usize) < size { id } else { 0 });
        }
        (Int, Particles) => {
            // 1.20.5 turned the potion colour into entity effect particles.
            let color = rw.r.varint()?;
            if color == 0 {
                rw.w.varint(0);
            } else {
                let effect = data::version(to)
                    .expect_registry("particle_type")
                    .id("entity_effect")
                    .ok_or_else(|| invalid("no entity_effect particle"))?;
                rw.w.varint(1)
                    .varint(effect)
                    .i32(color | 0xff00_0000_u32 as i32);
            }
        }
        (Particles, Int) => {
            let mut color = 0;
            for _ in 0..rw.r.count()? {
                if let particle::Options::Color(argb) = particle::read(from, &mut rw.r)?.options
                    && color == 0
                {
                    color = argb & 0x00ff_ffff;
                }
            }
            rw.w.varint(color);
        }
        (Int, OptionalBlockState) | (OptionalBlockState, Int) => {
            // 1.21.5 made a minecart's display block optional.
            let state = rw.r.varint()?;
            rw.w.varint(if state == 0 {
                0
            } else {
                block_state(shared, direction, state)
            });
        }
        (String, Int) => {
            let value = rw.r.str()?;
            let Some((_, names)) = STRING_ENUMS.iter().find(|(key, _)| *key == target.key) else {
                return Ok(false);
            };
            rw.w.varint(names.iter().position(|name| *name == value).unwrap_or(0) as i32);
        }
        (Int, String) => {
            let value = rw.r.varint()?;
            let Some((_, names)) = STRING_ENUMS.iter().find(|(key, _)| *key == target.key) else {
                return Ok(false);
            };
            rw.w.string(names.get(value as usize).copied().unwrap_or(names[0]));
        }
        (Byte, HumanoidArm) => {
            let arm = rw.r.u8()?;
            rw.w.varint(i32::from(arm.min(1)));
        }
        (HumanoidArm, Byte) => {
            let arm = rw.r.varint()?;
            rw.w.u8(arm.clamp(0, 1) as u8);
        }
        (PaintingVariant, PaintingVariantHolder) | (WolfVariant, WolfVariantHolder) => {
            let id = rw.r.varint()?;
            match map_registry(shared, direction, target.from, id) {
                Some(id) => drop(rw.w.varint(id + 1)),
                None => return Ok(false),
            }
        }
        (PaintingVariantHolder, PaintingVariant) | (WolfVariantHolder, WolfVariant) => {
            let holder = rw.r.varint()?;
            if holder == 0 {
                skip_inline(from, target.from, &mut rw.r)?;
                return Ok(false);
            }
            match map_registry(shared, direction, target.from, holder - 1) {
                Some(id) => drop(rw.w.varint(id)),
                None => return Ok(false),
            }
        }
        (kind, _) => {
            skip_value(from, kind, &mut rw.r)?;
            return Ok(false);
        }
    }
    Ok(true)
}

/// A registry-ID value in the receiver's terms (see
/// [`common::registry_id`]); `None` when the receiver has no equivalent.
fn map_registry(shared: &mut Shared, direction: Direction, kind: Kind, id: i32) -> Option<i32> {
    match kind.registry() {
        Some(registry) => common::registry_id(shared, direction, registry, id),
        None => Some(id),
    }
}

/// Copy an entity data list from `rw.r` to `rw.w`, converting it from the
/// sender's protocol for an entity the sender knows as `from_type` to the
/// receiver's, who knows it as `to_type` (type names; `None` when the
/// entity is not tracked, which keeps only the fields all entities have).
pub fn transcode(
    shared: &mut Shared,
    direction: Direction,
    from_type: Option<&str>,
    to_type: Option<&str>,
    rw: &mut Rewriter<'_>,
) -> io::Result<()> {
    let (from, to) = (shared.source(direction), shared.target(direction));
    let (source, target) = (tables::tables(from), tables::tables(to));
    let remap = remap(from, to, from_type, to_type);
    loop {
        let index = rw.r.u8()?;
        if index == END {
            rw.w.u8(END);
            return Ok(());
        }
        let serializer = rw.r.varint()?;
        let kind = source
            .serializer(serializer)
            .ok_or_else(|| invalid("unknown entity data serializer"))?;
        let mark = rw.w.len();
        // An entry the tables cannot place, or whose serializer differs from
        // the tables', would break the receiver: it is dropped.
        let Some(found) = remap.get(index).filter(|found| found.from == kind) else {
            skip_value(from, kind, &mut rw.r)?;
            continue;
        };
        let Some(id) = target.serializer_id(found.to) else {
            skip_value(from, kind, &mut rw.r)?;
            continue;
        };
        rw.w.u8(found.index).varint(id);
        if !convert_value(shared, direction, found, rw)? {
            rw.w.buf.truncate(mark);
        }
    }
}

/// Read an entity data list in `protocol` and check every entry against the
/// tables of entity type `type_name`: the index exists and the serializer
/// matches. Returns the number of entries; for tests and diagnostics.
pub fn check(protocol: i32, type_name: &str, reader: &mut Reader<'_>) -> io::Result<usize> {
    let tables = tables::tables(protocol);
    let fields = tables
        .fields(type_name)
        .ok_or_else(|| invalid(&format!("unknown entity type {type_name}")))?;
    let mut entries = 0;
    loop {
        let index = reader.u8()?;
        if index == END {
            return Ok(entries);
        }
        let kind = tables
            .serializer(reader.varint()?)
            .ok_or_else(|| invalid("unknown entity data serializer"))?;
        let field = fields
            .get(usize::from(index))
            .ok_or_else(|| invalid(&format!("{type_name} has no entity data index {index}")))?;
        if field.kind != kind {
            return Err(invalid(&format!(
                "{type_name} index {index} ({}) is {}, sent as {}",
                field.key,
                field.kind.name(),
                kind.name()
            )));
        }
        skip_value(protocol, kind, reader)?;
        entries += 1;
    }
}

/// Whether entity data lists ever need converting between two protocols:
/// indexes, serializer IDs or value layouts differ, or a registry or
/// structure a value refers to changes.
pub fn differs(from: i32, to: i32) -> bool {
    use std::{
        collections::HashMap,
        sync::{Mutex, OnceLock},
    };
    static CACHE: OnceLock<Mutex<HashMap<(i32, i32), bool>>> = OnceLock::new();
    if from == to {
        return false;
    }
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    *cache.entry((from, to)).or_insert_with(|| {
        let (a, b) = (tables::tables(from), tables::tables(to));
        if a.serializers != b.serializers {
            return true;
        }
        if a.serializers.iter().any(|kind| !unchanged(from, to, *kind)) {
            return true;
        }
        let mut names: Vec<&str> = a.type_names().collect();
        names.sort_unstable();
        names.iter().any(|name| {
            // A type the receiver lacks reaches it as a stand-in, through a
            // remap by key.
            b.fields(name) != a.fields(name)
        })
    })
}
