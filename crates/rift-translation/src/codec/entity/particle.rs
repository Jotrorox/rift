//! Particle types and their options in every protocol.
//!
//! Which particles carry options, and of which type, comes from each
//! version's `ParticleTypes` (see `data/entities`); how each options type is
//! laid out in each protocol is below. 1.12 numbers particles by their old
//! names and sends block and item particles' data as an int array instead.
use super::common::{self, block_state, items_change};
use crate::{
    Direction,
    codec::{Shared, item},
    data,
    wire::{Position, Reader, Rewriter, Writer, invalid},
};
use std::io;

/// A position source of a vibration particle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Source {
    Block(Position),
    /// An entity, and from 1.19 the height above its feet.
    Entity {
        id: i32,
        y_offset: f32,
    },
}

/// One particle's options, read in one protocol. Colours are kept as the
/// floats or ints the protocol sends.
#[derive(Clone, Debug, PartialEq)]
pub enum Options {
    None,
    Block(i32),
    Dust {
        color: Color,
        scale: f32,
    },
    DustTransition {
        from: Color,
        to: Color,
        scale: f32,
    },
    /// An item stack as sent, converted only when the stack layout changes.
    Item(Vec<u8>),
    Vibration {
        /// 1.17 and 1.18 also send where the vibration starts.
        origin: Option<Position>,
        source: Source,
        ticks: i32,
    },
    SculkCharge(f32),
    Shriek(i32),
    /// An ARGB colour.
    Color(i32),
    Trail {
        target: [f64; 3],
        color: i32,
        /// From 1.21.4.
        duration: i32,
    },
    Spell {
        color: i32,
        power: f32,
    },
    Power(f32),
    Geyser(i32),
    GeyserBase(i32, f32),
}

/// An RGB colour as floats (before 1.21.2) or a packed int (from then on).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Color {
    Floats([f32; 3]),
    Rgb(i32),
}

impl Color {
    fn rgb(self) -> i32 {
        match self {
            Color::Rgb(rgb) => rgb,
            Color::Floats(floats) => floats.iter().fold(0, |rgb, channel| {
                (rgb << 8) | ((channel.clamp(0.0, 1.0) * 255.0).round() as i32)
            }),
        }
    }

    fn floats(self) -> [f32; 3] {
        match self {
            Color::Floats(floats) => floats,
            Color::Rgb(rgb) => [16, 8, 0].map(|shift| ((rgb >> shift) & 0xff) as f32 / 255.0),
        }
    }

    fn read(protocol: i32, reader: &mut Reader<'_>) -> io::Result<Self> {
        Ok(if protocol >= 768 {
            Color::Rgb(reader.i32()?)
        } else {
            Color::Floats([reader.f32()?, reader.f32()?, reader.f32()?])
        })
    }

    fn write(self, protocol: i32, writer: &mut Writer) {
        if protocol >= 768 {
            writer.i32(self.rgb());
        } else {
            for channel in self.floats() {
                writer.f32(channel);
            }
        }
    }
}

/// A particle type ID and its options in one protocol.
#[derive(Clone, Debug, PartialEq)]
pub struct Particle {
    pub id: i32,
    pub options: Options,
}

/// The options type of particle `id` in `protocol` (1.13 and newer).
fn options_type(protocol: i32, id: i32) -> io::Result<Option<&'static str>> {
    let name = data::version(protocol)
        .expect_registry("particle_type")
        .name(id)
        .ok_or_else(|| invalid("unknown particle type"))?;
    Ok(super::tables::tables(protocol).particle_options(name))
}

/// Read the options of particle `id`, which the caller has read already.
pub fn read_options(protocol: i32, id: i32, reader: &mut Reader<'_>) -> io::Result<Options> {
    let Some(kind) = options_type(protocol, id)? else {
        return Ok(Options::None);
    };
    Ok(match kind {
        "BlockParticleOption" => Options::Block(reader.varint()?),
        "DustParticleOptions" => Options::Dust {
            color: Color::read(protocol, reader)?,
            scale: reader.f32()?,
        },
        "DustColorTransitionOptions" => {
            if protocol >= 768 {
                Options::DustTransition {
                    from: Color::Rgb(reader.i32()?),
                    to: Color::Rgb(reader.i32()?),
                    scale: reader.f32()?,
                }
            } else if protocol >= 766 {
                let from = Color::read(protocol, reader)?;
                let to = Color::read(protocol, reader)?;
                Options::DustTransition {
                    from,
                    to,
                    scale: reader.f32()?,
                }
            } else {
                let from = Color::read(protocol, reader)?;
                let scale = reader.f32()?;
                Options::DustTransition {
                    from,
                    to: Color::read(protocol, reader)?,
                    scale,
                }
            }
        }
        "ItemParticleOption" => {
            let mut writer = Writer::new();
            if protocol >= 775 {
                copy_template(protocol, reader, &mut writer)?;
            } else {
                common::copy_item(protocol, reader, &mut writer)?;
            }
            Options::Item(writer.buf)
        }
        "VibrationParticleOption" => {
            let origin = if protocol < 759 {
                Some(reader.position()?)
            } else {
                None
            };
            let source = read_source(protocol, reader)?;
            Options::Vibration {
                origin,
                source,
                ticks: reader.varint()?,
            }
        }
        "SculkChargeParticleOptions" => Options::SculkCharge(reader.f32()?),
        "ShriekParticleOption" => Options::Shriek(reader.varint()?),
        "ColorParticleOption" => Options::Color(reader.i32()?),
        "TargetColorParticleOption" | "TrailParticleOption" => {
            let target = [reader.f64()?, reader.f64()?, reader.f64()?];
            let color = reader.i32()?;
            let duration = if protocol >= 769 {
                reader.varint()?
            } else {
                20
            };
            Options::Trail {
                target,
                color,
                duration,
            }
        }
        "SpellParticleOption" => Options::Spell {
            color: reader.i32()?,
            power: reader.f32()?,
        },
        "PowerParticleOption" => Options::Power(reader.f32()?),
        "GeyserParticleOptions" => Options::Geyser(reader.i32()?),
        "GeyserBaseParticleOptions" => Options::GeyserBase(reader.i32()?, reader.f32()?),
        other => return Err(invalid(&format!("unknown particle options {other}"))),
    })
}

/// 26.1 item particles carry an item template: item, count, components.
fn copy_template(protocol: i32, reader: &mut Reader<'_>, writer: &mut Writer) -> io::Result<()> {
    let start = *reader;
    reader.varint()?;
    reader.varint()?;
    item::components::read(protocol, reader)?;
    writer.bytes(&start.peek()[..start.remaining() - reader.remaining()]);
    Ok(())
}

fn read_source(protocol: i32, reader: &mut Reader<'_>) -> io::Result<Source> {
    let block = if protocol >= 766 {
        let registry = data::version(protocol).expect_registry("position_source_type");
        match registry.name(reader.varint()?) {
            Some("block") => true,
            Some("entity") => false,
            _ => return Err(invalid("unknown position source type")),
        }
    } else {
        match data::strip(reader.string(32767)?) {
            "block" => true,
            "entity" => false,
            _ => return Err(invalid("unknown position source type")),
        }
    };
    Ok(if block {
        Source::Block(reader.position()?)
    } else {
        let id = reader.varint()?;
        let y_offset = if protocol >= 759 { reader.f32()? } else { 0.0 };
        Source::Entity { id, y_offset }
    })
}

fn write_source(protocol: i32, source: Source, writer: &mut Writer) -> io::Result<()> {
    let name = match source {
        Source::Block(_) => "block",
        Source::Entity { .. } => "entity",
    };
    if protocol >= 766 {
        let registry = data::version(protocol).expect_registry("position_source_type");
        writer.varint(
            registry
                .id(name)
                .ok_or_else(|| invalid("no position source type"))?,
        );
    } else {
        writer.string(&format!("minecraft:{name}"));
    }
    match source {
        Source::Block(position) => drop(writer.position(position)),
        Source::Entity { id, y_offset } => {
            writer.varint(id);
            if protocol >= 759 {
                writer.f32(y_offset);
            }
        }
    }
    Ok(())
}

/// Write options in `protocol`'s layout for a particle whose options type
/// there is `kind`; the options must already be in that protocol's terms.
fn write_options(
    protocol: i32,
    kind: Option<&str>,
    options: &Options,
    writer: &mut Writer,
) -> io::Result<()> {
    match (kind, options) {
        (None, _) => {}
        (Some("BlockParticleOption"), Options::Block(state)) => drop(writer.varint(*state)),
        (Some("DustParticleOptions"), Options::Dust { color, scale }) => {
            color.write(protocol, writer);
            writer.f32(*scale);
        }
        (Some("DustColorTransitionOptions"), Options::DustTransition { from, to, scale }) => {
            if protocol >= 766 {
                from.write(protocol, writer);
                to.write(protocol, writer);
                writer.f32(*scale);
            } else {
                from.write(protocol, writer);
                writer.f32(*scale);
                to.write(protocol, writer);
            }
        }
        (Some("ItemParticleOption"), Options::Item(stack)) => drop(writer.bytes(stack)),
        (
            Some("VibrationParticleOption"),
            Options::Vibration {
                origin,
                source,
                ticks,
            },
        ) => {
            if protocol < 759 {
                let fallback = match source {
                    Source::Block(position) => *position,
                    Source::Entity { .. } => Position { x: 0, y: 0, z: 0 },
                };
                writer.position(origin.unwrap_or(fallback));
            }
            write_source(protocol, *source, writer)?;
            writer.varint(*ticks);
        }
        (Some("SculkChargeParticleOptions"), Options::SculkCharge(roll)) => drop(writer.f32(*roll)),
        (Some("ShriekParticleOption"), Options::Shriek(delay)) => drop(writer.varint(*delay)),
        (Some("ColorParticleOption"), Options::Color(color)) => drop(writer.i32(*color)),
        (
            Some("TargetColorParticleOption" | "TrailParticleOption"),
            Options::Trail {
                target,
                color,
                duration,
            },
        ) => {
            for axis in target {
                writer.f64(*axis);
            }
            writer.i32(*color);
            if protocol >= 769 {
                writer.varint(*duration);
            }
        }
        (Some("SpellParticleOption"), Options::Spell { color, power }) => {
            writer.i32(*color).f32(*power);
        }
        (Some("PowerParticleOption"), Options::Power(power)) => drop(writer.f32(*power)),
        (Some("GeyserParticleOptions"), Options::Geyser(color)) => drop(writer.i32(*color)),
        (Some("GeyserBaseParticleOptions"), Options::GeyserBase(color, value)) => {
            writer.i32(*color).f32(*value);
        }
        (Some(kind), options) => {
            return Err(invalid(&format!(
                "particle options {options:?} do not fit {kind}"
            )));
        }
    }
    Ok(())
}

/// Options for a receiver whose options type for the same particle differs
/// from the sender's (it gained or lost options); `None` when there is no
/// sensible equivalent.
fn adapt(options: Options, kind: Option<&str>) -> Option<Options> {
    const WHITE: i32 = -1;
    Some(match (kind, options) {
        (None, _) => Options::None,
        (Some("ColorParticleOption"), Options::Spell { color, .. }) => Options::Color(color),
        (Some("ColorParticleOption"), Options::None) => Options::Color(WHITE),
        (Some("SpellParticleOption"), Options::Color(color)) => {
            Options::Spell { color, power: 1.0 }
        }
        (Some("SpellParticleOption"), Options::None) => Options::Spell {
            color: WHITE,
            power: 1.0,
        },
        (Some("PowerParticleOption"), Options::None) => Options::Power(1.0),
        (
            Some("TargetColorParticleOption" | "TrailParticleOption"),
            trail @ Options::Trail { .. },
        ) => trail,
        _ => return None,
    })
}

/// Read one particle (VarInt type ID, then options) in `protocol`, 1.13+.
pub fn read(protocol: i32, reader: &mut Reader<'_>) -> io::Result<Particle> {
    let id = reader.varint()?;
    let options = read_options(protocol, id, reader)?;
    Ok(Particle { id, options })
}

/// Write one particle in `protocol`, 1.13+.
pub fn write(protocol: i32, particle: &Particle, writer: &mut Writer) -> io::Result<()> {
    writer.varint(particle.id);
    write_options(
        protocol,
        options_type(protocol, particle.id)?,
        &particle.options,
        writer,
    )
}

/// Convert a particle read in the sender's protocol (1.13+) into the
/// receiver's (1.13+): its type by name, block states and items inside, and
/// its options' layout. `None` when the receiver has no such particle.
pub fn convert(
    shared: &mut Shared,
    direction: Direction,
    particle: Particle,
) -> io::Result<Option<Particle>> {
    let (from, to) = (shared.source(direction), shared.target(direction));
    if from == to {
        return Ok(Some(particle));
    }
    let Some(id) = shared.mapping(direction, "particle_type").get(particle.id) else {
        return Ok(None);
    };
    let (sent, received) = (options_type(from, particle.id)?, options_type(to, id)?);
    let mut options = particle.options;
    if sent != received {
        match adapt(options, received) {
            Some(adapted) => options = adapted,
            None => return Ok(None),
        }
    }
    match &mut options {
        Options::Block(state) => *state = block_state(shared, direction, *state),
        Options::Item(stack) if items_change(from, to) => {
            let mut converted = Writer::new();
            if from >= 775 || to >= 775 {
                if !(from >= 775 && to >= 775) {
                    return Ok(None);
                }
                // INTEGRATION: item templates (26.1+) need the item codec's
                // component conversion; the item ID is mapped here.
                let mut reader = Reader::new(stack);
                let item_id = reader.varint()?;
                let count = reader.varint()?;
                let mut components = item::components::read(from, &mut reader)?;
                reader.finish()?;
                let Some(item_id) = shared.mapping(direction, "item").get(item_id) else {
                    return Ok(None);
                };
                item::components::convert(shared, direction, item_id, &mut components)?;
                converted.varint(item_id).varint(count);
                item::components::write(to, &components, &mut converted)?;
            } else {
                let mut reader = Reader::new(stack);
                let mut value = item::read(from, &mut reader)?;
                reader.finish()?;
                item::convert(shared, direction, &mut value)?;
                if value.is_none() {
                    return Ok(None);
                }
                item::write(to, value.as_ref(), &mut converted)?;
            }
            *stack = converted.buf;
        }
        _ => {}
    }
    Ok(Some(Particle { id, options }))
}

/// Copy one particle (its type ID and the options that follow) from `rw.r`
/// to `rw.w`, converting it between the two protocols of the step. `Ok(false)`
/// means the receiver has no such particle and no stand-in: nothing was
/// written and the caller drops what contains it. For 1.13 and newer, where a
/// particle is a type ID followed by its options; for other codecs whose
/// structures contain particles.
pub fn transcode(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<bool> {
    let particle = read(shared.source(direction), &mut rw.r)?;
    match convert(shared, direction, particle)? {
        Some(particle) => {
            write(shared.target(direction), &particle, &mut rw.w)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Whether particles look the same in two protocols: same IDs, same options
/// types, same layouts, and nothing inside that converts.
pub fn unchanged(from: i32, to: i32) -> bool {
    if from == to {
        return true;
    }
    if from < 393 || to < 393 {
        return from < 393 && to < 393 && common::registry_identity(from, to, "particle_type");
    }
    // Layouts of some options types changed on these protocols.
    const LAYOUTS: &[i32] = &[759, 766, 768, 769, 775];
    let (low, high) = (from.min(to), from.max(to));
    if LAYOUTS
        .iter()
        .any(|boundary| low < *boundary && *boundary <= high)
    {
        return false;
    }
    let (a, b) = (super::tables::tables(from), super::tables::tables(to));
    let registry = data::version(from).expect_registry("particle_type");
    common::registry_identity(from, to, "particle_type")
        && registry
            .names()
            .all(|(_, name)| a.particle_options(name) == b.particle_options(name))
        && common::block_states_identity(from, to)
        && !items_change(from, to)
}

/// 1.12 particle names (by ID) with their 1.13 names; `None` for those 1.13
/// dropped.
const LEGACY_NAMES: &[(&str, Option<&str>)] = &[
    ("explode", Some("poof")),
    ("largeexplode", Some("explosion")),
    ("hugeexplosion", Some("explosion_emitter")),
    ("fireworksSpark", Some("firework")),
    ("bubble", Some("bubble")),
    ("splash", Some("splash")),
    ("wake", Some("fishing")),
    ("suspended", Some("underwater")),
    ("depthsuspend", Some("underwater")),
    ("crit", Some("crit")),
    ("magicCrit", Some("enchanted_hit")),
    ("smoke", Some("smoke")),
    ("largesmoke", Some("large_smoke")),
    ("spell", Some("effect")),
    ("instantSpell", Some("instant_effect")),
    ("mobSpell", Some("entity_effect")),
    ("mobSpellAmbient", Some("ambient_entity_effect")),
    ("witchMagic", Some("witch")),
    ("dripWater", Some("dripping_water")),
    ("dripLava", Some("dripping_lava")),
    ("angryVillager", Some("angry_villager")),
    ("happyVillager", Some("happy_villager")),
    ("townaura", Some("mycelium")),
    ("note", Some("note")),
    ("portal", Some("portal")),
    ("enchantmenttable", Some("enchant")),
    ("flame", Some("flame")),
    ("lava", Some("lava")),
    ("footstep", None),
    ("cloud", Some("cloud")),
    ("reddust", Some("dust")),
    ("snowballpoof", Some("item_snowball")),
    ("snowshovel", Some("item_snowball")),
    ("slime", Some("item_slime")),
    ("heart", Some("heart")),
    ("barrier", Some("barrier")),
    ("iconcrack", Some("item")),
    ("blockcrack", Some("block")),
    ("blockdust", Some("block")),
    ("droplet", Some("rain")),
    ("take", None),
    ("mobappearance", Some("elder_guardian")),
    ("dragonbreath", Some("dragon_breath")),
    ("endRod", Some("end_rod")),
    ("damageIndicator", Some("damage_indicator")),
    ("sweepAttack", Some("sweep_attack")),
    ("fallingdust", Some("falling_dust")),
    ("totem", Some("totem_of_undying")),
    ("spit", Some("spit")),
];

/// How many ints of data a 1.12 particle carries: an item and its damage
/// for `iconcrack`, a block (ID | metadata << 12) for the block particles.
pub fn legacy_data_len(id: i32) -> usize {
    match LEGACY_NAMES.get(id as usize).map(|(name, _)| *name) {
        Some("iconcrack") => 2,
        Some("blockcrack" | "blockdust" | "fallingdust") => 1,
        _ => 0,
    }
}

/// A 1.12 particle in 1.13 terms. `color` is the dust colour 1.12 sends in
/// the offsets. `None` when 1.13 has no such particle, or it needs a block
/// or item 1.12 numbers by legacy ID.
// INTEGRATION: block and item particles across 1.13 need the data
// engineer's legacy block and item tables; they are dropped until then.
pub fn upgrade_legacy(id: i32, color: [f32; 3]) -> Option<Particle> {
    let (_, name) = LEGACY_NAMES.get(usize::try_from(id).ok()?)?;
    let name = (*name)?;
    let options = match name {
        "dust" => Options::Dust {
            color: Color::Floats(color),
            scale: 1.0,
        },
        "item" | "block" | "falling_dust" => return None,
        _ => Options::None,
    };
    let id = data::version(393)
        .expect_registry("particle_type")
        .id(name)?;
    Some(Particle { id, options })
}

/// A 1.13 particle as a 1.12 particle ID, with the dust colour 1.12 takes
/// from the offsets. `None` when 1.12 has no equivalent.
pub fn downgrade_legacy(particle: &Particle) -> Option<(i32, Option<[f32; 3]>)> {
    let name = data::version(393)
        .expect_registry("particle_type")
        .name(particle.id)?;
    let id = LEGACY_NAMES
        .iter()
        .position(|(_, modern)| *modern == Some(name))?;
    match (&particle.options, name) {
        (Options::Dust { color, .. }, _) => Some((id as i32, Some(color.floats()))),
        (_, "item" | "block" | "falling_dust") => None,
        _ => Some((id as i32, None)),
    }
}
