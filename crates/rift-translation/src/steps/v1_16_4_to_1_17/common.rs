//! What the steps from 1.16.4 to 1.18.2 share: the join game (`login`) and
//! `respawn` layouts of 1.16.2 to 1.18.2, edits to the dimension registry
//! they carry, pre-1.20.5 item stacks as raw bytes, and sending a packet
//! under another name than the one it arrived as.
//!
//! The dimension registry edits stand in for the synchronised-registries
//! codec, which is to provide conversions of the registry parts of `login`
//! and `respawn` for every protocol.
use crate::{
    Direction, Packet, State,
    codec::{self, Shared},
    data,
    nbt::{Compound, Nbt},
    step::Cx,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::io;

/// Send `data` to the client as the packet `name` of `protocol`, the step's
/// client side.
pub fn to_client(cx: &mut Cx<'_>, protocol: i32, state: State, name: &str, data: Vec<u8>) {
    let id = data::version(protocol).expect_packet(state, Direction::Clientbound, name);
    cx.to_client(state, Packet::new(id, data));
}

/// Send `data` to the backend as the packet `name` of `protocol`, the step's
/// backend side.
pub fn to_backend(cx: &mut Cx<'_>, protocol: i32, state: State, name: &str, data: Vec<u8>) {
    let id = data::version(protocol).expect_packet(state, Direction::Serverbound, name);
    cx.to_backend(state, Packet::new(id, data));
}

/// Copy one JSON text component from `rw.r` to `rw.w`, converting it
/// between the step's protocols.
// INTEGRATION: the text codec's `transcode`, a stub that fails until the
// text codec lands; packets these steps split or merge carry their text
// through it.
pub fn text(shared: &mut Shared, direction: Direction, rw: &mut Rewriter<'_>) -> io::Result<()> {
    #[cfg(test)]
    if testing::stand_ins() {
        rw.str()?;
        return Ok(());
    }
    codec::text::transcode(shared, direction, rw)
}

/// Copy one item stack from `rw.r` to `rw.w`, converting it between the
/// step's protocols.
// INTEGRATION: the item codec's `transcode`, a stub that fails until the
// item codec lands.
pub fn item(shared: &mut Shared, direction: Direction, rw: &mut Rewriter<'_>) -> io::Result<()> {
    #[cfg(test)]
    if testing::stand_ins() {
        let item = raw_item(&mut rw.r)?;
        rw.w.bytes(item);
        return Ok(());
    }
    codec::item::transcode(shared, direction, rw)
}

/// The empty item stack before 1.20.5: a false presence flag.
pub const EMPTY_ITEM: &[u8] = &[0];

/// The raw bytes of one item stack as 1.13.2 to 1.20.4 send it: a presence
/// flag, then the item ID, count and named NBT tag.
pub fn raw_item<'a>(reader: &mut Reader<'a>) -> io::Result<&'a [u8]> {
    let start = reader.peek();
    if reader.bool()? {
        reader.varint()?;
        reader.i8()?;
        reader.nbt_bytes(true)?;
    }
    Ok(&start[..start.len() - reader.remaining()])
}

/// The join game packet of 1.16.2 to 1.18.2 (protocols 751 to 758).
#[derive(Clone, Debug, PartialEq)]
pub struct Login {
    pub entity_id: i32,
    pub hardcore: bool,
    pub game_mode: u8,
    pub previous_game_mode: i8,
    pub worlds: Vec<String>,
    /// The dimension codec: dimension types and biomes.
    pub registries: Nbt,
    /// The type of the dimension the player joins.
    pub dimension: Nbt,
    pub world: String,
    pub seed: i64,
    pub max_players: i32,
    pub view_distance: i32,
    /// Sent from 1.18 on; older backends simulate as far as they send.
    pub simulation_distance: i32,
    pub reduced_debug_info: bool,
    pub respawn_screen: bool,
    pub debug: bool,
    pub flat: bool,
}

fn nbt(reader: &mut Reader<'_>) -> io::Result<Nbt> {
    Nbt::read(reader, true)?.ok_or_else(|| invalid("missing NBT compound"))
}

impl Login {
    pub fn read(protocol: i32, data: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(data);
        let entity_id = r.i32()?;
        let hardcore = r.bool()?;
        let game_mode = r.u8()?;
        let previous_game_mode = r.i8()?;
        let worlds = (0..r.count()?)
            .map(|_| r.str().map(str::to_owned))
            .collect::<io::Result<_>>()?;
        let registries = nbt(&mut r)?;
        let dimension = nbt(&mut r)?;
        let world = r.str()?.to_owned();
        let seed = r.i64()?;
        let max_players = r.varint()?;
        let view_distance = r.varint()?;
        let simulation_distance = if protocol >= 757 {
            r.varint()?
        } else {
            view_distance
        };
        let login = Self {
            entity_id,
            hardcore,
            game_mode,
            previous_game_mode,
            worlds,
            registries,
            dimension,
            world,
            seed,
            max_players,
            view_distance,
            simulation_distance,
            reduced_debug_info: r.bool()?,
            respawn_screen: r.bool()?,
            debug: r.bool()?,
            flat: r.bool()?,
        };
        r.finish()?;
        Ok(login)
    }

    pub fn write(&self, protocol: i32) -> Vec<u8> {
        let mut w = Writer::new();
        w.i32(self.entity_id)
            .bool(self.hardcore)
            .u8(self.game_mode)
            .i8(self.previous_game_mode)
            .varint(self.worlds.len() as i32);
        for world in &self.worlds {
            w.string(world);
        }
        self.registries.write(&mut w, true);
        self.dimension.write(&mut w, true);
        w.string(&self.world)
            .i64(self.seed)
            .varint(self.max_players)
            .varint(self.view_distance);
        if protocol >= 757 {
            w.varint(self.simulation_distance);
        }
        w.bool(self.reduced_debug_info)
            .bool(self.respawn_screen)
            .bool(self.debug)
            .bool(self.flat);
        w.into_inner()
    }

    /// Apply `edit` to every dimension type: the registry's and the joined
    /// dimension's.
    pub fn edit_dimension_types(
        &mut self,
        mut edit: impl FnMut(&mut Compound) -> io::Result<()>,
    ) -> io::Result<()> {
        for element in elements(&mut self.registries, "dimension_type")? {
            edit(element)?;
        }
        edit(compound(&mut self.dimension)?)
    }

    /// Apply `edit` to every biome of the registry.
    pub fn edit_biomes(
        &mut self,
        mut edit: impl FnMut(&mut Compound) -> io::Result<()>,
    ) -> io::Result<()> {
        for element in elements(&mut self.registries, "worldgen/biome")? {
            edit(element)?;
        }
        Ok(())
    }
}

/// The respawn packet of 1.16.2 to 1.18.2: only the dimension type changes
/// between these protocols, so the rest is checked and copied.
pub struct Respawn {
    pub dimension: Nbt,
    pub world: String,
    rest: Vec<u8>,
}

impl Respawn {
    pub fn read(data: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(data);
        let dimension = nbt(&mut r)?;
        let world = r.str()?.to_owned();
        let rest = r.peek();
        // Seed, game mode, previous game mode, debug, flat, keep data.
        r.i64()?;
        r.u8()?;
        r.i8()?;
        for _ in 0..3 {
            r.bool()?;
        }
        r.finish()?;
        Ok(Self {
            dimension,
            world,
            rest: rest.to_vec(),
        })
    }

    pub fn write(&self) -> Vec<u8> {
        let mut w = Writer::new();
        self.dimension.write(&mut w, true);
        w.string(&self.world).bytes(&self.rest);
        w.into_inner()
    }

    pub fn dimension_type(&mut self) -> io::Result<&mut Compound> {
        compound(&mut self.dimension)
    }
}

fn compound(tag: &mut Nbt) -> io::Result<&mut Compound> {
    tag.as_compound_mut()
        .ok_or_else(|| invalid("dimension type is not a compound"))
}

/// The `element` compound of every entry of `registry` (without namespace)
/// in a dimension codec.
fn elements<'a>(registries: &'a mut Nbt, registry: &str) -> io::Result<Vec<&'a mut Compound>> {
    let entries = registries
        .as_compound_mut()
        .and_then(|root| root.compound_mut(&format!("minecraft:{registry}")))
        .and_then(|registry| registry.get_mut("value"))
        .and_then(Nbt::as_list_mut)
        .ok_or_else(|| invalid("dimension codec lacks a registry"))?;
    entries
        .iter_mut()
        .map(|entry| {
            entry
                .as_compound_mut()
                .and_then(|entry| entry.compound_mut("element"))
                .ok_or_else(|| invalid("registry entry without element"))
        })
        .collect()
}

/// The highest `logical_height` a 1.16 client accepts.
const LEGACY_HEIGHT: i32 = 256;

/// 1.17 dimension types state the world's vertical extent; before, every
/// world spanned 0 to 256.
pub fn add_world_height(dimension_type: &mut Compound) -> io::Result<()> {
    if !dimension_type.contains("min_y") {
        dimension_type.insert("min_y", 0);
    }
    if !dimension_type.contains("height") {
        dimension_type.insert("height", LEGACY_HEIGHT);
    }
    Ok(())
}

/// The reverse of [`add_world_height`]. A 1.16 client only knows worlds from
/// 0 to 256 and rejects a larger logical height; what lies outside that
/// range is the chunk codec's to cut.
pub fn remove_world_height(dimension_type: &mut Compound) -> io::Result<()> {
    dimension_type.remove("min_y");
    dimension_type.remove("height");
    if let Some(Nbt::Int(height)) = dimension_type.get_mut("logical_height") {
        *height = (*height).clamp(0, LEGACY_HEIGHT);
    }
    Ok(())
}

/// Biome categories a 1.16 client knows.
pub const CATEGORIES_1_16: &[&str] = &[
    "none",
    "taiga",
    "extreme_hills",
    "jungle",
    "mesa",
    "plains",
    "savanna",
    "icy",
    "the_end",
    "beach",
    "forest",
    "ocean",
    "desert",
    "river",
    "swamp",
    "mushroom",
    "nether",
];

/// Biome categories a 1.17 client knows: 1.16's and the caves'.
pub const CATEGORIES_1_17: &[&str] = &[
    "none",
    "taiga",
    "extreme_hills",
    "jungle",
    "mesa",
    "plains",
    "savanna",
    "icy",
    "the_end",
    "beach",
    "forest",
    "ocean",
    "desert",
    "river",
    "swamp",
    "mushroom",
    "nether",
    "underground",
];

/// Give a biome a category the receiver knows: an unknown category fails
/// the whole dimension codec on the client. 1.18's mountains were extreme
/// hills before; anything else unknown becomes `none`.
pub fn known_category(biome: &mut Compound, known: &[&str]) -> io::Result<()> {
    let Some(Nbt::String(category)) = biome.get_mut("category") else {
        return Err(invalid("biome without category"));
    };
    if !known.contains(&category.as_str()) {
        *category = if category == "mountain" && known.contains(&"extreme_hills") {
            "extreme_hills"
        } else {
            "none"
        }
        .to_owned();
    }
    Ok(())
}

/// Before 1.18 every biome stated the terrain depth and scale it generated;
/// clients require both but do not use them. Plains' values stand in.
pub fn add_terrain_shape(biome: &mut Compound) -> io::Result<()> {
    if !biome.contains("depth") {
        biome.insert("depth", 0.125f32);
    }
    if !biome.contains("scale") {
        biome.insert("scale", 0.05f32);
    }
    Ok(())
}

/// The reverse of [`add_terrain_shape`]: 1.18 biomes no longer carry them.
pub fn remove_terrain_shape(biome: &mut Compound) -> io::Result<()> {
    biome.remove("depth");
    biome.remove("scale");
    Ok(())
}

/// 1.18.2 names the blocks that burn forever in a dimension by a tag
/// reference (`#minecraft:infiniburn_overworld`), 1.18 by the tag's bare ID.
pub fn tag_infiniburn(dimension_type: &mut Compound, hashed: bool) -> io::Result<()> {
    let Some(Nbt::String(infiniburn)) = dimension_type.get_mut("infiniburn") else {
        return Err(invalid("dimension type without infiniburn"));
    };
    match (hashed, infiniburn.strip_prefix('#')) {
        (true, None) => infiniburn.insert(0, '#'),
        (false, Some(bare)) => *infiniburn = bare.to_owned(),
        _ => {}
    }
    Ok(())
}

/// Running a step on single packets and on recorded sessions, for the
/// tests of the steps from 1.16.4 to 1.18.2.
#[cfg(test)]
pub mod testing {
    use crate::{
        Direction, Packet, State,
        codec::testing::corpus,
        data,
        step::{Cx, Emitted, Step},
    };
    use std::{cell::Cell, collections::BTreeMap, collections::VecDeque, io};

    thread_local! {
        static STAND_INS: Cell<bool> = const { Cell::new(false) };
    }

    /// Whether [`super::item`] and [`super::text`] copy stacks and text
    /// unchanged on this thread instead of calling the shared codecs: for
    /// testing what a step does around them, before the codecs exist.
    pub fn stand_ins() -> bool {
        STAND_INS.with(Cell::get)
    }

    /// Run `test` with stand-ins for the item and text codecs.
    pub fn with_stand_ins<T>(test: impl FnOnce() -> T) -> T {
        STAND_INS.with(|flag| flag.set(true));
        let result = test();
        STAND_INS.with(|flag| flag.set(false));
        result
    }

    /// A packet a step emitted, named in the protocol it is in.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Out {
        pub direction: Direction,
        pub state: State,
        pub name: String,
        pub data: Vec<u8>,
    }

    /// Feed the packet `name` (of the sender's protocol) to `step`, which
    /// sits between `client` and `backend`.
    pub fn send(
        step: &mut dyn Step,
        (client, backend): (i32, i32),
        direction: Direction,
        state: State,
        name: &str,
        body: &[u8],
    ) -> io::Result<Vec<Out>> {
        let source = match direction {
            Direction::Clientbound => backend,
            Direction::Serverbound => client,
        };
        let id = data::version(source).expect_packet(state, direction, name);
        send_id(step, (client, backend), direction, state, id, body)
    }

    pub fn send_id(
        step: &mut dyn Step,
        (client, backend): (i32, i32),
        direction: Direction,
        state: State,
        id: i32,
        body: &[u8],
    ) -> io::Result<Vec<Out>> {
        let mut emitted = VecDeque::new();
        let mut cx = Cx {
            emitted: &mut emitted,
            client_protocol: client,
            backend_protocol: backend,
        };
        let packet = Packet::new(id, body.to_vec());
        match direction {
            Direction::Clientbound => step.clientbound(&mut cx, state, packet)?,
            Direction::Serverbound => step.serverbound(&mut cx, state, packet)?,
        }
        Ok(emitted
            .into_iter()
            .map(
                |Emitted {
                     direction,
                     state,
                     packet,
                 }| {
                    let protocol = match direction {
                        Direction::Clientbound => client,
                        Direction::Serverbound => backend,
                    };
                    Out {
                        direction,
                        state,
                        name: data::version(protocol)
                            .packet_name(state, direction, packet.id)
                            .unwrap_or("?")
                            .to_owned(),
                        data: packet.data,
                    }
                },
            )
            .collect())
    }

    /// What replaying one direction of a recorded session did, per packet
    /// name of the sender's protocol.
    #[derive(Debug, Default)]
    pub struct Replay {
        pub passed: BTreeMap<String, usize>,
        /// Failures inside a shared codec that has not landed yet.
        pub awaiting_codec: BTreeMap<String, usize>,
        pub failed: BTreeMap<String, String>,
    }

    /// Replay every `direction` packet the official `recorded` server's
    /// session holds through `step`. `None` without the corpus.
    pub fn replay(
        step: &mut dyn Step,
        sides: (i32, i32),
        recorded: i32,
        direction: Direction,
    ) -> Option<Replay> {
        let packets = corpus(recorded)?;
        let mut replay = Replay::default();
        for packet in packets
            .iter()
            .filter(|packet| packet.direction == direction)
        {
            let name = packet.name(recorded).to_owned();
            match send_id(
                step,
                sides,
                direction,
                packet.state,
                packet.id,
                &packet.data,
            ) {
                Ok(_) => *replay.passed.entry(name).or_default() += 1,
                Err(error) if error.to_string().contains("not implemented") => {
                    *replay.awaiting_codec.entry(name).or_default() += 1
                }
                Err(error) => {
                    replay
                        .failed
                        .entry(name)
                        .or_insert_with(|| error.to_string());
                }
            }
        }
        Some(replay)
    }

    /// The bodies of every `name` packet in `protocol`'s recorded session.
    pub fn recorded(protocol: i32, direction: Direction, name: &str) -> Vec<Vec<u8>> {
        let Some(packets) = corpus(protocol) else {
            return Vec::new();
        };
        packets
            .into_iter()
            .filter(|packet| packet.direction == direction && packet.name(protocol) == name)
            .map(|packet| packet.data)
            .collect()
    }

    /// Whether a step's error is a shared codec that has not landed yet.
    pub fn awaiting_codec(error: &io::Error) -> bool {
        error.to_string().contains("not implemented")
    }

    pub fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::testing::corpus;

    fn login_body(protocol: i32) -> Option<Vec<u8>> {
        let packets = corpus(protocol)?;
        let id =
            data::version(protocol).expect_packet(State::Play, Direction::Clientbound, "login");
        packets
            .into_iter()
            .find(|packet| {
                packet.state == State::Play
                    && packet.direction == Direction::Clientbound
                    && packet.id == id
            })
            .map(|packet| packet.data)
    }

    #[test]
    fn login_and_respawn_round_trip_on_real_sessions() {
        for protocol in 754..=758 {
            let Some(body) = login_body(protocol) else {
                eprintln!("no corpus for {protocol}; skipped");
                continue;
            };
            let login = Login::read(protocol, &body).unwrap();
            assert_eq!(login.write(protocol), body, "{protocol}");
            let packets = corpus(protocol).unwrap();
            let respawn = data::version(protocol).expect_packet(
                State::Play,
                Direction::Clientbound,
                "respawn",
            );
            for packet in packets.iter().filter(|packet| {
                packet.direction == Direction::Clientbound
                    && packet.state == State::Play
                    && packet.id == respawn
            }) {
                assert_eq!(Respawn::read(&packet.data).unwrap().write(), packet.data);
            }
        }
    }

    #[test]
    fn dimension_type_edits_match_what_each_server_sends() {
        let (Some(old), Some(new)) = (login_body(754), login_body(755)) else {
            eprintln!("no corpus for 754/755; skipped");
            return;
        };
        let (mut old, new) = (
            Login::read(754, &old).unwrap(),
            Login::read(755, &new).unwrap(),
        );
        old.edit_dimension_types(add_world_height).unwrap();
        let types = |login: &mut Login| {
            let mut all = Vec::new();
            login
                .edit_dimension_types(|element| {
                    let mut sorted: Vec<_> =
                        element.iter().map(|(k, v)| format!("{k}={v:?}")).collect();
                    sorted.sort();
                    all.push(sorted);
                    Ok(())
                })
                .unwrap();
            all
        };
        assert_eq!(types(&mut old), types(&mut new.clone()));
        let mut back = new.clone();
        back.edit_dimension_types(remove_world_height).unwrap();
        back.edit_biomes(|biome| known_category(biome, CATEGORIES_1_16))
            .unwrap();
        let mut seen = 0;
        back.edit_biomes(|biome| {
            assert!(CATEGORIES_1_16.contains(&biome.str("category").unwrap()));
            seen += 1;
            Ok(())
        })
        .unwrap();
        assert!(seen > 70);
    }

    #[test]
    fn infiniburn_gains_and_loses_its_hash() {
        let mut dimension = Compound::new();
        dimension.insert("infiniburn", "minecraft:infiniburn_end");
        tag_infiniburn(&mut dimension, true).unwrap();
        assert_eq!(
            dimension.str("infiniburn"),
            Some("#minecraft:infiniburn_end")
        );
        tag_infiniburn(&mut dimension, true).unwrap();
        assert_eq!(
            dimension.str("infiniburn"),
            Some("#minecraft:infiniburn_end")
        );
        tag_infiniburn(&mut dimension, false).unwrap();
        assert_eq!(
            dimension.str("infiniburn"),
            Some("minecraft:infiniburn_end")
        );
        assert!(tag_infiniburn(&mut Compound::new(), true).is_err());
    }

    #[test]
    fn categories_and_terrain_shape_fit_older_clients() {
        let mut biome = Compound::new();
        biome.insert("category", "mountain");
        known_category(&mut biome, CATEGORIES_1_17).unwrap();
        assert_eq!(biome.str("category"), Some("extreme_hills"));
        biome.insert("category", "underground");
        known_category(&mut biome, CATEGORIES_1_17).unwrap();
        assert_eq!(biome.str("category"), Some("underground"));
        known_category(&mut biome, CATEGORIES_1_16).unwrap();
        assert_eq!(biome.str("category"), Some("none"));
        add_terrain_shape(&mut biome).unwrap();
        assert_eq!(biome.get("depth"), Some(&Nbt::Float(0.125)));
        remove_terrain_shape(&mut biome).unwrap();
        assert!(!biome.contains("scale"));
    }

    #[test]
    fn malformed_logins_fail() {
        assert!(Login::read(754, &[]).is_err());
        assert!(Login::read(754, &[0, 0, 0, 1, 0, 1, 0xff, 0x7f]).is_err());
        assert!(Respawn::read(&[10, 0, 0, 0]).is_err());
    }
}
