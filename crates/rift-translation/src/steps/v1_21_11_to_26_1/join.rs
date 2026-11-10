//! Layouts the three 26.x steps share, written as functions of the
//! protocol: join game (`login`), `respawn` and the spawn info inside both,
//! game mode changes, and the low-precision vectors of 1.21.9 and later.
//!
//! Field orders come from the official servers' `CommonPlayerSpawnInfo`,
//! `ClientboundLoginPacket` and `ClientboundRespawnPacket` of 1.21.11 to
//! 26.3: 26.2 added the online-mode flag to join game and 26.3 made both
//! game modes VarInts (the previous one an optional VarInt).
use crate::{
    Direction,
    codec::Shared,
    wire::{Reader, Writer, invalid},
};
use std::io;

/// Identifiers (dimension and world names) are at most this long.
const MAX_IDENTIFIER: usize = 32767;

/// Game modes are VarInts and the previous one optional from 26.3 on.
pub(in crate::steps) fn varint_game_modes(protocol: i32) -> bool {
    protocol >= 777
}

/// Join game has the server's online-mode flag from 26.2 on.
pub(in crate::steps) fn online_mode_flag(protocol: i32) -> bool {
    protocol >= 776
}

/// The highest game mode ID (spectator).
pub(in crate::steps) const SPECTATOR: i32 = 3;

/// `CommonPlayerSpawnInfo`.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::steps) struct SpawnInfo {
    /// Network ID in the synchronised `dimension_type` registry.
    pub dimension_type: i32,
    pub dimension: String,
    pub seed: i64,
    pub game_mode: i32,
    pub previous_game_mode: Option<i32>,
    pub debug: bool,
    pub flat: bool,
    pub last_death: Option<(String, i64)>,
    pub portal_cooldown: i32,
    pub sea_level: i32,
}

impl SpawnInfo {
    pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        let dimension_type = r.varint()?;
        let dimension = r.string(MAX_IDENTIFIER)?.to_owned();
        let seed = r.i64()?;
        let (game_mode, previous_game_mode) = if varint_game_modes(protocol) {
            let game_mode = r.varint()?;
            let previous = match r.varint()? {
                0 => None,
                id => Some(id - 1),
            };
            (game_mode, previous)
        } else {
            let game_mode = i32::from(r.u8()?);
            let previous = match r.i8()? {
                -1 => None,
                id => Some(i32::from(id)),
            };
            (game_mode, previous)
        };
        let debug = r.bool()?;
        let flat = r.bool()?;
        let last_death = if r.bool()? {
            Some((r.string(MAX_IDENTIFIER)?.to_owned(), r.i64()?))
        } else {
            None
        };
        Ok(Self {
            dimension_type,
            dimension,
            seed,
            game_mode,
            previous_game_mode,
            debug,
            flat,
            last_death,
            portal_cooldown: r.varint()?,
            sea_level: r.varint()?,
        })
    }

    pub fn write(&self, protocol: i32, w: &mut Writer) {
        w.varint(self.dimension_type)
            .string(&self.dimension)
            .i64(self.seed);
        if varint_game_modes(protocol) {
            w.varint(self.game_mode)
                .varint(self.previous_game_mode.map_or(0, |id| id + 1));
        } else {
            // A byte game mode cannot carry what the older client does not
            // know; vanilla reads unknown IDs as survival anyway.
            let byte = |id: i32| if (0..=SPECTATOR).contains(&id) { id } else { 0 };
            w.u8(byte(self.game_mode) as u8)
                .i8(self.previous_game_mode.map_or(-1, byte) as i8);
        }
        w.bool(self.debug).bool(self.flat);
        match &self.last_death {
            Some((dimension, position)) => {
                w.bool(true).string(dimension).i64(*position);
            }
            None => {
                w.bool(false);
            }
        }
        w.varint(self.portal_cooldown).varint(self.sea_level);
    }

    /// Convert the parts that refer to synchronised registries.
    pub fn convert(&mut self, shared: &mut Shared, direction: Direction) {
        self.dimension_type = dimension_type(shared, direction, self.dimension_type);
    }
}

/// The receiver's network ID of a dimension type.
// INTEGRATION: `codec::registries::map(shared, direction, "dimension_type",
// id)` (branch t3/registries-codec-sync), falling back to `id`. Until it is
// merged the IDs are unchanged, which holds while the codec neither adds
// nor drops dimension types between these 26.x protocols.
fn dimension_type(shared: &mut Shared, direction: Direction, id: i32) -> i32 {
    let _ = (shared, direction);
    id
}

/// Join game (`ClientboundLoginPacket`).
#[derive(Clone, Debug, PartialEq)]
pub(in crate::steps) struct Login {
    pub entity_id: i32,
    pub hardcore: bool,
    pub levels: Vec<String>,
    pub max_players: i32,
    pub chunk_radius: i32,
    pub simulation_distance: i32,
    pub reduced_debug_info: bool,
    pub show_death_screen: bool,
    pub limited_crafting: bool,
    pub spawn: SpawnInfo,
    /// Present from 26.2 on.
    pub online_mode: Option<bool>,
    pub enforces_secure_chat: bool,
}

impl Login {
    /// The whole packet; trailing bytes are an error.
    pub fn parse(protocol: i32, data: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(data);
        let entity_id = r.i32()?;
        let hardcore = r.bool()?;
        let count = r.count()?;
        let mut levels = Vec::with_capacity(count.min(r.remaining()));
        for _ in 0..count {
            levels.push(r.string(MAX_IDENTIFIER)?.to_owned());
        }
        let login = Self {
            entity_id,
            hardcore,
            levels,
            max_players: r.varint()?,
            chunk_radius: r.varint()?,
            simulation_distance: r.varint()?,
            reduced_debug_info: r.bool()?,
            show_death_screen: r.bool()?,
            limited_crafting: r.bool()?,
            spawn: SpawnInfo::read(protocol, &mut r)?,
            online_mode: if online_mode_flag(protocol) {
                Some(r.bool()?)
            } else {
                None
            },
            enforces_secure_chat: r.bool()?,
        };
        r.finish()?;
        Ok(login)
    }

    /// The packet in `protocol`'s layout. A missing online-mode flag is
    /// written as `true`: clients before 26.2 always prepared a chat
    /// signing key, which 26.2 only does on online-mode servers.
    pub fn to_bytes(&self, protocol: i32) -> Vec<u8> {
        let mut w = Writer::new();
        w.i32(self.entity_id).bool(self.hardcore);
        w.varint(self.levels.len() as i32);
        for level in &self.levels {
            w.string(level);
        }
        w.varint(self.max_players)
            .varint(self.chunk_radius)
            .varint(self.simulation_distance)
            .bool(self.reduced_debug_info)
            .bool(self.show_death_screen)
            .bool(self.limited_crafting);
        self.spawn.write(protocol, &mut w);
        if online_mode_flag(protocol) {
            w.bool(self.online_mode.unwrap_or(true));
        }
        w.bool(self.enforces_secure_chat);
        w.into_inner()
    }
}

/// `ClientboundRespawnPacket`.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::steps) struct Respawn {
    pub spawn: SpawnInfo,
    pub data_to_keep: u8,
}

impl Respawn {
    pub fn parse(protocol: i32, data: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(data);
        let respawn = Self {
            spawn: SpawnInfo::read(protocol, &mut r)?,
            data_to_keep: r.u8()?,
        };
        r.finish()?;
        Ok(respawn)
    }

    pub fn to_bytes(&self, protocol: i32) -> Vec<u8> {
        let mut w = Writer::new();
        self.spawn.write(protocol, &mut w);
        w.u8(self.data_to_keep);
        w.into_inner()
    }
}

/// The game mode a `game_event` sets, if it is a game mode change (event 3,
/// the mode as a float). Unchanged in every 26.x protocol.
pub(in crate::steps) fn game_event_mode(data: &[u8]) -> Option<i32> {
    let [3, value @ ..] = data else {
        return None;
    };
    let value = f32::from_be_bytes(value.try_into().ok()?);
    Some((value + 0.5).floor() as i32)
}

/// A vector in `LpVec3` form (1.21.9+): a zero byte, or three 15-bit
/// components packed with a 2-bit scale, a continuation flag and a VarInt
/// for the rest of the scale.
pub(in crate::steps) fn read_lp_vec3(r: &mut Reader<'_>) -> io::Result<[f64; 3]> {
    let lowest = r.u8()?;
    if lowest == 0 {
        return Ok([0.0; 3]);
    }
    let middle = r.u8()?;
    let highest = u64::from(r.i32()? as u32);
    let buffer = highest << 16 | u64::from(middle) << 8 | u64::from(lowest);
    let mut scale = u64::from(lowest & 3);
    if lowest & 4 != 0 {
        scale |= u64::from(r.varint()? as u32) << 2;
    }
    let unpack = |value: u64| ((value & 32767) as f64).min(32766.0) * 2.0 / 32766.0 - 1.0;
    let scale = scale as f64;
    Ok([
        unpack(buffer >> 3) * scale,
        unpack(buffer >> 18) * scale,
        unpack(buffer >> 33) * scale,
    ])
}

pub(in crate::steps) fn write_lp_vec3(w: &mut Writer, value: [f64; 3]) {
    const LIMIT: f64 = 1.7179869183E10;
    let sanitize = |value: f64| {
        if value.is_nan() {
            0.0
        } else {
            value.clamp(-LIMIT, LIMIT)
        }
    };
    let [x, y, z] = value.map(sanitize);
    let length = x.abs().max(y.abs()).max(z.abs());
    if length < 3.051944088384301E-5 {
        w.u8(0);
        return;
    }
    let scale = length.ceil() as i64;
    let partial = scale & 3 != scale;
    let markers = if partial { scale & 3 | 4 } else { scale };
    // Java's Math.round: floor(x + 0.5).
    let pack = |value: f64| ((value * 0.5 + 0.5) * 32766.0 + 0.5).floor() as i64;
    let scale_f = scale as f64;
    let buffer =
        markers | pack(x / scale_f) << 3 | pack(y / scale_f) << 18 | pack(z / scale_f) << 33;
    w.u8(buffer as u8)
        .u8((buffer >> 8) as u8)
        .i32((buffer >> 16) as i32);
    if partial {
        w.varint((scale >> 2) as i32);
    }
}

/// Check a hand (`InteractionHand`) value: main hand 0, off hand 1.
pub(in crate::steps) fn hand(value: i32) -> io::Result<i32> {
    match value {
        0 | 1 => Ok(value),
        _ => Err(invalid("invalid hand")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn lp_vectors_round_trip_like_the_game_encodes_them() {
        for value in [
            [0.0, 0.0, 0.0],
            [0.5, -0.25, 0.125],
            [3.0, -2.0, 1.0],
            [-17.5, 4.0, 900.0],
            [1e9, -1e9, 0.0],
        ] {
            let mut w = Writer::new();
            write_lp_vec3(&mut w, value);
            let mut r = Reader::new(&w.buf);
            let decoded = read_lp_vec3(&mut r).unwrap();
            r.finish().unwrap();
            let mut again = Writer::new();
            write_lp_vec3(&mut again, decoded);
            assert_eq!(again.buf, w.buf, "{value:?}");
            let scale = value.iter().fold(1.0f64, |m, v| m.max(v.abs().ceil()));
            for (a, b) in value.iter().zip(decoded) {
                assert!((a - b).abs() <= scale / 16000.0, "{value:?} -> {decoded:?}");
            }
        }
        assert!(read_lp_vec3(&mut Reader::new(&[5, 0])).is_err());
    }

    #[test]
    fn spawn_info_converts_game_modes_between_byte_and_varint() {
        // 26.2 respawn: dimension type 0, overworld, survival, previous
        // none (-1), not debug, not flat, no death location.
        let mut w = Writer::new();
        w.varint(0).string("minecraft:overworld").i64(42);
        w.u8(3).i8(-1).bool(false).bool(true).bool(false);
        w.varint(0).varint(63).u8(1);
        let old = Respawn::parse(776, &w.buf).unwrap();
        assert_eq!(old.spawn.game_mode, 3);
        assert_eq!(old.spawn.previous_game_mode, None);
        assert_eq!(old.to_bytes(776), w.buf);
        let new = old.to_bytes(777);
        let parsed = Respawn::parse(777, &new).unwrap();
        assert_eq!(parsed, old);
        assert_eq!(parsed.to_bytes(776), w.buf);
        // An unknown game mode cannot reach a byte-mode client.
        let mut odd = parsed.clone();
        odd.spawn.game_mode = 9;
        odd.spawn.previous_game_mode = Some(200);
        let bytes = odd.to_bytes(776);
        let back = Respawn::parse(776, &bytes).unwrap();
        assert_eq!(
            (back.spawn.game_mode, back.spawn.previous_game_mode),
            (0, Some(0))
        );
        assert!(Respawn::parse(776, &w.buf[..w.buf.len() - 1]).is_err());
        let mut long = w.buf.clone();
        long.push(0);
        assert!(Respawn::parse(776, &long).is_err());
    }

    #[test]
    fn login_from_the_corpus_round_trips() {
        // 26.3's own join game, recorded from the official server.
        let body = hex(concat!(
            "000000010003136d696e6563726166743a6f766572776f726c64116d696e6563",
            "726166743a7468655f656e64146d696e6563726166743a7468655f6e65746865",
            "7214020200010000136d696e6563726166743a6f766572776f726c64ba3fe886",
            "e9522625010000010000c1ffffff0f0000"
        ));
        // 26.2's, with a byte game mode and -1 for no previous one.
        let older_body = hex(concat!(
            "000000010003136d696e6563726166743a6f766572776f726c64116d696e6563",
            "726166743a7468655f656e64146d696e6563726166743a7468655f6e65746865",
            "7214020200010000136d696e6563726166743a6f766572776f726c644ae77737",
            "d6c2002f01ff00010000c1ffffff0f0000"
        ));
        let older_login = Login::parse(776, &older_body).unwrap();
        assert_eq!(older_login.to_bytes(776), older_body);
        assert_eq!(
            (
                older_login.spawn.game_mode,
                older_login.spawn.previous_game_mode
            ),
            (1, None)
        );
        let login = Login::parse(777, &body).unwrap();
        assert_eq!(login.levels.len(), 3);
        assert_eq!(login.online_mode, Some(false));
        assert_eq!(login.to_bytes(777), body);
        let older = login.to_bytes(776);
        assert_eq!(Login::parse(776, &older).unwrap(), login);
        let oldest = login.to_bytes(775);
        let mut back = Login::parse(775, &oldest).unwrap();
        assert_eq!(back.online_mode, None);
        back.online_mode = Some(false);
        assert_eq!(back, login);
    }
}
