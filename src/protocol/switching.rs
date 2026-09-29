//! Explicit, tested switching contracts. Relay support does not imply switching.
//!
//! 1.21.8 and 1.21.11 share Join Game/SpawnInfo, signed command (including
//! checksum), and Brigadier parser layouts, but have different clientbound IDs.
//! Layouts checked against PrismarineJS minecraft-data data/pc/{1.21.8,1.21.11}/protocol.json:
//! https://github.com/PrismarineJS/minecraft-data/tree/master/data/pc
//! Add a pinned real-server switch AND recovery fixture in tests/servers.json
//! before adding a version here. Do not infer support from a protocol range.
use super::packets::{boolean, take};
use super::{
    Packet, ProtocolVersion, State, disconnect, invalid, read_string, read_varint, write_string,
    write_varint,
};
use std::io;

#[derive(Clone, Copy, Debug)]
pub(crate) struct SwitchingCapabilities {
    pub join_game: i32,
    pub commands: i32,
    pub bundle_delimiter: i32,
    pub play_information: i32,
    pub config_information: i32,
    pub play_payload: i32,
    pub config_payload: i32,
    pub config_pack_pop: i32,
    command: i32,
    signed_command: i32,
    chat_acknowledgement: i32,
    system_chat: i32,
}

const V1_21_8: SwitchingCapabilities = SwitchingCapabilities {
    join_game: 0x2b,
    commands: 0x10,
    bundle_delimiter: 0x00,
    play_information: 0x0d,
    config_information: 0x00,
    play_payload: 0x15,
    config_payload: 0x02,
    config_pack_pop: 0x08,
    command: 0x06,
    signed_command: 0x07,
    chat_acknowledgement: 0x05,
    system_chat: 0x72,
};
const V1_21_11: SwitchingCapabilities = SwitchingCapabilities {
    join_game: 0x30,
    system_chat: 0x77,
    ..V1_21_8
};

pub(super) fn for_version(version: ProtocolVersion) -> Option<SwitchingCapabilities> {
    match version.number() {
        772 => Some(V1_21_8),
        774 => Some(V1_21_11),
        _ => None,
    }
}

impl SwitchingCapabilities {
    pub(crate) fn validate_join(self, packet: &Packet, authenticated: bool) -> io::Result<()> {
        if packet.id != self.join_game {
            return Err(invalid("expected Join Game"));
        }
        let mut bytes = packet.data.as_slice();
        take(&mut bytes, 4)?; // entity id
        boolean(&mut bytes)?;
        let worlds = read_varint(&mut bytes)?;
        if worlds < 0 || worlds as usize > bytes.len() {
            return Err(invalid("invalid world count"));
        }
        for _ in 0..worlds {
            read_string(&mut bytes, 32767)?;
        }
        for _ in 0..3 {
            read_varint(&mut bytes)?;
        } // max players, view/simulation distance
        for _ in 0..3 {
            boolean(&mut bytes)?;
        }
        read_varint(&mut bytes)?; // dimension registry id
        read_string(&mut bytes, 32767)?; // world name
        take(&mut bytes, 10)?; // seed and game modes
        boolean(&mut bytes)?; // debug
        boolean(&mut bytes)?; // flat
        if boolean(&mut bytes)? {
            read_string(&mut bytes, 32767)?;
            take(&mut bytes, 8)?;
        }
        read_varint(&mut bytes)?; // portal cooldown
        read_varint(&mut bytes)?; // sea level
        let secure = boolean(&mut bytes)?;
        if !bytes.is_empty() {
            return Err(invalid("trailing Join Game data"));
        }
        // Modern online forwarding preserves the Mojang UUID used to validate the
        // client's secure chat key. Offline network sessions have no such identity.
        if secure && !authenticated {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Unauthenticated Rift network sessions require enforce-secure-profile=false on backends.",
            ));
        }
        Ok(())
    }

    /// Consume only proxy commands whose signature chain cannot be damaged. Signed
    /// arguments must reach their original backend unchanged. Empty signature lists
    /// may be consumed provided their last-seen offset is acknowledged separately,
    /// as in Velocity's SessionCommandHandler.consumeCommand.
    pub(crate) fn proxy_command(
        self,
        packet: &Packet,
    ) -> io::Result<Option<(String, Option<Packet>)>> {
        if packet.id != self.command && packet.id != self.signed_command {
            return Ok(None);
        }
        let mut bytes = packet.data.as_slice();
        let command = read_string(&mut bytes, 32767)?;
        let root = command.split_ascii_whitespace().next().unwrap_or("");
        if !matches!(root, "server" | "hub") {
            return Ok(None);
        }
        let mut acknowledgement = None;
        if packet.id == self.signed_command {
            take(&mut bytes, 16)?; // timestamp and salt
            let count = read_varint(&mut bytes)?;
            if !(0..=8).contains(&count) {
                return Err(invalid("invalid command signature count"));
            }
            for _ in 0..count {
                read_string(&mut bytes, 16)?;
                take(&mut bytes, 256)?;
            }
            let offset = read_varint(&mut bytes)?;
            if offset < 0 {
                return Err(invalid("invalid chat acknowledgement offset"));
            }
            take(&mut bytes, 4)?; // 20-bit acknowledgement set and 1.21.5+ checksum
            if !bytes.is_empty() {
                return Err(invalid("trailing signed command data"));
            }
            if count != 0 {
                return Ok(None);
            }
            if offset != 0 {
                let mut data = Vec::new();
                write_varint(offset, &mut data);
                acknowledgement = Some(Packet::new(self.chat_acknowledgement, data));
            }
        }
        if !bytes.is_empty() {
            return Err(invalid("trailing command data"));
        }
        Ok(Some((command.to_owned(), acknowledgement)))
    }

    pub(crate) fn system_message(
        self,
        version: ProtocolVersion,
        message: &str,
    ) -> io::Result<Packet> {
        // Disconnect and system-chat use the same anonymous NBT component encoding.
        let mut packet = disconnect(Some(version), State::Play, message)?;
        packet.id = self.system_chat;
        packet.data.push(0); // ordinary chat, not action bar
        Ok(packet)
    }

    /// Append unsigned Brigadier proxy literals to the backend's command
    /// tree. Existing node indexes and redirects stay valid; only root children
    /// with the same names are replaced. A brigadier:string argument is deliberately
    /// used instead of minecraft:message, which would require a signed argument.
    pub(crate) fn network_commands(self, packet: &Packet) -> io::Result<Packet> {
        if packet.id != self.commands {
            return Err(invalid("expected command tree"));
        }
        struct Node<'a> {
            raw: &'a [u8],
            flags: u8,
            children: Vec<i32>,
            tail: &'a [u8],
            name: Option<&'a str>,
        }
        let mut bytes = packet.data.as_slice();
        let count = read_varint(&mut bytes)?;
        if !(1..=65536).contains(&count) || count as usize > bytes.len() / 2 {
            return Err(invalid("invalid command node count"));
        }
        let index = |bytes: &mut &[u8]| -> io::Result<i32> {
            let value = read_varint(bytes)?;
            if value < 0 || value >= count {
                return Err(invalid("invalid command node index"));
            }
            Ok(value)
        };
        let mut nodes = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let start = bytes;
            let flags = take(&mut bytes, 1)?[0];
            let child_count = read_varint(&mut bytes)?;
            if child_count < 0 || child_count as usize > bytes.len() {
                return Err(invalid("invalid command child count"));
            }
            let mut children = Vec::with_capacity(child_count as usize);
            for _ in 0..child_count {
                children.push(index(&mut bytes)?);
            }
            let tail = bytes;
            if flags & 8 != 0 {
                index(&mut bytes)?;
            }
            let name = match flags & 3 {
                0 => None,
                1 => Some(read_string(&mut bytes, 32767)?),
                2 => {
                    let name = read_string(&mut bytes, 32767)?;
                    match read_varint(&mut bytes)? {
                        parser @ 1..=4 => {
                            let bounds = take(&mut bytes, 1)?[0];
                            if bounds & !3 != 0 {
                                return Err(invalid("invalid command argument bounds"));
                            }
                            let width = if parser == 2 || parser == 4 { 8 } else { 4 };
                            if bounds & 1 != 0 {
                                take(&mut bytes, width)?;
                            }
                            if bounds & 2 != 0 {
                                take(&mut bytes, width)?;
                            }
                        }
                        5 => {
                            if !(0..=2).contains(&read_varint(&mut bytes)?) {
                                return Err(invalid("invalid string argument type"));
                            }
                        }
                        6 | 31 => {
                            take(&mut bytes, 1)?;
                        }
                        43 => {
                            take(&mut bytes, 4)?;
                        }
                        44..=48 => {
                            read_string(&mut bytes, 32767)?;
                        }
                        0..=56 => {}
                        _ => {
                            return Err(invalid("unknown command parser for switching capability"));
                        }
                    }
                    if flags & 16 != 0 {
                        read_string(&mut bytes, 32767)?;
                    }
                    Some(name)
                }
                _ => return Err(invalid("invalid command node type")),
            };
            nodes.push(Node {
                raw: &start[..start.len() - bytes.len()],
                flags,
                children,
                tail: &tail[..tail.len() - bytes.len()],
                name,
            });
        }
        let root = index(&mut bytes)? as usize;
        if !bytes.is_empty() || nodes[root].flags & 3 != 0 {
            return Err(invalid("invalid command root"));
        }
        let mut data = Vec::with_capacity(packet.data.len() + 40);
        write_varint(count + 3, &mut data);
        for (i, node) in nodes.iter().enumerate() {
            if i != root {
                data.extend_from_slice(node.raw);
                continue;
            }
            let children: Vec<_> = node
                .children
                .iter()
                .copied()
                .filter(|child| !matches!(nodes[*child as usize].name, Some("server" | "hub")))
                .collect();
            data.push(node.flags);
            write_varint(children.len() as i32 + 2, &mut data);
            for child in children {
                write_varint(child, &mut data);
            }
            write_varint(count, &mut data);
            write_varint(count + 2, &mut data);
            data.extend_from_slice(node.tail);
        }
        data.push(5);
        write_varint(1, &mut data);
        write_varint(count + 1, &mut data);
        write_string("server", &mut data);
        data.push(6);
        write_varint(0, &mut data);
        write_string("name", &mut data);
        write_varint(5, &mut data);
        write_varint(2, &mut data);
        data.push(5);
        write_varint(0, &mut data);
        write_string("hub", &mut data);
        write_varint(root as i32, &mut data);
        Ok(Packet::new(packet.id, data))
    }
}
