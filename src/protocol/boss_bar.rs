//! Boss bars that Rift shows on behalf of extensions. Each bar has a UUID of
//! its own, so the backend's boss bars are never touched.
//!
//! Packet IDs: the legacy table through 1.20.1; PrismarineJS minecraft-data
//! through 26.1; 26.2 and 26.3 from the official server artifacts' packet
//! reports. The Boss Event layout is unchanged from 1.9 to 26.3 apart from its
//! text, which is network NBT from 1.20.3.
use super::packets::Text;
use super::{Packet, ProtocolVersion, write_varint};
use serde_json::Value as Json;
use std::{collections::BTreeMap, io};

/// Bars per player. The client stacks them down from the top of the screen.
pub const MAX_BOSS_BARS: usize = 8;

const ADD: i32 = 0;
const REMOVE: i32 = 1;
const PROGRESS: i32 = 2;
const NAME: i32 = 3;
const STYLE: i32 = 4;
const FLAGS: i32 = 5;

/// Vanilla `BossEvent.BossBarColor` order.
pub const COLORS: [&str; 7] = ["pink", "blue", "red", "green", "yellow", "purple", "white"];
/// Vanilla `BossEvent.BossBarOverlay` order.
pub const STYLES: [&str; 5] = [
    "progress",
    "notched_6",
    "notched_10",
    "notched_12",
    "notched_20",
];
/// Flag bits in `ClientboundBossEventPacket.encodeProperties` order.
pub const FLAG_NAMES: [&str; 3] = ["darken_screen", "play_music", "create_fog"];

/// What one bar shows.
#[derive(Debug, Clone, PartialEq)]
pub struct BossBar {
    /// A plain string or a chat component.
    pub text: Json,
    /// From 0 to 1.
    pub progress: f32,
    /// An index into [`COLORS`].
    pub color: u8,
    /// An index into [`STYLES`].
    pub style: u8,
    /// Bits for [`FLAG_NAMES`].
    pub flags: u8,
}

// Progress is always a finite number from 0 to 1.
impl Eq for BossBar {}

/// The Boss Event packet ID, from 1.9 onward.
fn id(version: ProtocolVersion) -> Option<i32> {
    version.switching()?;
    if let Some(ids) = super::legacy::ids(version.number()) {
        return (ids.boss >= 0).then_some(ids.boss);
    }
    match version.number() {
        764..=769 => Some(0x0a),
        770..=777 => Some(0x09),
        _ => None,
    }
}

/// Whether a player on `protocol` can be shown boss bars.
pub fn supported(protocol: i32) -> bool {
    ProtocolVersion::new(protocol).ok().and_then(id).is_some()
}

/// A stable UUID for an extension's bar name. The fixed first half keeps it
/// apart from entity and player UUIDs, which backends use for their own bars.
fn uuid(name: &str) -> [u8; 16] {
    // FNV-1a: stable across processes and Rust releases.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in name.bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut uuid = [0; 16];
    uuid[..8].copy_from_slice(b"rift\x00bb\x00");
    uuid[8..].copy_from_slice(&hash.to_be_bytes());
    // Version 8 (custom), RFC 4122 variant.
    uuid[6] = 0x80 | (uuid[6] & 0x0f);
    uuid[8] = 0x80 | (uuid[8] & 0x3f);
    uuid
}

/// The bars one client is showing, as far as Rift has sent them.
#[derive(Debug, Default)]
pub(crate) struct BossBars {
    shown: BTreeMap<String, BossBar>,
    /// Counts client resets, so the owner knows to sync again.
    revision: u64,
}

impl BossBars {
    /// The client discarded every boss bar with its play listener.
    pub(crate) fn reset(&mut self) {
        self.shown.clear();
        self.revision += 1;
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    /// Packets that bring the client from the bars it shows to `wanted`.
    /// Versions without boss bars get none.
    pub(crate) fn sync(
        &mut self,
        version: ProtocolVersion,
        wanted: &BTreeMap<String, BossBar>,
    ) -> io::Result<Vec<Packet>> {
        let Some(id) = id(version) else {
            return Ok(Vec::new());
        };
        let nbt = version.nbt_components();
        let mut packets = Vec::new();
        let packet = |name: &str, action: i32, body: &dyn Fn(&mut Vec<u8>) -> io::Result<()>| {
            let mut data = uuid(name).to_vec();
            write_varint(action, &mut data);
            body(&mut data)?;
            Ok::<_, io::Error>(Packet::new(id, data))
        };
        for name in self.shown.keys() {
            if !wanted.contains_key(name) {
                packets.push(packet(name, REMOVE, &|_| Ok(()))?);
            }
        }
        for (name, bar) in wanted {
            match self.shown.get(name) {
                None => packets.push(packet(name, ADD, &|data| {
                    Text::Component(&bar.text).write(nbt, data)?;
                    data.extend(bar.progress.to_be_bytes());
                    write_varint(bar.color.into(), data);
                    write_varint(bar.style.into(), data);
                    data.push(bar.flags);
                    Ok(())
                })?),
                Some(shown) => {
                    if shown.text != bar.text {
                        packets.push(packet(name, NAME, &|data| {
                            Text::Component(&bar.text).write(nbt, data)
                        })?);
                    }
                    if shown.progress != bar.progress {
                        packets.push(packet(name, PROGRESS, &|data| {
                            data.extend(bar.progress.to_be_bytes());
                            Ok(())
                        })?);
                    }
                    if (shown.color, shown.style) != (bar.color, bar.style) {
                        packets.push(packet(name, STYLE, &|data| {
                            write_varint(bar.color.into(), data);
                            write_varint(bar.style.into(), data);
                            Ok(())
                        })?);
                    }
                    if shown.flags != bar.flags {
                        packets.push(packet(name, FLAGS, &|data| {
                            data.push(bar.flags);
                            Ok(())
                        })?);
                    }
                }
            }
        }
        self.shown.clone_from(wanted);
        Ok(packets)
    }
}

#[cfg(test)]
#[path = "boss_bar_tests.rs"]
mod tests;
