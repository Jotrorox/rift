//! Explicit, tested switching contracts. Relay support does not imply switching.
//!
//! Layouts checked against PrismarineJS minecraft-data and Mojang server artifacts:
//! https://github.com/PrismarineJS/minecraft-data/tree/master/data/pc
//! See docs/network-protocol.md for the version matrix and source references.
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
    protocol: i32,
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
    sea_level: bool,
    varint_game_modes: bool,
    online_mode: bool,
    chat_checksum: bool,
    score_holder_parser: i32,
    time_parser: i32,
    first_resource_parser: i32,
    last_resource_parser: i32,
    last_parser: i32,
}

const V1_21_8: SwitchingCapabilities = SwitchingCapabilities {
    protocol: 772,
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
    sea_level: true,
    varint_game_modes: false,
    online_mode: false,
    chat_checksum: true,
    score_holder_parser: 31,
    time_parser: 43,
    first_resource_parser: 44,
    last_resource_parser: 48,
    last_parser: 56,
};
const V1_21: SwitchingCapabilities = SwitchingCapabilities {
    commands: 0x11,
    play_information: 0x0a,
    play_payload: 0x12,
    command: 0x04,
    signed_command: 0x05,
    chat_acknowledgement: 0x03,
    system_chat: 0x6c,
    sea_level: false,
    chat_checksum: false,
    score_holder_parser: 30,
    time_parser: 42,
    first_resource_parser: 43,
    last_resource_parser: 46,
    last_parser: 53,
    ..V1_21_8
};
const V1_21_2: SwitchingCapabilities = SwitchingCapabilities {
    join_game: 0x2c,
    play_information: 0x0c,
    play_payload: 0x14,
    command: 0x05,
    signed_command: 0x06,
    chat_acknowledgement: 0x04,
    system_chat: 0x73,
    sea_level: true,
    ..V1_21
};
const V1_21_5: SwitchingCapabilities = SwitchingCapabilities {
    join_game: 0x2b,
    commands: 0x10,
    system_chat: 0x72,
    chat_checksum: true,
    last_resource_parser: 47,
    last_parser: 54,
    ..V1_21_2
};
const V1_21_11: SwitchingCapabilities = SwitchingCapabilities {
    join_game: 0x30,
    system_chat: 0x77,
    ..V1_21_8
};
const V26_1: SwitchingCapabilities = SwitchingCapabilities {
    join_game: 0x31,
    play_information: 0x0e,
    play_payload: 0x16,
    command: 0x07,
    signed_command: 0x08,
    chat_acknowledgement: 0x06,
    system_chat: 0x79,
    ..V1_21_8
};
const V26_2: SwitchingCapabilities = SwitchingCapabilities {
    online_mode: true,
    ..V26_1
};
const V26_3: SwitchingCapabilities = SwitchingCapabilities {
    join_game: 0x32,
    system_chat: 0x7c,
    varint_game_modes: true,
    last_parser: 61,
    ..V26_2
};

pub(super) fn for_version(version: ProtocolVersion) -> Option<SwitchingCapabilities> {
    match version.number() {
        767 => Some(V1_21),
        768 | 769 => Some(V1_21_2),
        770 => Some(V1_21_5),
        771 | 772 => Some(V1_21_8),
        773 | 774 => Some(V1_21_11),
        775 => Some(V26_1),
        776 => Some(V26_2),
        777 => Some(V26_3),
        number => legacy_capabilities(number),
    }
    .map(|mut caps| {
        caps.protocol = version.number();
        caps
    })
}

include!("legacy_capabilities.rs");

impl SwitchingCapabilities {
    fn string_parser(self, data: &mut Vec<u8>) {
        if self.protocol < 759 {
            write_string("brigadier:string", data);
        } else {
            write_varint(5, data);
        }
    }
    pub(crate) fn legacy_world(self, packet: &Packet) -> io::Result<(Packet, Packet)> {
        super::legacy::world(self.protocol, packet)
    }
    pub(crate) fn legacy_keepalive(self) -> Option<(i32, i32)> {
        super::legacy::ids(self.protocol).map(|ids| (ids.keepalive, ids.keepalive_reply))
    }
    pub(crate) fn validate_join(self, packet: &Packet, authenticated: bool) -> io::Result<()> {
        if packet.id != self.join_game {
            return Err(invalid("expected Join Game"));
        }
        if self.protocol < 764 {
            super::legacy::world(self.protocol, packet)?;
            return Ok(());
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
        if self.protocol < 766 {
            read_string(&mut bytes, 32767)?;
        } else {
            read_varint(&mut bytes)?;
        } // Dimension registry ID since 1.20.5.
        read_string(&mut bytes, 32767)?; // world name
        take(&mut bytes, 8)?; // seed
        if self.varint_game_modes {
            read_varint(&mut bytes)?;
            read_varint(&mut bytes)?; // Optional VarInt: zero means absent.
        } else {
            take(&mut bytes, 2)?;
        }
        boolean(&mut bytes)?; // debug
        boolean(&mut bytes)?; // flat
        if boolean(&mut bytes)? {
            read_string(&mut bytes, 32767)?;
            take(&mut bytes, 8)?;
        }
        read_varint(&mut bytes)?; // portal cooldown
        if self.sea_level {
            read_varint(&mut bytes)?; // Added in 1.21.2.
        }
        if self.online_mode {
            boolean(&mut bytes)?;
        }
        let secure = self.protocol >= 766 && boolean(&mut bytes)?;
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
    #[cfg(test)]
    pub(crate) fn proxy_command(
        self,
        packet: &Packet,
    ) -> io::Result<Option<(String, Option<Packet>)>> {
        self.proxy_command_with(packet, &[])
    }

    pub(crate) fn proxy_command_with(
        self,
        packet: &Packet,
        commands: &[String],
    ) -> io::Result<Option<(String, Option<Packet>)>> {
        if packet.id != self.command && packet.id != self.signed_command {
            return Ok(None);
        }
        let mut bytes = packet.data.as_slice();
        let raw = read_string(&mut bytes, 32767)?;
        let command = if self.protocol < 759 {
            let Some(command) = raw.strip_prefix('/') else {
                return Ok(None);
            };
            command
        } else {
            raw
        };
        let root = command.split_ascii_whitespace().next().unwrap_or("");
        if !matches!(root, "server" | "hub") && !commands.iter().any(|name| name == root) {
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
                if self.protocol <= 760 {
                    signature(&mut bytes)?;
                } else {
                    take(&mut bytes, 256)?;
                }
            }
            if self.protocol <= 760 {
                boolean(&mut bytes)?; // Signed preview.
                if self.protocol == 760 {
                    let update = bytes;
                    let seen = read_varint(&mut bytes)?;
                    if !(0..=5).contains(&seen) {
                        return Err(invalid("invalid last-seen message count"));
                    }
                    for _ in 0..seen {
                        take(&mut bytes, 16)?;
                        signature(&mut bytes)?;
                    }
                    if boolean(&mut bytes)? {
                        take(&mut bytes, 16)?;
                        signature(&mut bytes)?;
                    }
                    acknowledgement = Some(Packet::new(self.chat_acknowledgement, update.to_vec()));
                }
                if !bytes.is_empty() {
                    return Err(invalid("trailing signed command data"));
                }
                return if count == 0 {
                    Ok(Some((command.to_owned(), acknowledgement)))
                } else {
                    Ok(None)
                };
            }
            let offset = read_varint(&mut bytes)?;
            if offset < 0 {
                return Err(invalid("invalid chat acknowledgement offset"));
            }
            take(&mut bytes, if self.chat_checksum { 4 } else { 3 })?;
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
        // Disconnect and system chat share the version's component encoding.
        let mut packet = disconnect(Some(version), State::Play, message)?;
        packet.id = self.system_chat;
        // Through 1.19, position 1 is system text (a byte before 1.19, then a
        // VarInt). 1.19.1 replaces the position with an overlay boolean.
        packet.data.push(u8::from(self.protocol <= 759));
        if (735..759).contains(&self.protocol) {
            packet.data.extend([0; 16]);
        }
        Ok(packet)
    }

    /// Append unsigned Brigadier proxy literals to the backend's command
    /// tree. Existing node indexes and redirects stay valid; only root children
    /// with the same names are replaced. A brigadier:string argument is deliberately
    /// used instead of minecraft:message, which would require a signed argument.
    #[cfg(test)]
    pub(crate) fn network_commands(self, packet: &Packet) -> io::Result<Packet> {
        self.network_commands_with(packet, &[])
    }

    pub(crate) fn network_commands_with(
        self,
        packet: &Packet,
        commands: &[String],
    ) -> io::Result<Packet> {
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
                    let parser = if self.protocol < 759 {
                        named_parser(read_string(&mut bytes, 32767)?)?
                    } else {
                        read_varint(&mut bytes)?
                    };
                    match parser {
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
                        parser if parser == 6 || parser == self.score_holder_parser => {
                            take(&mut bytes, 1)?;
                        }
                        parser if parser == self.time_parser => {
                            take(&mut bytes, 4)?;
                        }
                        parser
                            if (self.first_resource_parser..=self.last_resource_parser)
                                .contains(&parser) =>
                        {
                            read_string(&mut bytes, 32767)?;
                        }
                        parser if (0..=self.last_parser).contains(&parser) => {}
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
        write_varint(count + 3 + commands.len() as i32 * 2, &mut data);
        for (i, node) in nodes.iter().enumerate() {
            if i != root {
                data.extend_from_slice(node.raw);
                continue;
            }
            let children: Vec<_> = node
                .children
                .iter()
                .copied()
                .filter(|child| {
                    !matches!(nodes[*child as usize].name, Some("server" | "hub"))
                        && !commands
                            .iter()
                            .any(|name| Some(name.as_str()) == nodes[*child as usize].name)
                })
                .collect();
            data.push(node.flags);
            write_varint(children.len() as i32 + 2 + commands.len() as i32, &mut data);
            for child in children {
                write_varint(child, &mut data);
            }
            write_varint(count, &mut data);
            write_varint(count + 2, &mut data);
            for i in 0..commands.len() {
                write_varint(count + 3 + i as i32 * 2, &mut data);
            }
            data.extend_from_slice(node.tail);
        }
        data.push(5);
        write_varint(1, &mut data);
        write_varint(count + 1, &mut data);
        write_string("server", &mut data);
        data.push(6);
        write_varint(0, &mut data);
        write_string("name", &mut data);
        self.string_parser(&mut data);
        write_varint(2, &mut data);
        data.push(5);
        write_varint(0, &mut data);
        write_string("hub", &mut data);
        for (i, name) in commands.iter().enumerate() {
            data.push(5); // executable literal, optional greedy unsigned string
            write_varint(1, &mut data);
            write_varint(count + 4 + i as i32 * 2, &mut data);
            write_string(name, &mut data);
            data.push(6);
            write_varint(0, &mut data);
            write_string("args", &mut data);
            self.string_parser(&mut data);
            write_varint(2, &mut data);
        }
        write_varint(root as i32, &mut data);
        Ok(Packet::new(packet.id, data))
    }
}

fn signature(bytes: &mut &[u8]) -> io::Result<()> {
    let size = read_varint(bytes)?;
    if !(0..=8192).contains(&size) {
        return Err(invalid("invalid signature length"));
    }
    take(bytes, size as usize)?;
    Ok(())
}
