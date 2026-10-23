//! Pre-1.20.2 switching: Join Game updates the entity/registry, Respawn resets the
//! world. Tab entries, boss bars, header/footer and titles survive that reset.
//! Wire schemas: PrismarineJS minecraft-data; transition behavior cross-checked
//! against Velocity's ClientPlaySessionHandler.doFastClientServerSwitch.
use super::packets::{boolean, take};
use super::{
    Packet, ProtocolVersion, invalid, read_string, read_varint, write_string, write_varint,
};
use std::{collections::BTreeSet, io};

#[derive(Clone, Copy)]
pub(super) struct LegacyIds {
    pub respawn: i32,
    pub keepalive: i32,
    pub keepalive_reply: i32,
    pub boss: i32,
    pub players: i32,
    pub remove_players: i32,
    pub header: i32,
    title: i32,
}

// One reviewed row per released protocol. -1 means the packet does not exist.
include!("legacy_ids.rs");

pub(super) fn world(version: i32, packet: &Packet) -> io::Result<(Packet, Packet)> {
    let ids = ids(version).ok_or_else(|| invalid("missing legacy switching IDs"))?;
    let mut bytes = packet.data.as_slice();
    take(&mut bytes, 4)?;
    let mut join = packet.clone();
    let mut respawn = Vec::new();
    if version < 735 {
        let mode = take(&mut bytes, 1)?[0] & 7;
        let dim_offset = packet.data.len() - bytes.len();
        let dim = if version < 108 {
            i32::from(take(&mut bytes, 1)?[0] as i8)
        } else {
            i32::from_be_bytes(take(&mut bytes, 4)?.try_into().unwrap())
        };
        respawn.extend(dim.to_be_bytes());
        let alternate: i32 = if dim == 0 { -1 } else { 0 };
        if version < 108 {
            join.data[dim_offset] = alternate as u8;
        } else {
            join.data[dim_offset..dim_offset + 4].copy_from_slice(&alternate.to_be_bytes());
        }
        if version < 477 {
            respawn.extend(take(&mut bytes, 1)?);
        }
        if version >= 573 {
            respawn.extend(take(&mut bytes, 8)?);
        }
        take(&mut bytes, 1)?; // Player limit.
        respawn.push(mode);
        write_string(read_string(&mut bytes, 16)?, &mut respawn);
        if version >= 477 {
            read_varint(&mut bytes)?;
        }
        boolean(&mut bytes)?;
        if version >= 573 {
            boolean(&mut bytes)?;
        }
    } else {
        if version >= 751 {
            boolean(&mut bytes)?;
        }
        let modes = take(&mut bytes, 2)?;
        let worlds = read_varint(&mut bytes)?;
        if worlds < 0 || worlds as usize > bytes.len() {
            return Err(invalid("invalid world count"));
        }
        for _ in 0..worlds {
            read_string(&mut bytes, 32767)?;
        }
        super::nbt::named(&mut bytes)?;
        let dimension = bytes;
        if (751..=758).contains(&version) {
            super::nbt::named(&mut bytes)?;
        } else {
            read_string(&mut bytes, 32767)?;
        }
        read_string(&mut bytes, 32767)?;
        take(&mut bytes, 8)?;
        respawn.extend(&dimension[..dimension.len() - bytes.len()]);
        respawn.extend(modes);
        if version >= 751 {
            read_varint(&mut bytes)?;
        } else {
            take(&mut bytes, 1)?;
        }
        read_varint(&mut bytes)?;
        if version >= 757 {
            read_varint(&mut bytes)?;
        }
        boolean(&mut bytes)?;
        boolean(&mut bytes)?;
        respawn.push(u8::from(boolean(&mut bytes)?));
        respawn.push(u8::from(boolean(&mut bytes)?));
        respawn.push(0); // Reset attributes/metadata rather than carrying the old world.
        if version >= 759 {
            let death = bytes;
            if boolean(&mut bytes)? {
                read_string(&mut bytes, 32767)?;
                take(&mut bytes, 8)?;
            }
            respawn.extend(&death[..death.len() - bytes.len()]);
        }
        if version >= 763 {
            write_varint(read_varint(&mut bytes)?, &mut respawn);
        }
    }
    if !bytes.is_empty() {
        return Err(invalid("trailing legacy Join Game data"));
    }
    Ok((join, Packet::new(ids.respawn, respawn)))
}

#[derive(Default)]
pub(crate) struct LegacyState {
    players: BTreeSet<[u8; 16]>,
    bosses: BTreeSet<[u8; 16]>,
}
impl LegacyState {
    pub(crate) fn observe(&mut self, version: ProtocolVersion, packet: &Packet) -> io::Result<()> {
        let number = version.number();
        let Some(ids) = ids(number) else {
            return Ok(());
        };
        let mut bytes = packet.data.as_slice();
        if packet.id == ids.boss {
            let uuid = take(&mut bytes, 16)?.try_into().unwrap();
            match read_varint(&mut bytes)? {
                0 => {
                    self.bosses.insert(uuid);
                }
                1 => {
                    self.bosses.remove(&uuid);
                }
                _ => {}
            }
        } else {
            let tab = super::tab_list::TabIds {
                players: ids.players,
                remove_players: ids.remove_players,
                header: ids.header,
            };
            let players = &mut self.players;
            super::tab_list::changes(version, tab, packet, |uuid, added| {
                if added {
                    players.insert(uuid);
                } else {
                    players.remove(&uuid);
                }
            })?;
        }
        if self.players.len() > 65536 || self.bosses.len() > 4096 {
            return Err(invalid("legacy state tracking limit exceeded"));
        }
        Ok(())
    }

    pub(crate) fn reset(&mut self, version: ProtocolVersion) -> io::Result<Vec<Packet>> {
        let number = version.number();
        let ids = ids(number).ok_or_else(|| invalid("missing legacy switching IDs"))?;
        let mut packets = Vec::new();
        if !self.players.is_empty() {
            let mut data = Vec::new();
            if number < 761 {
                write_varint(4, &mut data);
            }
            write_varint(self.players.len() as i32, &mut data);
            for uuid in &self.players {
                data.extend(uuid);
            }
            packets.push(Packet::new(
                if number < 761 {
                    ids.players
                } else {
                    ids.remove_players
                },
                data,
            ));
        }
        for uuid in &self.bosses {
            let mut data = uuid.to_vec();
            data.push(1);
            packets.push(Packet::new(ids.boss, data));
        }
        let mut header = Vec::new();
        write_string("{\"text\":\"\"}", &mut header);
        write_string("{\"text\":\"\"}", &mut header);
        packets.push(Packet::new(ids.header, header));
        packets.push(Packet::new(
            ids.title,
            vec![if number >= 755 {
                1
            } else if number >= 315 {
                5
            } else {
                4
            }],
        ));
        self.players.clear();
        self.bosses.clear();
        Ok(packets)
    }
}

#[cfg(test)]
#[path = "legacy_tests.rs"]
mod tests;
