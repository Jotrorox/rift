//! Synchronised registries: the dimension list of 1.16, the registry codec
//! of 1.16.2 to 1.20.4 and the per-registry `registry_data` packets of
//! 1.20.5 on, with each version's vanilla contents (see [`defaults`]).
//!
//! # What the defaults do
//!
//! For every pair this codec converts `registry_data` (configuration,
//! clientbound) whenever the two protocols' registries differ: element
//! formats are converted (see [`convert`]), registries the client's version
//! expects but the backend's does not send are filled in from the client
//! version's vanilla contents, registries the client's version does not
//! know are dropped, and vanilla entries the client expects are appended.
//! Pairs whose registries are identical pass the packet through and only
//! record it. Across 1.20.4/1.20.5 the one-codec packet becomes one packet
//! per registry and back (a 1.20.4 client gets the codec once the backend
//! has sent every registry its version has; see [`take_pending`]); across
//! 1.20.1/1.20.2 the codec is held for the step's `login` (see
//! [`take_pending`] and [`login`]).
//!
//! `login` and `respawn` belong to the steps; this codec taps them to follow
//! the player's dimension and, for 1.16 to 1.20.1, the codec they carry.
//! [`login`] has the functions a step's handlers use to convert their
//! registry parts.
//!
//! # Known packs
//!
//! From 1.20.5 a `registry_data` entry may come without data when client
//! and server agreed on a pack that holds it. Translation only ever sees
//! servers and clients of different versions, which never agree on a pack
//! version, and the step owning `select_known_packs` answers for clients
//! that cannot; should data-less entries arrive anyway they take the
//! sender version's vanilla element (and fail if the entry is not vanilla).
//! Converted packets always carry full data.
//!
//! # Network IDs
//!
//! Other codecs translate references to synchronised entries (biomes in
//! chunks, dimension types, damage, chat and mob variant types, enchantment
//! and trim holders...) through [`map`], [`name_of`] and [`id_of`]. Each
//! side's IDs are what it was last sent; on a side whose protocol does not
//! synchronise a registry they are the protocol's built-in IDs (biomes before
//! 1.16.2, enchantments before 1.21...), and before any registry data
//! arrives they are the vanilla ones. The client side lists the backend's
//! entries in the backend's order with missing vanilla entries after them,
//! so most IDs map to themselves.
pub mod convert;
pub mod defaults;
pub mod form;
pub mod login;
pub mod snbt;
#[cfg(test)]
mod tests;
mod transform;

pub use form::{Entry, Registry};
pub use login::Dimension;

use super::{HasShared, Shared};
use crate::{
    Direction, Packet,
    data::{self, strip},
    nbt::{Compound, Nbt},
    step::{Cx, RouteBuilder},
    wire::Reader,
};
use std::{collections::HashMap, io, sync::Arc};

/// One end of a step: the client side or the backend side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Client,
    Backend,
}

impl Side {
    /// The side packets travelling in `direction` come from.
    pub fn sender(direction: Direction) -> Self {
        match direction {
            Direction::Clientbound => Side::Backend,
            Direction::Serverbound => Side::Client,
        }
    }

    /// The side packets travelling in `direction` go to.
    pub fn receiver(direction: Direction) -> Self {
        match direction {
            Direction::Clientbound => Side::Client,
            Direction::Serverbound => Side::Backend,
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// The protocol `side` of the step speaks.
pub fn protocol(shared: &Shared, side: Side) -> i32 {
    match side {
        Side::Client => shared.client,
        Side::Backend => shared.backend,
    }
}

/// Registries whose elements are kept per side because other codecs read
/// them: dimension types (heights, sky light) and chat types (decorations
/// for clients before 1.19).
const KEPT: &[&str] = &["dimension_type", "chat_type"];

/// What one side's network IDs of one registry mean.
#[derive(Debug, PartialEq)]
pub struct Synced {
    name: String,
    /// Network ID and stripped name, in registry order.
    entries: Vec<(i32, Box<str>)>,
    by_name: HashMap<Box<str>, usize>,
    /// IDs are positions (always from 1.20.5, usually before).
    dense: bool,
    /// Elements parallel to `entries` for [`KEPT`] registries.
    elements: Vec<Option<Arc<Nbt>>>,
}

impl Synced {
    pub fn new(registry: &Registry) -> Self {
        let keep = KEPT.contains(&registry.name.as_str());
        let entries: Vec<(i32, Box<str>)> = registry
            .entries
            .iter()
            .map(|entry| (entry.id, strip(&entry.name).into()))
            .collect();
        let dense = entries
            .iter()
            .enumerate()
            .all(|(index, (id, _))| *id == index as i32);
        let mut by_name = HashMap::with_capacity(entries.len());
        for (index, (_, name)) in entries.iter().enumerate() {
            by_name.entry(name.clone()).or_insert(index);
        }
        Self {
            name: registry.name.clone(),
            entries,
            by_name,
            dense,
            elements: if keep {
                registry
                    .entries
                    .iter()
                    .map(|entry| entry.element.clone().map(Arc::new))
                    .collect()
            } else {
                Vec::new()
            },
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn index_of_id(&self, id: i32) -> Option<usize> {
        if self.dense {
            let index = usize::try_from(id).ok()?;
            (index < self.entries.len()).then_some(index)
        } else {
            self.entries.iter().position(|(known, _)| *known == id)
        }
    }

    /// The entry name (without `minecraft:`) of network ID `id`.
    pub fn name_of(&self, id: i32) -> Option<&str> {
        Some(&self.entries[self.index_of_id(id)?].1)
    }

    /// The network ID of `name`, with or without its namespace.
    pub fn id_of(&self, name: &str) -> Option<i32> {
        Some(self.entries[*self.by_name.get(strip(name))?].0)
    }

    /// One past the highest network ID: the registry's size as the client
    /// sees it, which sets the bits of a direct biome palette.
    pub fn size(&self) -> usize {
        self.entries
            .iter()
            .map(|(id, _)| *id as usize + 1)
            .max()
            .unwrap_or(0)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Network IDs and names in registry order.
    pub fn iter(&self) -> impl Iterator<Item = (i32, &str)> {
        self.entries.iter().map(|(id, name)| (*id, &**name))
    }

    fn element(&self, name: &str) -> Option<&Compound> {
        let index = *self.by_name.get(strip(name))?;
        self.elements.get(index)?.as_deref()?.as_compound()
    }
}

/// What this codec keeps for one step of one connection.
#[derive(Default)]
pub struct State {
    sides: [SideState; 2],
    /// Registries received in the current 1.20.5+ configuration round; a
    /// name seen again starts a new round.
    round: Vec<String>,
    /// Converted registries held back: for a 1.20.4 client until the backend
    /// sent all of them, for a 1.20.1 client until the step writes `login`.
    pending: Vec<Registry>,
}

#[derive(Default)]
struct SideState {
    registries: Vec<Arc<Synced>>,
    dimension: Option<(Dimension, Option<String>)>,
}

impl State {
    fn side(&self, side: Side) -> &SideState {
        &self.sides[side.index()]
    }
}

/// Make `registry` what `side`'s network IDs of it mean from now on.
pub fn record(shared: &mut Shared, side: Side, registry: &Registry) {
    let synced = Synced::new(registry);
    let state = &mut shared.registries;
    let other = match side {
        Side::Client => Side::Backend,
        Side::Backend => Side::Client,
    };
    // Both sides usually agree; share one copy then.
    let synced = state
        .side(other)
        .registries
        .iter()
        .find(|known| ***known == synced)
        .cloned()
        .unwrap_or_else(|| Arc::new(synced));
    let registries = &mut state.sides[side.index()].registries;
    match registries
        .iter_mut()
        .find(|known| known.name == registry.name)
    {
        Some(slot) => *slot = synced,
        None => registries.push(synced),
    }
}

/// Record where `side` now has the player, as its `login` or `respawn`
/// put it.
pub fn record_dimension(
    shared: &mut Shared,
    side: Side,
    dimension: Dimension,
    world: Option<String>,
) {
    shared.registries.sides[side.index()].dimension = Some((dimension, world));
}

/// The registry `side` has synchronised as `registry`, if it was sent one.
pub fn synced<'a>(shared: &'a Shared, side: Side, registry: &str) -> Option<&'a Synced> {
    let registry = strip(registry);
    shared
        .registries
        .side(side)
        .registries
        .iter()
        .find(|known| known.name == registry)
        .map(|synced| &**synced)
}

/// The name of a synchronised registry in `protocol`'s built-in registry
/// table, for protocols that have it built in.
pub fn builtin_name(protocol: i32, registry: &str) -> &str {
    match strip(registry) {
        "worldgen/biome" if protocol < 751 => "biome",
        other => other,
    }
}

enum Ids<'a> {
    Synced(&'a Synced),
    Vanilla(&'static defaults::Defaults),
    Builtin(&'static data::Registry),
}

fn ids<'a>(shared: &'a Shared, side: Side, registry: &str) -> Option<Ids<'a>> {
    if let Some(synced) = synced(shared, side, registry) {
        return Some(Ids::Synced(synced));
    }
    let protocol = protocol(shared, side);
    if let Some(vanilla) = defaults::get(protocol, registry) {
        return Some(Ids::Vanilla(vanilla));
    }
    if strip(registry) == "dimension_type" {
        // Before 1.16 the wire has legacy numbers, not registry IDs.
        return None;
    }
    data::version(protocol)
        .registry(builtin_name(protocol, registry))
        .map(Ids::Builtin)
}

/// The entry name (without `minecraft:`) of network ID `id` in `side`'s
/// `registry`.
pub fn name_of<'a>(shared: &'a Shared, side: Side, registry: &str, id: i32) -> Option<&'a str> {
    match ids(shared, side, registry)? {
        Ids::Synced(synced) => synced.name_of(id),
        Ids::Vanilla(vanilla) => vanilla.by_id(id).map(|entry| strip(&entry.name)),
        Ids::Builtin(builtin) => builtin.name(id),
    }
}

/// The network ID of entry `name` in `side`'s `registry`.
pub fn id_of(shared: &Shared, side: Side, registry: &str, name: &str) -> Option<i32> {
    match ids(shared, side, registry)? {
        Ids::Synced(synced) => synced.id_of(name),
        Ids::Vanilla(vanilla) => vanilla.get(name).map(|entry| entry.id),
        Ids::Builtin(builtin) => builtin.id(name),
    }
}

/// The receiver's network ID for the sender's `id` in `registry`, by entry
/// name; `None` when the receiver has no such entry (the caller degrades:
/// a stand-in, a neutral value, or dropping the reference).
pub fn map(shared: &Shared, direction: Direction, registry: &str, id: i32) -> Option<i32> {
    let name = name_of(shared, Side::sender(direction), registry, id)?;
    id_of(shared, Side::receiver(direction), registry, name)
}

/// The size of `side`'s `registry` (one past its highest network ID).
pub fn size(shared: &Shared, side: Side, registry: &str) -> usize {
    match ids(shared, side, registry) {
        Some(Ids::Synced(synced)) => synced.size(),
        Some(Ids::Vanilla(vanilla)) => vanilla
            .entries
            .iter()
            .map(|entry| entry.id as usize + 1)
            .max()
            .unwrap_or(0),
        Some(Ids::Builtin(builtin)) => builtin.len(),
        None => 0,
    }
}

/// The element of entry `name` in `side`'s `registry`: what the side was
/// sent for [`KEPT`] registries, else its version's vanilla element.
pub fn element<'a>(
    shared: &'a Shared,
    side: Side,
    registry: &str,
    name: &str,
) -> Option<&'a Compound> {
    if let Some(synced) = synced(shared, side, registry)
        && KEPT.contains(&synced.name.as_str())
    {
        return synced.element(name);
    }
    defaults::element(protocol(shared, side), registry, name)
}

/// The name of the entry of `side`'s `registry` whose element is `element`.
pub fn find_element<'a>(
    shared: &'a Shared,
    side: Side,
    registry: &str,
    element: &Compound,
) -> Option<&'a str> {
    if let Some(synced) = synced(shared, side, registry) {
        return synced
            .elements
            .iter()
            .position(|known| known.as_deref().and_then(Nbt::as_compound) == Some(element))
            .map(|index| &*synced.entries[index].1);
    }
    defaults::get(protocol(shared, side), registry)?
        .entries
        .iter()
        .find(|entry| entry.element.as_compound() == Some(element))
        .map(|entry| strip(&entry.name))
}

/// The properties of a dimension type that chunks, light and positions
/// depend on, in any protocol.
#[derive(Clone, Debug, PartialEq)]
pub struct DimensionType {
    /// The type's name, when known.
    pub name: Option<String>,
    pub min_y: i32,
    pub height: i32,
    pub logical_height: i32,
    pub has_skylight: bool,
    pub has_ceiling: bool,
    pub coordinate_scale: f64,
    /// -1 (nether-like), 0 or 1 (end-like), as clients before 1.16 number it.
    pub legacy_id: i32,
}

impl DimensionType {
    /// From a dimension type element of any version (fields a version lacks
    /// take what that version implied: 256 blocks from y 0).
    pub fn from_element(name: Option<&str>, element: &Compound) -> Self {
        let flag = |key: &str, default: bool| {
            element
                .get(key)
                .and_then(crate::nbt::Nbt::as_i64)
                .map_or(default, |value| value != 0)
        };
        let height = element.int("height").unwrap_or(256);
        Self {
            name: name.map(str::to_owned),
            min_y: element.int("min_y").unwrap_or(0),
            height,
            logical_height: element.int("logical_height").unwrap_or(height),
            has_skylight: flag("has_skylight", true),
            has_ceiling: flag("has_ceiling", false),
            coordinate_scale: element
                .get("coordinate_scale")
                .and_then(crate::nbt::Nbt::as_f64)
                .unwrap_or_else(|| if flag("shrunk", false) { 8.0 } else { 1.0 }),
            legacy_id: login::legacy_id(name, Some(element)),
        }
    }

    /// A dimension before 1.16, by its legacy number.
    pub fn legacy(id: i32) -> Self {
        Self {
            name: Some(login::legacy_name(id).to_owned()),
            min_y: 0,
            height: 256,
            logical_height: if id == -1 { 128 } else { 256 },
            has_skylight: id == 0,
            has_ceiling: id == -1,
            coordinate_scale: if id == -1 { 8.0 } else { 1.0 },
            legacy_id: id.clamp(-1, 1),
        }
    }

    /// The number of 16-block chunk sections from `min_y` up.
    pub fn sections(&self) -> i32 {
        self.height.div_euclid(16)
    }
}

/// The dimension type `side` knows as `name`.
pub fn dimension_type(shared: &Shared, side: Side, name: &str) -> Option<DimensionType> {
    let element = element(shared, side, "dimension_type", name)?;
    Some(DimensionType::from_element(Some(strip(name)), element))
}

/// The type of the dimension `side` last put the player in by `login` or
/// `respawn`; `None` before the first `login`.
pub fn dimension(shared: &Shared, side: Side) -> Option<DimensionType> {
    let (dimension, _) = shared.registries.side(side).dimension.as_ref()?;
    match dimension {
        Dimension::Legacy(id) => Some(DimensionType::legacy(*id)),
        Dimension::Name(name) => dimension_type(shared, side, name),
        Dimension::Element(element) => {
            let name = find_element(shared, side, "dimension_type", element);
            Some(DimensionType::from_element(name, element))
        }
        Dimension::Id(id) => {
            let name = name_of(shared, side, "dimension_type", *id)?;
            dimension_type(shared, side, name)
        }
    }
}

/// The world (dimension) name `side` last put the player in, from 1.16 on.
pub fn world(shared: &Shared, side: Side) -> Option<&str> {
    shared
        .registries
        .side(side)
        .dimension
        .as_ref()?
        .1
        .as_deref()
}

/// The chat type `side` knows by network ID `id`, with its name.
pub fn chat_type(shared: &Shared, side: Side, id: i32) -> Option<(&str, &Compound)> {
    let name = name_of(shared, side, "chat_type", id)?;
    Some((name, element(shared, side, "chat_type", name)?))
}

/// Take the converted registries held back for the client: the codec of a
/// 1.20.1 client to write into its `login` (the step crossing 1.20.1/1.20.2
/// calls this), or, for a 1.20.4 client whose 1.20.5+ backend has not sent
/// every registry its version has, what arrived so far (the step may send it
/// as the codec when the backend finishes configuration).
pub fn take_pending(shared: &mut Shared) -> Vec<Registry> {
    std::mem::take(&mut shared.registries.pending)
}

/// Whether the registries of two protocols differ in any way a converter
/// must handle: which registries, which entries in which order, vanilla
/// elements, or a format rule for custom entries at the newer one.
pub fn registries_differ(a: i32, b: i32) -> bool {
    let (older, newer) = (a.min(b), a.max(b));
    if transform::BOUNDARIES.contains(&newer) {
        return true;
    }
    let (old, new) = (defaults::all(older), defaults::all(newer));
    old.len() != new.len()
        || old.iter().zip(new).any(|(old, new)| {
            old.name != new.name
                || old.entries.len() != new.entries.len()
                || old.entries.iter().zip(&new.entries).any(|(old, new)| {
                    old.name != new.name
                        || old.id != new.id
                        || !(Arc::ptr_eq(&old.element, &new.element) || old.element == new.element)
                })
        })
}

/// Install this codec's default routes for the builder's pair.
pub fn install<S: HasShared + 'static>(builder: RouteBuilder<S>) -> RouteBuilder<S> {
    use crate::State::{Configuration, Play};
    use Direction::Clientbound;
    use form::Sync;
    let (client, backend) = (builder.client().protocol, builder.backend().protocol);
    let builder = builder.tap(Clientbound, Play, "login", tap_login::<S>).tap(
        Clientbound,
        Play,
        "respawn",
        tap_respawn::<S>,
    );
    let differ = registries_differ(client, backend);
    match (form::sync(backend), form::sync(client)) {
        (Sync::Packets, Sync::Packets) if differ => {
            builder.default_handler(Clientbound, Configuration, "registry_data", packets::<S>)
        }
        (Sync::ConfigurationCodec, Sync::ConfigurationCodec) if differ => {
            builder.default_handler(Clientbound, Configuration, "registry_data", codec::<S>)
        }
        (Sync::ConfigurationCodec, Sync::Packets) => builder.default_handler(
            Clientbound,
            Configuration,
            "registry_data",
            codec_to_packets::<S>,
        ),
        (Sync::Packets, Sync::ConfigurationCodec) => builder.default_handler(
            Clientbound,
            Configuration,
            "registry_data",
            packets_to_codec::<S>,
        ),
        (Sync::ConfigurationCodec, Sync::LoginCodec) => builder.default_handler(
            Clientbound,
            Configuration,
            "registry_data",
            codec_for_login::<S>,
        ),
        (Sync::Packets | Sync::ConfigurationCodec, _) => builder.tap(
            Clientbound,
            Configuration,
            "registry_data",
            tap_registry_data::<S>,
        ),
        _ => builder,
    }
}

/// Record a backend `login`'s codec and dimension. The client side is
/// recorded as if the packet passed through; a step converting it with
/// [`login`]'s functions records the converted parts over this.
fn tap_login<S: HasShared>(step: &mut S, _: crate::State, packet: &Packet) -> io::Result<()> {
    let shared = step.shared();
    let parsed = login::read_login(shared.backend, &mut Reader::new(&packet.data))?;
    if let Some(codec) = &parsed.codec {
        for registry in codec {
            record(shared, Side::Backend, registry);
            // A client that does not synchronise a registry (biomes before
            // 1.16.2) keeps its built-in IDs.
            if defaults::syncs(shared.client, &registry.name) {
                record(shared, Side::Client, registry);
            }
        }
    }
    follow(shared, parsed.dimension, parsed.world);
    Ok(())
}

fn tap_respawn<S: HasShared>(step: &mut S, _: crate::State, packet: &Packet) -> io::Result<()> {
    let shared = step.shared();
    let (dimension, world) = login::read_respawn(shared.backend, &mut Reader::new(&packet.data))?;
    follow(shared, dimension, world);
    Ok(())
}

fn follow(shared: &mut Shared, dimension: Dimension, world: Option<String>) {
    let converted = login::convert_dimension(shared, Direction::Clientbound, dimension.clone())
        .unwrap_or_else(|_| dimension.clone());
    record_dimension(shared, Side::Backend, dimension, world.clone());
    record_dimension(shared, Side::Client, converted, world);
}

/// Record `registry_data` that passes through unchanged.
fn tap_registry_data<S: HasShared>(
    step: &mut S,
    _: crate::State,
    packet: &Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let registries = match form::sync(shared.backend) {
        form::Sync::Packets => vec![form::read_packet(&packet.data)?],
        _ => form::read_codec_packet(shared.backend, &packet.data)?,
    };
    for registry in &registries {
        record(shared, Side::Backend, registry);
        record(shared, Side::Client, registry);
    }
    Ok(())
}

/// Note a 1.20.5+ registry arriving; true when it starts a new round.
fn begin(shared: &mut Shared, name: &str) -> bool {
    let state = &mut shared.registries;
    let new_round = state.round.is_empty() || state.round.iter().any(|known| known == name);
    if new_round {
        state.round.clear();
        state.pending.clear();
    }
    state.round.push(name.to_owned());
    new_round
}

/// 1.20.5+ on both sides: convert each registry, and with the first of a
/// round send what the client's version has and the backend's lacks.
fn packets<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: crate::State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let (from, to) = (shared.backend, shared.client);
    let sent = form::read_packet(&packet.data)?;
    if begin(shared, &sent.name) {
        for registry in convert::missing(from, to) {
            record(shared, Side::Client, &registry);
            cx.to_client(state, Packet::new(packet.id, form::write_packet(&registry)));
        }
    }
    record(shared, Side::Backend, &sent);
    if let Some(converted) = convert::registry(from, to, &sent)? {
        record(shared, Side::Client, &converted);
        cx.to_client(
            state,
            Packet::new(packet.id, form::write_packet(&converted)),
        );
    }
    Ok(())
}

/// 1.20.2–1.20.4 on both sides: convert the codec.
fn codec<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: crate::State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let sent = form::read_codec_packet(shared.backend, &packet.data)?;
    let converted = login::convert_codec(shared, Direction::Clientbound, &sent)?;
    let body = form::write_codec_packet(shared.client, &converted)?;
    cx.to_client(state, Packet::new(packet.id, body));
    Ok(())
}

/// A 1.20.4 backend and a 1.20.5 client: one packet per registry.
fn codec_to_packets<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: crate::State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let sent = form::read_codec_packet(shared.backend, &packet.data)?;
    for registry in login::convert_codec(shared, Direction::Clientbound, &sent)? {
        cx.to_client(state, Packet::new(packet.id, form::write_packet(&registry)));
    }
    Ok(())
}

/// A 1.20.5 backend and a 1.20.4 client: collect the registries and send
/// the codec once every registry the backend's version has has arrived.
fn packets_to_codec<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: crate::State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let (from, to) = (shared.backend, shared.client);
    let sent = form::read_packet(&packet.data)?;
    begin(shared, &sent.name);
    record(shared, Side::Backend, &sent);
    if let Some(converted) = convert::registry(from, to, &sent)? {
        shared.registries.pending.push(converted);
    }
    let complete = defaults::all(from).iter().all(|vanilla| {
        shared
            .registries
            .round
            .iter()
            .any(|name| name == vanilla.name)
    });
    if complete {
        let pending = take_pending(shared);
        let registries = convert::assemble(to, pending);
        for registry in &registries {
            record(shared, Side::Client, registry);
        }
        let body = form::write_codec_packet(to, &registries)?;
        cx.to_client(state, Packet::new(packet.id, body));
    }
    Ok(())
}

/// A 1.20.2 backend and a 1.20.1 client: the client has no configuration
/// phase; convert the codec and hold it for the step's `login`.
fn codec_for_login<S: HasShared>(
    step: &mut S,
    _: &mut Cx<'_>,
    _: crate::State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let sent = form::read_codec_packet(shared.backend, &packet.data)?;
    let converted = login::convert_codec(shared, Direction::Clientbound, &sent)?;
    shared.registries.pending = converted;
    Ok(())
}
