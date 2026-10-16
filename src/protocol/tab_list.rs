//! Tab list header/footer and the entries Rift lists for players on other
//! servers. Rift records which entries the current backend added, so it only
//! adds players elsewhere on the network and never removes a backend's entry.
//!
//! Packet IDs: PrismarineJS minecraft-data through 26.1; 26.2 and 26.3 from
//! the official server artifacts' generated packet reports. Player Info Update
//! actions follow the vanilla enum order, so list order (bit 6) precedes the
//! hat flag (bit 7) on the wire.
use super::packets::{Text, boolean, take};
use super::{
    Packet, ProtocolVersion, invalid, read_string, read_varint, write_string, write_varint,
};
use crate::auth::ProfileProperty;
use serde_json::Value as Json;
use std::{collections::BTreeSet, io, sync::Arc};

/// Entries per Rift-built player list packet, keeping skins well inside the
/// frame limit.
const BATCH: usize = 64;
/// Entries tracked per session, shared by backend and Rift entries.
const MAX_ENTRIES: usize = 65536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TabIds {
    pub players: i32,
    /// Player Info Remove from 1.19.3; -1 before, where action 4 removes.
    pub remove_players: i32,
    pub header: i32,
}

/// Tab list packets for each switchable version.
pub(crate) fn ids(version: ProtocolVersion) -> Option<TabIds> {
    version.switching()?;
    if let Some(ids) = super::legacy::ids(version.number()) {
        return Some(TabIds {
            players: ids.players,
            remove_players: ids.remove_players,
            header: ids.header,
        });
    }
    let (players, remove_players, header) = match version.number() {
        764 => (0x3c, 0x3b, 0x68),
        765 => (0x3c, 0x3b, 0x6a),
        766 | 767 => (0x3e, 0x3d, 0x6d),
        768 | 769 => (0x40, 0x3f, 0x74),
        770..=772 => (0x3f, 0x3e, 0x73),
        773 | 774 => (0x44, 0x43, 0x78),
        775 | 776 => (0x46, 0x45, 0x7a),
        777 => (0x47, 0x46, 0x7d),
        _ => return None,
    };
    Some(TabIds {
        players,
        remove_players,
        header,
    })
}

fn count(bytes: &mut &[u8]) -> io::Result<usize> {
    let n = read_varint(bytes)?;
    if n < 0 || n as usize > bytes.len() {
        return Err(invalid("invalid player list length"));
    }
    Ok(n as usize)
}
fn blob(bytes: &mut &[u8]) -> io::Result<()> {
    let n = count(bytes)?;
    take(bytes, n)?;
    Ok(())
}
fn profile(bytes: &mut &[u8]) -> io::Result<()> {
    read_string(bytes, 16)?;
    for _ in 0..count(bytes)? {
        read_string(bytes, 32767)?;
        read_string(bytes, 32767)?;
        if boolean(bytes)? {
            read_string(bytes, 32767)?;
        }
    }
    Ok(())
}
fn display(bytes: &mut &[u8], nbt: bool) -> io::Result<()> {
    if boolean(bytes)? {
        if nbt {
            super::nbt::anonymous(bytes)?;
        } else {
            read_string(bytes, 262144)?;
        }
    }
    Ok(())
}
fn key(bytes: &mut &[u8], session: bool) -> io::Result<()> {
    if boolean(bytes)? {
        if session {
            take(bytes, 16)?;
        }
        take(bytes, 8)?;
        blob(bytes)?;
        blob(bytes)?;
    }
    Ok(())
}

/// Report each entry a backend player list packet adds (`true`) or removes
/// (`false`). Returns whether the packet was a player list packet at all.
pub(super) fn changes(
    version: ProtocolVersion,
    ids: TabIds,
    packet: &Packet,
    mut change: impl FnMut([u8; 16], bool),
) -> io::Result<bool> {
    let number = version.number();
    let mut bytes = packet.data.as_slice();
    if packet.id == ids.remove_players {
        for _ in 0..count(&mut bytes)? {
            change(take(&mut bytes, 16)?.try_into().unwrap(), false);
        }
    } else if packet.id == ids.players {
        let action = if number >= 761 {
            i32::from(take(&mut bytes, 1)?[0])
        } else {
            read_varint(&mut bytes)?
        };
        let known = match number {
            ..761 => 0,
            761..=767 => 63,
            768 => 127,
            _ => 255,
        };
        if (number >= 761 && action & !known != 0) || (number < 761 && !(0..=4).contains(&action)) {
            return Err(invalid("unknown player list action"));
        }
        for _ in 0..count(&mut bytes)? {
            let uuid: [u8; 16] = take(&mut bytes, 16)?.try_into().unwrap();
            if number >= 761 {
                if action & 1 != 0 {
                    profile(&mut bytes)?;
                    change(uuid, true);
                }
                if action & 2 != 0 {
                    key(&mut bytes, true)?;
                }
                if action & 4 != 0 {
                    read_varint(&mut bytes)?;
                }
                if action & 8 != 0 {
                    boolean(&mut bytes)?;
                }
                if action & 16 != 0 {
                    read_varint(&mut bytes)?;
                }
                if action & 32 != 0 {
                    display(&mut bytes, version.nbt_components())?;
                }
                if action & 64 != 0 {
                    read_varint(&mut bytes)?;
                }
                if action & 128 != 0 {
                    boolean(&mut bytes)?;
                }
            } else {
                match action {
                    0 => {
                        profile(&mut bytes)?;
                        read_varint(&mut bytes)?;
                        read_varint(&mut bytes)?;
                        display(&mut bytes, false)?;
                        if number >= 759 {
                            key(&mut bytes, false)?;
                        }
                        change(uuid, true);
                    }
                    1 | 2 => {
                        read_varint(&mut bytes)?;
                    }
                    3 => display(&mut bytes, false)?,
                    4 => change(uuid, false),
                    _ => unreachable!(),
                }
            }
        }
    } else {
        return Ok(false);
    }
    if !bytes.is_empty() {
        return Err(invalid("trailing player list data"));
    }
    Ok(true)
}

/// A player on another server, as Rift lists them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabEntry {
    pub uuid: [u8; 16],
    pub name: String,
    pub properties: Arc<[ProfileProperty]>,
}

/// What Rift wants the client's tab list to show.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabView {
    /// Header and footer text components. `None` leaves both to the backend.
    pub header: Option<(Json, Json)>,
    /// Players to list in addition to the backend's own entries. `None` lists
    /// only the backend's players.
    pub players: Option<Vec<TabEntry>>,
}

/// One client's tab list, as far as Rift has changed or observed it.
#[derive(Debug, Default)]
pub(crate) struct TabList {
    /// Entries the current backend added.
    backend: BTreeSet<[u8; 16]>,
    /// Entries Rift added for players on other servers.
    remote: BTreeSet<[u8; 16]>,
    /// Header and footer last sent by Rift since the client last reset them.
    header: Option<(Json, Json)>,
    /// Rift has set a header and footer, so the backend's are dropped.
    owns_header: bool,
    /// Counts changes Rift did not make itself.
    revision: u64,
    /// IDs for the session's version, looked up once.
    ids: Option<Option<TabIds>>,
}

impl TabList {
    /// Track a clientbound play packet. Returns false for a backend header and
    /// footer that would replace Rift's.
    pub(crate) fn observe(
        &mut self,
        version: ProtocolVersion,
        packet: &Packet,
    ) -> io::Result<bool> {
        let Some(ids) = *self.ids.get_or_insert_with(|| ids(version)) else {
            return Ok(true);
        };
        if packet.id == ids.header {
            return Ok(!self.owns_header);
        }
        let (backend, remote) = (&mut self.backend, &mut self.remote);
        let mut changed = false;
        changes(version, ids, packet, |uuid, added| {
            // Either way the client no longer has a Rift entry for this player:
            // an add replaces it and a removal deletes it.
            changed |= remote.remove(&uuid);
            changed |= if added {
                backend.insert(uuid)
            } else {
                backend.remove(&uuid)
            };
        })?;
        if changed {
            self.revision += 1;
        }
        if self.backend.len() > MAX_ENTRIES {
            return Err(invalid("tab list tracking limit exceeded"));
        }
        Ok(true)
    }

    /// The client discarded its tab list and header with its play listener
    /// (reconfiguration), or Rift cleared them for a legacy switch.
    pub(crate) fn reset(&mut self) {
        self.backend.clear();
        self.remote.clear();
        self.header = None;
        self.revision += 1;
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn modified(&self) -> bool {
        self.owns_header || !self.remote.is_empty()
    }

    /// Remove every Rift entry, for a legacy switch that keeps the client's list.
    pub(crate) fn remove_remote(&mut self, version: ProtocolVersion) -> io::Result<Vec<Packet>> {
        let remote = std::mem::take(&mut self.remote);
        removal(version, remote.iter())
    }

    /// Packets that bring the client to `view`, skipping `own` and every entry
    /// the backend already lists.
    pub(crate) fn sync(
        &mut self,
        version: ProtocolVersion,
        own: Option<[u8; 16]>,
        view: &TabView,
    ) -> io::Result<Vec<Packet>> {
        let Some(ids) = *self.ids.get_or_insert_with(|| ids(version)) else {
            return Ok(Vec::new());
        };
        let mut packets = Vec::new();
        match &view.header {
            Some(header) if self.header.as_ref() != Some(header) => {
                packets.push(header_packet(version, ids, &header.0, &header.1)?);
                self.header = Some(header.clone());
                self.owns_header = true;
            }
            Some(_) => {}
            None if self.owns_header => {
                // Hand the header back to the backend with a blank one; it
                // sends its own again whenever its plugins update it.
                let blank = Json::String(String::new());
                packets.push(header_packet(version, ids, &blank, &blank)?);
                self.header = None;
                self.owns_header = false;
            }
            None => {}
        }
        let wanted: Vec<&TabEntry> = view
            .players
            .iter()
            .flatten()
            .filter(|entry| Some(entry.uuid) != own && !self.backend.contains(&entry.uuid))
            .take(MAX_ENTRIES)
            .collect();
        let keep: BTreeSet<[u8; 16]> = wanted.iter().map(|entry| entry.uuid).collect();
        let gone: Vec<[u8; 16]> = self.remote.difference(&keep).copied().collect();
        packets.extend(removal(version, gone.iter())?);
        for uuid in &gone {
            self.remote.remove(uuid);
        }
        let added: Vec<&TabEntry> = wanted
            .into_iter()
            .filter(|entry| !self.remote.contains(&entry.uuid))
            .collect();
        for batch in added.chunks(BATCH) {
            packets.push(addition(version, ids, batch));
            self.remote.extend(batch.iter().map(|entry| entry.uuid));
        }
        Ok(packets)
    }
}

fn header_packet(
    version: ProtocolVersion,
    ids: TabIds,
    header: &Json,
    footer: &Json,
) -> io::Result<Packet> {
    let mut data = Vec::new();
    for text in [header, footer] {
        Text::Component(text).write(version.nbt_components(), &mut data)?;
    }
    Ok(Packet::new(ids.header, data))
}

fn addition(version: ProtocolVersion, ids: TabIds, entries: &[&TabEntry]) -> Packet {
    let number = version.number();
    let mut data = Vec::new();
    if number >= 761 {
        data.push(1 | 8); // Add player, update listed.
    } else {
        write_varint(0, &mut data);
    }
    write_varint(entries.len() as i32, &mut data);
    for entry in entries {
        data.extend(entry.uuid);
        write_string(&entry.name, &mut data);
        write_varint(entry.properties.len() as i32, &mut data);
        for property in entry.properties.iter() {
            write_string(&property.name, &mut data);
            write_string(&property.value, &mut data);
            data.push(u8::from(property.signature.is_some()));
            if let Some(signature) = &property.signature {
                write_string(signature, &mut data);
            }
        }
        if number >= 761 {
            data.push(1); // Listed.
        } else {
            data.extend([0, 0, 0]); // Survival, no latency, no display name.
            if number >= 759 {
                data.push(0); // No profile key.
            }
        }
    }
    Packet::new(ids.players, data)
}

fn removal<'a>(
    version: ProtocolVersion,
    uuids: impl ExactSizeIterator<Item = &'a [u8; 16]>,
) -> io::Result<Vec<Packet>> {
    if uuids.len() == 0 {
        return Ok(Vec::new());
    }
    let ids = ids(version).ok_or_else(|| invalid("missing tab list IDs"))?;
    let mut data = Vec::new();
    if version.number() < 761 {
        write_varint(4, &mut data);
    }
    write_varint(uuids.len() as i32, &mut data);
    for uuid in uuids {
        data.extend(uuid);
    }
    Ok(vec![Packet::new(
        if version.number() < 761 {
            ids.players
        } else {
            ids.remove_players
        },
        data,
    )])
}

#[cfg(test)]
#[path = "tab_list_tests.rs"]
mod tests;
