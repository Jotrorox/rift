//! The registry parts of the play `login` and `respawn` packets: the world
//! list, the registry codec (1.16 to 1.20.1) and the dimension the player
//! is in. Steps own these packets because their other fields change in most
//! versions; this module reads and converts the registry parts for them and
//! keeps the bookkeeping current (see [`super::tap_login`]).
//!
//! The parts sit at the start of both packets, so a step's handler can call
//! [`transcode_login`] or [`transcode_respawn`] first and then deal with the
//! rest. Where the fields before them differ between the pair's protocols
//! (1.15.2/1.16, 1.16.1/1.16.2, 1.20.1/1.20.2), the step uses the pieces:
//! [`read_dimension`], [`convert_dimension`], [`write_dimension`],
//! [`read_codec`], [`convert_codec`], [`write_codec`].
use super::{
    Side, convert, defaults,
    form::{self, Registry},
    record, record_dimension,
};
use crate::{
    Direction,
    codec::Shared,
    nbt::{Compound, Nbt},
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::io;

/// The dimension a `login` or `respawn` puts the player in, in the form
/// the protocol sends it.
#[derive(Clone, Debug, PartialEq)]
pub enum Dimension {
    /// Before 1.16: -1 (nether), 0 (overworld), 1 (end).
    Legacy(i32),
    /// The dimension type's name: 1.16, 1.16.1 and 1.19 to 1.20.4.
    Name(String),
    /// The dimension type itself: 1.16.2 to 1.18.2.
    Element(Compound),
    /// The dimension type's network ID: 1.20.5 on.
    Id(i32),
}

/// How `protocol` sends the dimension in `login` and `respawn`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DimensionForm {
    Legacy,
    Name,
    Element,
    Id,
}

pub fn dimension_form(protocol: i32) -> DimensionForm {
    match protocol {
        ..735 => DimensionForm::Legacy,
        735..751 | 759..766 => DimensionForm::Name,
        751..759 => DimensionForm::Element,
        _ => DimensionForm::Id,
    }
}

/// Network NBT has a named root before 1.20.2.
fn named(protocol: i32) -> bool {
    !crate::version::nameless_nbt(protocol)
}

pub fn read_dimension(protocol: i32, reader: &mut Reader<'_>) -> io::Result<Dimension> {
    Ok(match dimension_form(protocol) {
        DimensionForm::Legacy => Dimension::Legacy(reader.i32()?),
        DimensionForm::Name => Dimension::Name(reader.str()?.to_owned()),
        DimensionForm::Element => match Nbt::read(reader, named(protocol))? {
            Some(Nbt::Compound(element)) => Dimension::Element(element),
            _ => return Err(invalid("dimension type is not a compound")),
        },
        DimensionForm::Id => Dimension::Id(reader.varint()?),
    })
}

pub fn write_dimension(
    protocol: i32,
    dimension: &Dimension,
    writer: &mut Writer,
) -> io::Result<()> {
    match (dimension_form(protocol), dimension) {
        (DimensionForm::Legacy, Dimension::Legacy(id)) => drop(writer.i32(*id)),
        (DimensionForm::Name, Dimension::Name(name)) => drop(writer.string(name)),
        (DimensionForm::Element, Dimension::Element(element)) => {
            Nbt::Compound(element.clone()).write(writer, named(protocol))
        }
        (DimensionForm::Id, Dimension::Id(id)) => drop(writer.varint(*id)),
        _ => return Err(invalid("dimension is not in this protocol's form")),
    }
    Ok(())
}

/// The legacy number of a dimension type: -1 for nether-like, 1 for
/// end-like, else 0 (by its name, its `effects` or, from 1.21.11, its
/// skybox and timelines).
pub fn legacy_id(name: Option<&str>, element: Option<&Compound>) -> i32 {
    let effects = element.and_then(|element| {
        element
            .str("effects")
            .map(crate::data::strip)
            .or(match element.str("skybox") {
                Some("end") => Some("the_end"),
                _ if element.str("cardinal_light") == Some("nether") => Some("the_nether"),
                _ => None,
            })
    });
    match effects.or(name.map(crate::data::strip)) {
        Some("the_nether") => -1,
        Some("the_end") => 1,
        _ => 0,
    }
}

/// The vanilla dimension type name of a legacy dimension number.
pub fn legacy_name(id: i32) -> &'static str {
    match id {
        -1 => "minecraft:the_nether",
        1 => "minecraft:the_end",
        _ => "minecraft:overworld",
    }
}

/// The name of a dimension type element as `side` knows it: the entry of
/// its synchronised `dimension_type` registry that is equal, else the
/// vanilla type it looks like.
fn name_of_element(shared: &Shared, side: Side, element: &Compound) -> String {
    if let Some(name) = super::find_element(shared, side, "dimension_type", element) {
        return name.to_owned();
    }
    legacy_name(legacy_id(None, Some(element))).to_owned()
}

/// The dimension type element `side` knows by `name`: from its synchronised
/// registry, else its version's vanilla one, else the vanilla overworld.
fn element_of_name(shared: &Shared, side: Side, name: &str) -> io::Result<Compound> {
    let protocol = super::protocol(shared, side);
    if let Some(element) = super::element(shared, side, "dimension_type", name) {
        return Ok(element.clone());
    }
    defaults::element(protocol, "dimension_type", name)
        .or_else(|| {
            defaults::element(
                protocol,
                "dimension_type",
                legacy_name(legacy_id(Some(name), None)),
            )
        })
        .cloned()
        .ok_or_else(|| invalid("no dimension type to send"))
}

/// The dimension in the sender's form as the receiver's form.
pub fn convert_dimension(
    shared: &Shared,
    direction: Direction,
    dimension: Dimension,
) -> io::Result<Dimension> {
    let (from, to) = (shared.source(direction), shared.target(direction));
    let (sender, receiver) = (Side::sender(direction), Side::receiver(direction));
    // Every form reduces to the type's name in the sender's terms first.
    let name = match &dimension {
        Dimension::Legacy(id) => legacy_name(*id).to_owned(),
        Dimension::Name(name) => name.clone(),
        Dimension::Element(element) => name_of_element(shared, sender, element),
        Dimension::Id(id) => super::name_of(shared, sender, "dimension_type", *id)
            .ok_or_else(|| invalid("dimension type ID is not in the registry"))?
            .to_owned(),
    };
    Ok(match dimension_form(to) {
        DimensionForm::Legacy => {
            let element = match &dimension {
                Dimension::Element(element) => Some(element.clone()),
                _ => super::element(shared, sender, "dimension_type", &name).cloned(),
            };
            Dimension::Legacy(legacy_id(Some(&name), element.as_ref()))
        }
        // Registry entries are kept without the namespace the game writes.
        DimensionForm::Name => Dimension::Name(form::namespaced(&name)),
        DimensionForm::Element => match dimension {
            // A type in the packet need not be in the codec: convert it.
            Dimension::Element(element) if dimension_form(from) == DimensionForm::Element => {
                match convert::element_for(
                    "dimension_type",
                    from,
                    to,
                    &name,
                    Nbt::Compound(element),
                )? {
                    Nbt::Compound(element) => Dimension::Element(element),
                    _ => return Err(invalid("dimension type is not a compound")),
                }
            }
            _ => Dimension::Element(element_of_name(shared, receiver, &name)?),
        },
        DimensionForm::Id => Dimension::Id(
            super::id_of(shared, receiver, "dimension_type", &name)
                .ok_or_else(|| invalid("dimension type is not in the client's registry"))?,
        ),
    })
}

/// The registries of a 1.16–1.20.1 `login` codec (named root).
pub fn read_codec(protocol: i32, reader: &mut Reader<'_>) -> io::Result<Vec<Registry>> {
    match Nbt::read(reader, named(protocol))? {
        Some(codec) => form::read_codec(protocol, &codec),
        None => Err(invalid("login without a registry codec")),
    }
}

pub fn write_codec(protocol: i32, registries: &[Registry], writer: &mut Writer) -> io::Result<()> {
    form::write_codec(protocol, registries)?.write(writer, named(protocol));
    Ok(())
}

/// Convert a whole codec's registries for the receiver and record both
/// sides. For steps that move a codec between `login` and configuration
/// (1.20.1/1.20.2) as well as for [`transcode_login`].
pub fn convert_codec(
    shared: &mut Shared,
    direction: Direction,
    registries: &[Registry],
) -> io::Result<Vec<Registry>> {
    let (from, to) = (shared.source(direction), shared.target(direction));
    for registry in registries {
        record(shared, Side::sender(direction), registry);
    }
    let converted = convert::registries(from, to, registries)?;
    for registry in &converted {
        record(shared, Side::receiver(direction), registry);
    }
    Ok(converted)
}

/// Read the codec at `rw.r` in the sender's protocol and write it in the
/// receiver's, both 1.16–1.20.1.
pub fn transcode_codec(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<()> {
    let (from, to) = (shared.source(direction), shared.target(direction));
    let registries = read_codec(from, &mut rw.r)?;
    let converted = convert_codec(shared, direction, &registries)?;
    write_codec(to, &converted, &mut rw.w)
}

/// Read a dimension at `rw.r`, convert it and write it, recording where the
/// player now is on both sides.
pub fn transcode_dimension(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
    world: Option<&str>,
) -> io::Result<()> {
    let (from, to) = (shared.source(direction), shared.target(direction));
    let dimension = read_dimension(from, &mut rw.r)?;
    let converted = convert_dimension(shared, direction, dimension.clone())?;
    write_dimension(to, &converted, &mut rw.w)?;
    record_dimension(
        shared,
        Side::sender(direction),
        dimension,
        world.map(str::to_owned),
    );
    record_dimension(
        shared,
        Side::receiver(direction),
        converted,
        world.map(str::to_owned),
    );
    Ok(())
}

/// The layout of the fields `login` starts with, up to and including the
/// world name, apart from the registry parts' own forms.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Prefix {
    /// Entity ID, game mode, legacy dimension.
    Legacy,
    /// Entity ID, game mode, previous game mode, worlds, codec, dimension, world.
    Codec,
    /// The same with a hardcore flag after the entity ID (1.16.2 to 1.20.1).
    HardcoreCodec,
    /// Entity ID, hardcore, worlds, three VarInts, three flags, dimension, world.
    Configured,
}

fn prefix(protocol: i32) -> Prefix {
    match protocol {
        ..735 => Prefix::Legacy,
        735..751 => Prefix::Codec,
        751..764 => Prefix::HardcoreCodec,
        _ => Prefix::Configured,
    }
}

/// Whether [`transcode_login`] can convert the start of `login` between the
/// pair's protocols: their fields before the world name are the same
/// apart from the registry parts.
pub fn transcodes_login(shared: &Shared) -> bool {
    prefix(shared.client) == prefix(shared.backend)
}

/// What a `login` says about registries, read from the start of the packet.
#[derive(Clone, Debug, PartialEq)]
pub struct Login {
    pub world_names: Vec<String>,
    /// The codec of 1.16 to 1.20.1.
    pub codec: Option<Vec<Registry>>,
    pub dimension: Dimension,
    /// The world (dimension) name from 1.16 on.
    pub world: Option<String>,
}

/// Read the registry parts at the start of a `login` body of `protocol`.
/// The rest of the packet is left unread.
pub fn read_login(protocol: i32, reader: &mut Reader<'_>) -> io::Result<Login> {
    reader.i32()?;
    let layout = prefix(protocol);
    if layout == Prefix::Legacy {
        reader.u8()?;
        let dimension = read_dimension(protocol, reader)?;
        return Ok(Login {
            world_names: Vec::new(),
            codec: None,
            dimension,
            world: None,
        });
    }
    if layout != Prefix::Codec {
        reader.bool()?;
    }
    if layout != Prefix::Configured {
        reader.u8()?;
        reader.u8()?;
    }
    let count = reader.count()?;
    let world_names = (0..count)
        .map(|_| Ok(reader.str()?.to_owned()))
        .collect::<io::Result<_>>()?;
    let codec = if layout == Prefix::Configured {
        for _ in 0..3 {
            reader.varint()?;
        }
        for _ in 0..3 {
            reader.bool()?;
        }
        None
    } else {
        Some(read_codec(protocol, reader)?)
    };
    let dimension = read_dimension(protocol, reader)?;
    let world = Some(reader.str()?.to_owned());
    Ok(Login {
        world_names,
        codec,
        dimension,
        world,
    })
}

/// Read the dimension and world name at the start of a `respawn` body.
pub fn read_respawn(
    protocol: i32,
    reader: &mut Reader<'_>,
) -> io::Result<(Dimension, Option<String>)> {
    let dimension = read_dimension(protocol, reader)?;
    let world = if protocol >= 735 {
        Some(reader.str()?.to_owned())
    } else {
        None
    };
    Ok((dimension, world))
}

/// Convert the start of a `login` (through the world name) from the
/// sender's protocol to the receiver's, recording the registries and the
/// dimension; `rw` is left after the world name for the step to finish.
/// Only for pairs where [`transcodes_login`] holds; it fails otherwise.
pub fn transcode_login(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<()> {
    if !transcodes_login(shared) {
        return Err(invalid(
            "the start of login differs between these protocols",
        ));
    }
    let layout = prefix(shared.source(direction));
    rw.i32()?;
    if layout == Prefix::Legacy {
        rw.u8()?;
        return transcode_dimension(shared, direction, rw, None);
    }
    if layout != Prefix::Codec {
        rw.bool()?;
    }
    if layout != Prefix::Configured {
        rw.u8()?;
        rw.u8()?;
    }
    rw.list(|rw| rw.str().map(drop))?;
    if layout == Prefix::Configured {
        for _ in 0..3 {
            rw.varint()?;
        }
        for _ in 0..3 {
            rw.bool()?;
        }
    } else {
        transcode_codec(shared, direction, rw)?;
    }
    // The world name follows the dimension; peek at it to record both.
    let from = shared.source(direction);
    let mut ahead = rw.r;
    read_dimension(from, &mut ahead)?;
    let world = ahead.str()?.to_owned();
    transcode_dimension(shared, direction, rw, Some(&world))?;
    rw.str()?;
    Ok(())
}

/// Convert the start of a `respawn` (dimension and world name), recording
/// the dimension; `rw` is left after the world name.
pub fn transcode_respawn(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<()> {
    let (from, to) = (shared.source(direction), shared.target(direction));
    if (from < 735) != (to < 735) {
        return Err(invalid("respawn changes shape between these protocols"));
    }
    let mut ahead = rw.r;
    read_dimension(from, &mut ahead)?;
    let world = if from >= 735 {
        Some(ahead.str()?.to_owned())
    } else {
        None
    };
    transcode_dimension(shared, direction, rw, world.as_deref())?;
    if world.is_some() {
        rw.str()?;
    }
    Ok(())
}

/// The registries a client of `protocol` gets when the backend sends none
/// (going up across 1.16): every vanilla registry of that version.
pub fn default_codec(protocol: i32) -> io::Result<Nbt> {
    form::write_codec(protocol, &defaults::registries(protocol))
}

/// Whether a `login` from the backend must have its registry parts
/// converted for the client: the pair's codec or dimension forms differ or
/// some registry element changed format.
pub fn needs_conversion(shared: &Shared) -> bool {
    let (client, backend) = (shared.client, shared.backend);
    dimension_form(client) != dimension_form(backend)
        || form::sync(client) != form::sync(backend)
        || super::registries_differ(client, backend)
}
