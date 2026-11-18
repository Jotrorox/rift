//! Tab list header/footer and the entries Rift lists for players on other
//! servers. Rift records which entries the current backend added, so it only
//! adds players elsewhere on the network and never removes a backend's entry.
//! With `tab_list.names = "all"` it also sets the display name and list order
//! of the backend's own entries, and sets them again after the backend does.
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
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::Arc,
};

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

/// What a player list packet does to one entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Change {
    Added,
    Removed,
    /// The display name or list order of a listed entry changed.
    Renamed,
}

/// Report each entry a backend player list packet adds, removes or renames.
/// An addition is reported once, whatever else the packet sets with it.
/// Returns whether the packet was a player list packet at all.
pub(super) fn changes(
    version: ProtocolVersion,
    ids: TabIds,
    packet: &Packet,
    mut change: impl FnMut([u8; 16], Change),
) -> io::Result<bool> {
    let number = version.number();
    let mut bytes = packet.data.as_slice();
    if packet.id == ids.remove_players {
        for _ in 0..count(&mut bytes)? {
            change(take(&mut bytes, 16)?.try_into().unwrap(), Change::Removed);
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
                    change(uuid, Change::Added);
                } else if action & (32 | 64) != 0 {
                    change(uuid, Change::Renamed);
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
                        change(uuid, Change::Added);
                    }
                    1 | 2 => {
                        read_varint(&mut bytes)?;
                    }
                    3 => {
                        display(&mut bytes, false)?;
                        change(uuid, Change::Renamed);
                    }
                    4 => change(uuid, Change::Removed),
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
    /// The entry's display name, a text component; `None` shows the name.
    pub display: Option<Arc<Json>>,
    /// The list order clients from 1.21.2 sort by, highest first.
    pub order: i32,
}

/// A player's display name and list order, for an entry the backend lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TabName {
    /// A text component; `None` shows the name.
    pub display: Option<Arc<Json>>,
    /// The list order clients from 1.21.2 sort by, highest first.
    pub order: i32,
}

/// The display name and list order a client has for an entry Rift set.
type Shown = (Option<Arc<Json>>, i32);

/// What an entry shows on `version`; the list order exists from 1.21.2.
fn shown_as(version: ProtocolVersion, display: &Option<Arc<Json>>, order: i32) -> Shown {
    let order = if version.number() >= 768 { order } else { 0 };
    (display.clone(), order)
}

fn shown(version: ProtocolVersion, entry: &TabEntry) -> Shown {
    shown_as(version, &entry.display, entry.order)
}

/// What Rift wants the client's tab list to show.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabView {
    /// Header and footer text components. `None` leaves both to the backend.
    pub header: Option<(Json, Json)>,
    /// Players to list in addition to the backend's own entries. `None` lists
    /// only the backend's players.
    pub players: Option<Vec<TabEntry>>,
    /// Display names and list orders for entries the backend lists, by UUID.
    /// `None` leaves them to the backend. A listed player missing here keeps
    /// the backend's name, unless Rift named them before.
    pub names: Option<BTreeMap<[u8; 16], TabName>>,
}

/// One client's tab list, as far as Rift has changed or observed it.
#[derive(Debug, Default)]
pub(crate) struct TabList {
    /// Entries the current backend added.
    backend: BTreeSet<[u8; 16]>,
    /// Entries Rift added for players on other servers, as the client shows
    /// them.
    remote: BTreeMap<[u8; 16], Shown>,
    /// Backend entries whose display name and list order Rift set, as the
    /// client shows them.
    named: BTreeMap<[u8; 16], Shown>,
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
        let (backend, remote, named) = (&mut self.backend, &mut self.remote, &mut self.named);
        let mut changed = false;
        changes(version, ids, packet, |uuid, change| {
            // The backend replaced or removed whatever name Rift set; a
            // later sync sets it again.
            changed |= named.remove(&uuid).is_some();
            if change == Change::Renamed {
                return;
            }
            // Either way the client no longer has a Rift entry for this player:
            // an add replaces it and a removal deletes it.
            changed |= remote.remove(&uuid).is_some();
            changed |= if change == Change::Added {
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
        self.named.clear();
        self.header = None;
        self.revision += 1;
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn modified(&self) -> bool {
        self.owns_header || !self.remote.is_empty() || !self.named.is_empty()
    }

    /// Remove every Rift entry, for a legacy switch that keeps the client's list.
    pub(crate) fn remove_remote(&mut self, version: ProtocolVersion) -> io::Result<Vec<Packet>> {
        let remote = std::mem::take(&mut self.remote);
        removal(version, remote.keys())
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
        let gone: Vec<[u8; 16]> = self
            .remote
            .keys()
            .filter(|uuid| !keep.contains(*uuid))
            .copied()
            .collect();
        packets.extend(removal(version, gone.iter())?);
        for uuid in &gone {
            self.remote.remove(uuid);
        }
        let (added, kept): (Vec<&TabEntry>, Vec<&TabEntry>) = wanted
            .into_iter()
            .partition(|entry| !self.remote.contains_key(&entry.uuid));
        // Listed players whose prefix, suffix or rank changed.
        let mut changed: Vec<([u8; 16], Shown)> = kept
            .into_iter()
            .map(|entry| (entry.uuid, shown(version, entry)))
            .filter(|(uuid, shown)| self.remote.get(uuid) != Some(shown))
            .collect();
        for batch in added.chunks(BATCH) {
            packets.push(addition(version, ids, batch)?);
        }
        for entry in added {
            self.remote.insert(entry.uuid, shown(version, entry));
        }
        for (uuid, shown) in &changed {
            self.remote.insert(*uuid, shown.clone());
        }
        // The backend's own entries, the player's included.
        let plain: Shown = (None, 0);
        let renamed: Vec<([u8; 16], Shown)> = self
            .backend
            .iter()
            .filter_map(|uuid| {
                let wanted = view
                    .names
                    .as_ref()
                    .and_then(|names| names.get(uuid))
                    .map_or_else(
                        || plain.clone(),
                        |name| shown_as(version, &name.display, name.order),
                    );
                match self.named.get(uuid) {
                    Some(current) if *current == wanted => None,
                    // Never named by Rift and nothing to show: the backend's.
                    None if wanted == plain => None,
                    _ => Some((*uuid, wanted)),
                }
            })
            .collect();
        for (uuid, shown) in &renamed {
            if *shown == plain {
                self.named.remove(uuid);
            } else {
                self.named.insert(*uuid, shown.clone());
            }
        }
        changed.extend(renamed);
        for batch in changed.chunks(BATCH) {
            packets.push(update(version, ids, batch)?);
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
        // Rendered for the client like every component Rift sends.
        let converted = crate::text::for_client(text, Some(version));
        Text::Component(converted.as_ref().unwrap_or(text))
            .write(version.nbt_components(), &mut data)?;
    }
    Ok(Packet::new(ids.header, data))
}

/// An optional display name: a JSON string, or NBT from 1.20.3. Rendered for
/// the client like every component Rift sends.
fn write_display(
    version: ProtocolVersion,
    display: Option<&Json>,
    data: &mut Vec<u8>,
) -> io::Result<()> {
    data.push(u8::from(display.is_some()));
    if let Some(text) = display {
        let converted = crate::text::for_client(text, Some(version));
        Text::Component(converted.as_ref().unwrap_or(text))
            .write(version.nbt_components(), data)?;
    }
    Ok(())
}

fn addition(version: ProtocolVersion, ids: TabIds, entries: &[&TabEntry]) -> io::Result<Packet> {
    let number = version.number();
    // Display names and list orders are sent only when an entry has one.
    let named = entries.iter().any(|entry| entry.display.is_some());
    let ordered = number >= 768 && entries.iter().any(|entry| entry.order != 0);
    let mut data = Vec::new();
    if number >= 761 {
        // Add player, update listed; update display name, update list order.
        data.push(1 | 8 | if named { 32 } else { 0 } | if ordered { 64 } else { 0 });
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
        let display = entry.display.as_deref();
        if number >= 761 {
            data.push(1); // Listed.
            if named {
                write_display(version, display, &mut data)?;
            }
            if ordered {
                write_varint(entry.order, &mut data);
            }
        } else {
            data.extend([0, 0]); // Survival, no latency.
            write_display(version, display, &mut data)?;
            if number >= 759 {
                data.push(0); // No profile key.
            }
        }
    }
    Ok(Packet::new(ids.players, data))
}

/// Set the display name, and from 1.21.2 the list order, of listed entries.
fn update(
    version: ProtocolVersion,
    ids: TabIds,
    entries: &[([u8; 16], Shown)],
) -> io::Result<Packet> {
    let number = version.number();
    let mut data = Vec::new();
    if number >= 761 {
        // Update display name, update list order.
        data.push(32 | if number >= 768 { 64 } else { 0 });
    } else {
        write_varint(3, &mut data); // Update display name.
    }
    write_varint(entries.len() as i32, &mut data);
    for (uuid, (display, order)) in entries {
        data.extend(uuid);
        write_display(version, display.as_deref(), &mut data)?;
        if number >= 768 {
            write_varint(*order, &mut data);
        }
    }
    Ok(Packet::new(ids.players, data))
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
