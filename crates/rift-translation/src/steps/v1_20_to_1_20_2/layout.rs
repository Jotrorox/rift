//! Packet layouts that differ between 1.20/1.20.1 (763) and 1.20.2 (764),
//! written once for both directions.
//!
//! Sources: PrismarineJS minecraft-data (`pc/1.20` and `pc/1.20.2`
//! `protocol.json`) and the official servers' traffic in the corpus.
use super::NEWER;
use crate::{
    data,
    nbt::Nbt,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::io;

/// Identifiers, world and dimension names: vanilla's string limit.
const MAX_STRING: usize = 32767;
const MAX_NAME: usize = 16;

/// Join Game (`login`), the fields both protocols share. 1.20.1 carries the
/// registry codec inline; 1.20.2 sends it as configuration `registry_data`
/// and adds `limited_crafting`.
#[derive(Clone, Debug, PartialEq)]
pub struct Join {
    pub entity_id: i32,
    pub hardcore: bool,
    pub game_mode: u8,
    pub previous_game_mode: i8,
    pub worlds: Vec<String>,
    pub dimension_type: String,
    pub dimension: String,
    pub seed: i64,
    pub max_players: i32,
    pub view_distance: i32,
    pub simulation_distance: i32,
    pub reduced_debug_info: bool,
    pub respawn_screen: bool,
    pub limited_crafting: bool,
    pub debug: bool,
    pub flat: bool,
    /// Dimension and packed position of the last death.
    pub death: Option<(String, i64)>,
    pub portal_cooldown: i32,
}

fn worlds(r: &mut Reader<'_>) -> io::Result<Vec<String>> {
    let count = r.count()?;
    let mut worlds = Vec::with_capacity(count);
    for _ in 0..count {
        worlds.push(r.string(MAX_STRING)?.to_owned());
    }
    Ok(worlds)
}

fn death(r: &mut Reader<'_>) -> io::Result<Option<(String, i64)>> {
    Ok(if r.bool()? {
        Some((r.string(MAX_STRING)?.to_owned(), r.i64()?))
    } else {
        None
    })
}

fn write_death(w: &mut Writer, death: &Option<(String, i64)>) {
    match death {
        Some((dimension, position)) => {
            w.bool(true).string(dimension).i64(*position);
        }
        None => {
            w.bool(false);
        }
    }
}

impl Join {
    /// A 1.20.1 Join Game and its registry codec.
    pub fn read_older(body: &[u8]) -> io::Result<(Self, Nbt)> {
        let mut r = Reader::new(body);
        let entity_id = r.i32()?;
        let hardcore = r.bool()?;
        let game_mode = r.u8()?;
        let previous_game_mode = r.i8()?;
        let worlds = worlds(&mut r)?;
        let codec =
            Nbt::read(&mut r, true)?.ok_or_else(|| invalid("Join Game without registries"))?;
        let join = Self {
            entity_id,
            hardcore,
            game_mode,
            previous_game_mode,
            worlds,
            dimension_type: r.string(MAX_STRING)?.to_owned(),
            dimension: r.string(MAX_STRING)?.to_owned(),
            seed: r.i64()?,
            max_players: r.varint()?,
            view_distance: r.varint()?,
            simulation_distance: r.varint()?,
            reduced_debug_info: r.bool()?,
            respawn_screen: r.bool()?,
            limited_crafting: false,
            debug: r.bool()?,
            flat: r.bool()?,
            death: death(&mut r)?,
            portal_cooldown: r.varint()?,
        };
        r.finish()?;
        Ok((join, codec))
    }

    pub fn write_older(&self, codec: &Nbt) -> Vec<u8> {
        let mut w = Writer::new();
        w.i32(self.entity_id)
            .bool(self.hardcore)
            .u8(self.game_mode)
            .i8(self.previous_game_mode)
            .varint(self.worlds.len() as i32);
        for world in &self.worlds {
            w.string(world);
        }
        codec.write(&mut w, true);
        w.string(&self.dimension_type)
            .string(&self.dimension)
            .i64(self.seed)
            .varint(self.max_players)
            .varint(self.view_distance)
            .varint(self.simulation_distance)
            .bool(self.reduced_debug_info)
            .bool(self.respawn_screen)
            .bool(self.debug)
            .bool(self.flat);
        write_death(&mut w, &self.death);
        w.varint(self.portal_cooldown);
        w.buf
    }

    /// A 1.20.2 Join Game.
    pub fn read_newer(body: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(body);
        let entity_id = r.i32()?;
        let hardcore = r.bool()?;
        let worlds = worlds(&mut r)?;
        let max_players = r.varint()?;
        let view_distance = r.varint()?;
        let simulation_distance = r.varint()?;
        let reduced_debug_info = r.bool()?;
        let respawn_screen = r.bool()?;
        let limited_crafting = r.bool()?;
        let join = Self {
            entity_id,
            hardcore,
            worlds,
            max_players,
            view_distance,
            simulation_distance,
            reduced_debug_info,
            respawn_screen,
            limited_crafting,
            dimension_type: r.string(MAX_STRING)?.to_owned(),
            dimension: r.string(MAX_STRING)?.to_owned(),
            seed: r.i64()?,
            game_mode: r.u8()?,
            previous_game_mode: r.i8()?,
            debug: r.bool()?,
            flat: r.bool()?,
            death: death(&mut r)?,
            portal_cooldown: r.varint()?,
        };
        r.finish()?;
        Ok(join)
    }

    pub fn write_newer(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.i32(self.entity_id)
            .bool(self.hardcore)
            .varint(self.worlds.len() as i32);
        for world in &self.worlds {
            w.string(world);
        }
        w.varint(self.max_players)
            .varint(self.view_distance)
            .varint(self.simulation_distance)
            .bool(self.reduced_debug_info)
            .bool(self.respawn_screen)
            .bool(self.limited_crafting)
            .string(&self.dimension_type)
            .string(&self.dimension)
            .i64(self.seed)
            .u8(self.game_mode)
            .i8(self.previous_game_mode)
            .bool(self.debug)
            .bool(self.flat);
        write_death(&mut w, &self.death);
        w.varint(self.portal_cooldown);
        w.buf
    }

    /// The 1.20.1 Respawn an older client needs after a second Join Game to
    /// reset its world, as Rift's own pre-1.20.2 server switch does: the same
    /// dimension, nothing kept.
    pub fn respawn_older(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.string(&self.dimension_type)
            .string(&self.dimension)
            .i64(self.seed)
            .u8(self.game_mode)
            .i8(self.previous_game_mode)
            .bool(self.debug)
            .bool(self.flat)
            .u8(0);
        write_death(&mut w, &self.death);
        w.varint(self.portal_cooldown);
        w.buf
    }
}

/// Respawn: 1.20.2 moved the "data to keep" flags from before the death
/// location to the end. `to_newer` selects the direction.
pub fn respawn(body: &[u8], to_newer: bool) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(body);
    rw.string(MAX_STRING)?; // Dimension type.
    rw.string(MAX_STRING)?; // Dimension.
    rw.i64()?; // Hashed seed.
    rw.u8()?; // Game mode.
    rw.i8()?; // Previous game mode.
    rw.bool()?; // Debug.
    rw.bool()?; // Flat.
    if to_newer {
        let kept = rw.r.u8()?;
        death(&mut rw.r).map(|death| write_death(&mut rw.w, &death))?;
        rw.varint()?; // Portal cooldown.
        rw.w.u8(kept);
    } else {
        let tail = rw.r.peek();
        let mut probe = rw.r;
        death(&mut probe)?;
        probe.varint()?;
        let tail = rw.r.take(tail.len() - probe.remaining())?;
        let kept = rw.r.u8()?;
        rw.w.u8(kept).bytes(tail);
    }
    rw.finish()
}

/// Login Start: 1.20.2 made the profile UUID mandatory.
pub fn hello(body: &[u8], to_newer: bool) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(body);
    let name = rw.string(MAX_NAME)?;
    if to_newer {
        let uuid = if rw.r.bool()? {
            rw.r.uuid()?
        } else {
            offline_uuid(name)
        };
        rw.w.uuid(uuid);
    } else {
        let uuid = rw.r.uuid()?;
        rw.w.bool(true).uuid(uuid);
    }
    rw.finish()
}

/// The UUID an offline-mode server gives `name` (Java's
/// `UUID.nameUUIDFromBytes("OfflinePlayer:" + name)`), for an older client
/// that sent none.
pub fn offline_uuid(name: &str) -> u128 {
    let mut hash = md5(format!("OfflinePlayer:{name}").as_bytes());
    hash[6] = hash[6] & 0x0f | 0x30;
    hash[8] = hash[8] & 0x3f | 0x80;
    u128::from_be_bytes(hash)
}

/// RFC 1321 MD5, only for [`offline_uuid`].
fn md5(input: &[u8]) -> [u8; 16] {
    const SHIFTS: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let constants: [u32; 64] =
        std::array::from_fn(|i| ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32);
    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let mut message = input.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((input.len() as u64).wrapping_mul(8)).to_le_bytes());
    for block in message.as_chunks::<64>().0 {
        let words: [u32; 16] = std::array::from_fn(|i| {
            u32::from_le_bytes(block[i * 4..i * 4 + 4].try_into().unwrap())
        });
        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let rotated = a
                .wrapping_add(f)
                .wrapping_add(constants[i])
                .wrapping_add(words[g])
                .rotate_left(SHIFTS[i]);
            (a, d, c) = (d, c, b);
            b = b.wrapping_add(rotated);
        }
        for (word, value) in state.iter_mut().zip([a, b, c, d]) {
            *word = word.wrapping_add(value);
        }
    }
    let mut digest = [0; 16];
    for (chunk, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    digest
}

/// Copy one network NBT tag, changing its root between the named (1.20.1)
/// and nameless (1.20.2) forms. A lone TAG_End (absent tag) is the same in
/// both.
pub fn nbt_root(rw: &mut Rewriter<'_>, to_newer: bool) -> io::Result<()> {
    // The source is named exactly when it is the older form.
    let tag = rw.r.nbt_bytes(to_newer)?;
    match *tag {
        [] | [0, ..] => {
            rw.w.bytes(tag);
        }
        [kind, high, low, ref rest @ ..] if to_newer => {
            let payload = rest
                .get(usize::from(u16::from_be_bytes([high, low]))..)
                .ok_or_else(|| invalid("short NBT name"))?;
            rw.w.u8(kind).bytes(payload);
        }
        [_] => return Err(invalid("short NBT tag")),
        [kind, ref payload @ ..] => {
            rw.w.u8(kind).u16(0).bytes(payload);
        }
    }
    Ok(())
}

/// Tag Query response: transaction ID, then an optional tag whose root form
/// changed.
pub fn tag_query(body: &[u8], to_newer: bool) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(body);
    rw.varint()?;
    nbt_root(&mut rw, to_newer)?;
    rw.finish()
}

/// Display Objective: the slot became a VarInt. `None` drops a 1.20.2 slot
/// a 1.20.1 client cannot index (it has slots 0 to 18 only).
pub fn display_objective(body: &[u8], to_newer: bool) -> io::Result<Option<Vec<u8>>> {
    let mut rw = Rewriter::new(body);
    if to_newer {
        let slot = rw.r.i8()?;
        rw.w.varint(i32::from(slot));
    } else {
        let slot = rw.r.varint()?;
        if !(0..=18).contains(&slot) {
            rw.r.str()?;
            rw.r.finish()?;
            return Ok(None);
        }
        rw.w.i8(slot as i8);
    }
    rw.str()?;
    rw.finish().map(Some)
}

/// The player entity type's ID in either protocol.
pub fn player_type(protocol: i32) -> i32 {
    data::version(protocol)
        .expect_registry("entity_type")
        .id("player")
        .expect("every protocol has a player entity type")
}

/// A 1.20.1 Spawn Player as the 1.20.2 Add Entity of a player.
pub fn add_player(body: &[u8]) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(body);
    rw.varint()?; // Entity ID.
    rw.uuid()?;
    rw.w.varint(player_type(NEWER));
    for _ in 0..3 {
        rw.f64()?;
    }
    let yaw = rw.r.i8()?;
    let pitch = rw.r.i8()?;
    rw.w.i8(pitch).i8(yaw).i8(yaw).varint(0);
    rw.w.i16(0).i16(0).i16(0);
    rw.finish()
}

/// A 1.20.2 Add Entity, read whole.
#[derive(Clone, Debug, PartialEq)]
pub struct AddEntity {
    pub id: i32,
    pub uuid: u128,
    pub kind: i32,
    pub position: [f64; 3],
    pub pitch: i8,
    pub yaw: i8,
    pub head_yaw: i8,
    pub data: i32,
    pub velocity: [i16; 3],
}

impl AddEntity {
    pub fn read(body: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(body);
        let entity = Self {
            id: r.varint()?,
            uuid: r.uuid()?,
            kind: r.varint()?,
            position: [r.f64()?, r.f64()?, r.f64()?],
            pitch: r.i8()?,
            yaw: r.i8()?,
            head_yaw: r.i8()?,
            data: r.varint()?,
            velocity: [r.i16()?, r.i16()?, r.i16()?],
        };
        r.finish()?;
        Ok(entity)
    }

    pub fn write(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.varint(self.id).uuid(self.uuid).varint(self.kind);
        for value in self.position {
            w.f64(value);
        }
        w.i8(self.pitch)
            .i8(self.yaw)
            .i8(self.head_yaw)
            .varint(self.data);
        for value in self.velocity {
            w.i16(value);
        }
        w.buf
    }

    /// The 1.20.1 Spawn Player for a player entity.
    pub fn spawn_player(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.varint(self.id).uuid(self.uuid);
        for value in self.position {
            w.f64(value);
        }
        w.i8(self.yaw).i8(self.pitch);
        w.buf
    }
}

/// The registry that a mob effect sits in changed numbering in 1.20.2 (the
/// empty ID 0 went away); Set Beacon carries two optional effect IDs.
pub fn set_beacon(body: &[u8], map: impl Fn(i32) -> Option<i32>) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(body);
    for _ in 0..2 {
        if rw.r.bool()? {
            match map(rw.r.varint()?) {
                Some(id) => {
                    rw.w.bool(true).varint(id);
                }
                None => {
                    rw.w.bool(false);
                }
            }
        } else {
            rw.w.bool(false);
        }
    }
    rw.finish()
}

/// A Custom Payload on its way to a 1.20.2+ client, which parses
/// `minecraft:brand` strictly: anything after the brand string would
/// disconnect it. 1.20.1 clients ignored such trailing bytes.
pub fn custom_payload_to_newer(body: &[u8]) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(body);
    if rw.string(MAX_STRING)? == "minecraft:brand" {
        rw.string(MAX_STRING)?;
        rw.r.rest();
    } else {
        rw.copy_rest();
    }
    rw.finish()
}

/// Synchronised registry content: 1.20.2 trim patterns carry a `decal`
/// flag (false unless set), which 1.20.1 does not know.
///
// INTEGRATION: `codec::registries` is to convert registry content between
// every pair of protocols. Until it exposes that, this covers the only
// difference between 763 and 764, found by comparing the official servers'
// registries in the corpus entry by entry.
pub fn convert_registries(codec: &mut Nbt, to_newer: bool) {
    let Some(entries) = codec
        .as_compound_mut()
        .and_then(|codec| codec.compound_mut("minecraft:trim_pattern"))
        .and_then(|registry| registry.get_mut("value"))
        .and_then(Nbt::as_list_mut)
    else {
        return;
    };
    for element in entries
        .iter_mut()
        .filter_map(Nbt::as_compound_mut)
        .filter_map(|entry| entry.compound_mut("element"))
    {
        if to_newer {
            if !element.contains("decal") {
                element.insert("decal", Nbt::Byte(0));
            }
        } else {
            element.remove("decal");
        }
    }
}
