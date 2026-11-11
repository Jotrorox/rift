//! Explicit, tested switching contracts. Relay support does not imply switching.
//!
//! Layouts checked against PrismarineJS minecraft-data and Mojang server artifacts:
//! https://github.com/PrismarineJS/minecraft-data/tree/master/data/pc
//! See docs/network-protocol.md for the version matrix and source references.
//! Add a pinned real-server switch AND recovery fixture in tests/servers.json
//! before adding a version here. Do not infer support from a protocol range.
use super::packets::{Text, boolean, disconnect_text, take, with_text};
use std::io;

/// One Command Suggestions Response entry. Tooltips are plain text or a
/// component, rendered for the client.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Suggestion {
    pub text: String,
    pub tooltip: Option<crate::text::Text>,
}
use super::{
    Packet, ProtocolVersion, State, invalid, read_string, read_varint, write_string, write_varint,
};

/// Where a version's title packets live.
enum TitleIds {
    /// Before 1.17: one Title packet, and the action number of the times.
    Combined(i32, i32),
    /// Subtitle, title and times packets.
    Separate(i32, i32, i32),
}

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
    /// Clientbound Respawn and Game Event, read only for the player's
    /// `dimension` and `gamemode` contexts.
    respawn: i32,
    game_event: i32,
    pub config_pack_pop: i32,
    command: i32,
    signed_command: i32,
    /// Serverbound Chat Message. Before 1.19 it is `command` as well: a
    /// leading `/` marks a command.
    chat: i32,
    /// Serverbound Player Session (chat session update); -1 before 1.19.3.
    chat_session: i32,
    chat_acknowledgement: i32,
    system_chat: i32,
    /// Serverbound Command Suggestions Request and clientbound Response; -1
    /// before 1.19.3, where Rift does not answer suggestion requests.
    suggestion_request: i32,
    suggestion_response: i32,
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
    respawn: 0x4b,
    game_event: 0x22,
    config_pack_pop: 0x08,
    command: 0x06,
    signed_command: 0x07,
    chat: 0x08,
    chat_session: 0x09,
    chat_acknowledgement: 0x05,
    system_chat: 0x72,
    suggestion_request: 0x0e,
    suggestion_response: 0x0f,
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
    respawn: 0x47,
    command: 0x04,
    signed_command: 0x05,
    chat: 0x06,
    chat_session: 0x07,
    chat_acknowledgement: 0x03,
    system_chat: 0x6c,
    suggestion_request: 0x0b,
    suggestion_response: 0x10,
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
    respawn: 0x4c,
    game_event: 0x23,
    command: 0x05,
    signed_command: 0x06,
    chat: 0x07,
    chat_session: 0x08,
    chat_acknowledgement: 0x04,
    system_chat: 0x73,
    suggestion_request: 0x0d,
    sea_level: true,
    ..V1_21
};
const V1_21_5: SwitchingCapabilities = SwitchingCapabilities {
    join_game: 0x2b,
    commands: 0x10,
    respawn: 0x4b,
    game_event: 0x22,
    system_chat: 0x72,
    suggestion_response: 0x0f,
    chat_checksum: true,
    last_resource_parser: 47,
    last_parser: 54,
    ..V1_21_2
};
const V1_21_11: SwitchingCapabilities = SwitchingCapabilities {
    join_game: 0x30,
    respawn: 0x50,
    game_event: 0x26,
    system_chat: 0x77,
    ..V1_21_8
};
const V26_1: SwitchingCapabilities = SwitchingCapabilities {
    join_game: 0x31,
    play_information: 0x0e,
    play_payload: 0x16,
    respawn: 0x52,
    game_event: 0x26,
    command: 0x07,
    signed_command: 0x08,
    chat: 0x09,
    chat_session: 0x0a,
    chat_acknowledgement: 0x06,
    system_chat: 0x79,
    suggestion_request: 0x0f,
    ..V1_21_8
};
const V26_2: SwitchingCapabilities = SwitchingCapabilities {
    online_mode: true,
    ..V26_1
};
const V26_3: SwitchingCapabilities = SwitchingCapabilities {
    join_game: 0x32,
    respawn: 0x54,
    game_event: 0x27,
    system_chat: 0x7c,
    varint_game_modes: true,
    last_parser: 61,
    ..V26_2
};

/// What a serverbound chat packet asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatKind {
    Message,
    Command,
}

/// A chat message or command a client sent, read on the client's own protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChatAttempt {
    pub kind: ChatKind,
    /// The message, or the command line without its leading slash.
    pub text: String,
    /// Whether the packet carries a message signature or at least one
    /// argument signature. A backend that verifies the player's chat counts
    /// such a packet in the signature chain, so withholding it invalidates
    /// everything the player signs afterwards. It may be dropped only when
    /// the backend takes the player's chat as unsigned: with
    /// `authentication.signed_chat = false`, or across a translation step
    /// that removes signing. An unsigned packet may always be dropped.
    pub signed: bool,
    /// The acknowledgement the backend must still receive if the packet is
    /// dropped: the packet's last-seen update, as a separate packet. `None`
    /// where the packet acknowledges nothing.
    pub ack: Option<Packet>,
}

const OVERWORLD: &str = "minecraft:overworld";
const NETHER: &str = "minecraft:the_nether";
const END: &str = "minecraft:the_end";

/// The `dimension` context for a world: its namespaced key, which for the
/// vanilla worlds (and Paper's main three) is also the dimension type's key.
fn dimension(world: &str) -> Option<String> {
    (!world.is_empty()).then(|| world.to_owned())
}

/// A player's game mode, as Join Game, Respawn and Game Event carry it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameMode {
    Survival,
    Creative,
    Adventure,
    Spectator,
}

impl GameMode {
    fn from_id(id: i64) -> Option<Self> {
        Some(match id {
            0 => Self::Survival,
            1 => Self::Creative,
            2 => Self::Adventure,
            3 => Self::Spectator,
            _ => return None,
        })
    }

    /// The `gamemode` context value, spelled as the backend adapters report it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Survival => "survival",
            Self::Creative => "creative",
            Self::Adventure => "adventure",
            Self::Spectator => "spectator",
        }
    }
}

/// What one clientbound play packet says about where the player is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WorldUpdate {
    /// Join Game or Respawn: the player is in a new world. `None` for a
    /// value the packet does not name in a form Rift can map.
    Spawn {
        dimension: Option<String>,
        gamemode: Option<GameMode>,
    },
    /// Game Event 3: the game mode changed in place.
    GameMode(GameMode),
}

/// The dimension and game mode the backend last told the client, for the
/// proxy's `dimension` and `gamemode` contexts. The proxy never learns a
/// Bukkit world name, so `world` comes only from backends.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlayerWorld {
    pub dimension: Option<String>,
    pub gamemode: Option<GameMode>,
}

impl PlayerWorld {
    /// Apply an update; `true` when anything changed.
    pub(crate) fn apply(&mut self, update: WorldUpdate) -> bool {
        match update {
            WorldUpdate::Spawn {
                dimension,
                gamemode,
            } => {
                let changed = self.dimension != dimension || self.gamemode != gamemode;
                self.dimension = dimension;
                self.gamemode = gamemode;
                changed
            }
            WorldUpdate::GameMode(mode) => self.gamemode.replace(mode) != Some(mode),
        }
    }
}

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
    /// An executable greedy string leaf. Suggestions use
    /// `minecraft:ask_server`, which Rift answers itself.
    fn string_argument(self, name: &str, suggest: bool, data: &mut Vec<u8>) {
        data.push(if suggest { 0x16 } else { 0x06 });
        write_varint(0, data);
        write_string(name, data);
        self.string_parser(data);
        write_varint(2, data);
        if suggest {
            write_string("minecraft:ask_server", data);
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
        let (_, secure) = self.join_game_fields(packet)?;
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

    /// A 1.20.2+ Join Game, checked to its last byte: the world it spawns
    /// the player in and whether the backend enforces secure profiles.
    fn join_game_fields(self, packet: &Packet) -> io::Result<(WorldUpdate, bool)> {
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
        let world = self.spawn_info(&mut bytes)?;
        if self.online_mode {
            boolean(&mut bytes)?;
        }
        let secure = self.protocol >= 766 && boolean(&mut bytes)?;
        if !bytes.is_empty() {
            return Err(invalid("trailing Join Game data"));
        }
        Ok((world, secure))
    }

    /// The spawn info shared by 1.20.2+ Join Game and Respawn
    /// (`CommonPlayerSpawnInfo`). A game mode outside 0-3 is not an error;
    /// it only leaves `gamemode` unknown.
    fn spawn_info(self, bytes: &mut &[u8]) -> io::Result<WorldUpdate> {
        if self.protocol < 766 {
            read_string(bytes, 32767)?;
        } else {
            read_varint(bytes)?;
        } // Dimension registry ID since 1.20.5.
        let world = read_string(bytes, 32767)?;
        take(bytes, 8)?; // seed
        let mode = if self.varint_game_modes {
            let mode = read_varint(bytes)?;
            read_varint(bytes)?; // Optional VarInt: zero means absent.
            i64::from(mode)
        } else {
            i64::from(take(bytes, 2)?[0] as i8)
        };
        boolean(bytes)?; // debug
        boolean(bytes)?; // flat
        if boolean(bytes)? {
            read_string(bytes, 32767)?;
            take(bytes, 8)?;
        }
        read_varint(bytes)?; // portal cooldown
        if self.sea_level {
            read_varint(bytes)?; // Added in 1.21.2.
        }
        Ok(WorldUpdate::Spawn {
            dimension: dimension(world),
            gamemode: GameMode::from_id(mode),
        })
    }

    /// What a clientbound play packet says about the player's `dimension`
    /// and `gamemode`: Join Game and Respawn name both, Game Event 3 the game
    /// mode alone. Every other packet costs three ID comparisons. Context
    /// tracking must never break the relay, so a packet that does not parse
    /// exactly is `None` (skip the update), except a Join Game, which always
    /// starts a new world and so forgets what it cannot read.
    pub(crate) fn world_update(self, packet: &Packet) -> Option<WorldUpdate> {
        if packet.id == self.game_event {
            // Unsigned byte event and float value since 1.8. Event 3 changes
            // the game mode to the value's ID.
            let [3, value @ ..] = packet.data.as_slice() else {
                return None;
            };
            let value = f32::from_be_bytes(value.try_into().ok()?);
            if value.fract() != 0.0 {
                return None;
            }
            return GameMode::from_id(value as i64).map(WorldUpdate::GameMode);
        }
        if packet.id == self.join_game {
            let world = if self.protocol < 764 {
                super::legacy::world(self.protocol, packet)
                    .and_then(|(_, respawn)| self.legacy_respawn(&respawn.data))
            } else {
                self.join_game_fields(packet).map(|(world, _)| world)
            };
            return Some(world.unwrap_or(WorldUpdate::Spawn {
                dimension: None,
                gamemode: None,
            }));
        }
        if packet.id != self.respawn {
            return None;
        }
        if self.protocol < 764 {
            return self.legacy_respawn(&packet.data).ok();
        }
        let mut bytes = packet.data.as_slice();
        let world = self.spawn_info(&mut bytes).ok()?;
        take(&mut bytes, 1).ok()?; // data kept
        bytes.is_empty().then_some(world)
    }

    /// A pre-1.20.2 Respawn, in the layouts `legacy::world` writes.
    fn legacy_respawn(self, data: &[u8]) -> io::Result<WorldUpdate> {
        let version = self.protocol;
        let mut bytes = data;
        let (dimension, mode) = if version < 735 {
            let id = i32::from_be_bytes(take(&mut bytes, 4)?.try_into().unwrap());
            if version < 477 {
                take(&mut bytes, 1)?; // difficulty
            }
            if version >= 573 {
                take(&mut bytes, 8)?; // seed
            }
            let mode = take(&mut bytes, 1)?[0];
            read_string(&mut bytes, 16)?; // level type
            let dimension = match id {
                -1 => Some(NETHER.to_owned()),
                0 => Some(OVERWORLD.to_owned()),
                1 => Some(END.to_owned()),
                _ => None,
            };
            // Bit 3 was the hardcore flag before 1.16.2.
            (dimension, i64::from(mode & 7))
        } else {
            if (751..=758).contains(&version) {
                super::nbt::named(&mut bytes)?;
            } else {
                read_string(&mut bytes, 32767)?;
            } // dimension type
            let world = read_string(&mut bytes, 32767)?;
            take(&mut bytes, 8)?; // seed
            let modes = take(&mut bytes, 2)?;
            let mode = if version < 751 {
                i64::from(modes[0] & 7)
            } else {
                i64::from(modes[0] as i8)
            };
            boolean(&mut bytes)?; // debug
            boolean(&mut bytes)?; // flat
            take(&mut bytes, 1)?; // copy metadata
            if version >= 759 && boolean(&mut bytes)? {
                read_string(&mut bytes, 32767)?;
                take(&mut bytes, 8)?;
            }
            if version >= 763 {
                read_varint(&mut bytes)?; // portal cooldown
            }
            (dimension(world), mode)
        };
        if !bytes.is_empty() {
            return Err(invalid("trailing legacy Respawn data"));
        }
        Ok(WorldUpdate::Spawn {
            dimension,
            gamemode: GameMode::from_id(mode),
        })
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
        self.proxy_command_with(packet, &|_| false)
    }

    /// `intercepts` says whether a root command name belongs to the proxy
    /// (besides `server` and `hub`); it is asked only for command packets.
    pub(crate) fn proxy_command_with(
        self,
        packet: &Packet,
        intercepts: &dyn Fn(&str) -> bool,
    ) -> io::Result<Option<(String, Option<Packet>)>> {
        if packet.id != self.command && packet.id != self.signed_command {
            return Ok(None);
        }
        // Only the root name decides: every other command, and all chat,
        // reaches the backend without Rift reading the rest of the packet.
        let raw = read_string(&mut packet.data.as_slice(), 32767)?;
        let command = if self.protocol < 759 {
            let Some(command) = raw.strip_prefix('/') else {
                return Ok(None);
            };
            command
        } else {
            raw
        };
        let root = command.split_ascii_whitespace().next().unwrap_or("");
        if !matches!(root, "server" | "hub") && !intercepts(root) {
            return Ok(None);
        }
        Ok(self
            .parse_chat(packet)?
            .filter(|attempt| attempt.kind == ChatKind::Command && !attempt.signed)
            .map(|attempt| (attempt.text, attempt.ack)))
    }

    /// Whether this is the client's Player Session packet, which gives the
    /// backend the key to verify the player's chat with (1.19.3+).
    pub(crate) fn chat_session_update(self, packet: &Packet) -> bool {
        self.chat_session >= 0 && packet.id == self.chat_session
    }

    /// A serverbound chat message or command, checked to its last byte, or
    /// `None` for any other packet. Layouts per protocol are those of
    /// PrismarineJS minecraft-data (tests/minecraft_data.json):
    ///
    /// - before 1.19, one Chat packet holding a string, a command when it
    ///   starts with `/`;
    /// - 1.19–1.19.2, Chat Command and Chat Message with a timestamp, a salt,
    ///   variable-length signatures and a signed-preview flag, and from
    ///   1.19.1 a last-seen update (seen messages and an optional rejected one);
    /// - 1.19.3+, fixed 256-byte signatures and a last-seen update of an
    ///   offset, a 20-bit set and, from 1.21.5, a checksum byte;
    /// - 1.20.5+, also an unsigned Chat Command holding only its string.
    ///
    /// `ack` carries the last-seen update alone, as Velocity's
    /// SessionCommandHandler.consumeCommand sends it for a consumed command:
    /// the whole update in 1.19.1–1.19.2, a non-zero offset from 1.19.3.
    pub(crate) fn parse_chat(self, packet: &Packet) -> io::Result<Option<ChatAttempt>> {
        // An absent packet is -1 in the tables; no packet has that ID.
        if packet.id < 0 {
            return Ok(None);
        }
        let is_command = packet.id == self.command || packet.id == self.signed_command;
        if !is_command && packet.id != self.chat {
            return Ok(None);
        }
        let mut bytes = packet.data.as_slice();
        let raw = read_string(&mut bytes, 32767)?;
        if self.protocol < 759 {
            if !bytes.is_empty() {
                return Err(invalid("trailing command data"));
            }
            let (kind, text) = match raw.strip_prefix('/') {
                Some(command) => (ChatKind::Command, command),
                None => (ChatKind::Message, raw),
            };
            return Ok(Some(ChatAttempt {
                kind,
                text: text.to_owned(),
                signed: false,
                ack: None,
            }));
        }
        let mut attempt = ChatAttempt {
            kind: if is_command {
                ChatKind::Command
            } else {
                ChatKind::Message
            },
            text: raw.to_owned(),
            signed: false,
            ack: None,
        };
        // The unsigned Chat Command of 1.20.5+ is its string alone.
        if packet.id != self.command {
            take(&mut bytes, 16)?; // timestamp and salt
            if is_command {
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
                attempt.signed = count != 0;
            } else if self.protocol <= 760 {
                // An unsigned message has an empty signature.
                let before = bytes.len();
                signature(&mut bytes)?;
                attempt.signed = before - bytes.len() > 1;
            } else if boolean(&mut bytes)? {
                take(&mut bytes, 256)?;
                attempt.signed = true;
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
                    attempt.ack = Some(Packet::new(self.chat_acknowledgement, update.to_vec()));
                }
            } else {
                let offset = read_varint(&mut bytes)?;
                if offset < 0 {
                    return Err(invalid("invalid chat acknowledgement offset"));
                }
                take(&mut bytes, if self.chat_checksum { 4 } else { 3 })?;
                if offset != 0 {
                    let mut data = Vec::new();
                    write_varint(offset, &mut data);
                    attempt.ack = Some(Packet::new(self.chat_acknowledgement, data));
                }
            }
        }
        if !bytes.is_empty() {
            return Err(invalid(if packet.id == self.command {
                "trailing command data"
            } else if is_command {
                "trailing signed command data"
            } else {
                "trailing chat data"
            }));
        }
        Ok(Some(attempt))
    }

    /// The transaction id and full text of a Command Suggestions Request for a
    /// proxy command's arguments. Requests for backend commands, and for a root
    /// name still being typed, are not ours and must reach the backend unchanged.
    pub(crate) fn proxy_suggestion_request(
        self,
        packet: &Packet,
        intercepts: &dyn Fn(&str) -> bool,
    ) -> io::Result<Option<(i32, String)>> {
        if self.suggestion_request < 0 || packet.id != self.suggestion_request {
            return Ok(None);
        }
        let mut bytes = packet.data.as_slice();
        let id = read_varint(&mut bytes)?;
        let text = read_string(&mut bytes, 32767)?;
        if !bytes.is_empty() {
            return Err(invalid("trailing command suggestion data"));
        }
        // Chat input keeps its slash; the command block editor has none.
        let input = text.strip_prefix('/').unwrap_or(text);
        let Some((root, _)) = input.split_once(' ') else {
            return Ok(None);
        };
        if !matches!(root, "server" | "hub") && !intercepts(root) {
            return Ok(None);
        }
        Ok(Some((id, text.to_owned())))
    }

    /// Command Suggestions Response. `start` and `length` select the replaced
    /// range of the request text in UTF-16 code units, as Brigadier counts.
    pub(crate) fn suggestions(
        self,
        version: ProtocolVersion,
        id: i32,
        start: usize,
        length: usize,
        matches: &[Suggestion],
    ) -> io::Result<Packet> {
        if self.suggestion_response < 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Command suggestions require Minecraft 1.19.3 or newer.",
            ));
        }
        let range = |value: usize| {
            i32::try_from(value).map_err(|_| invalid("suggestion range out of bounds"))
        };
        let mut data = Vec::new();
        write_varint(id, &mut data);
        write_varint(range(start)?, &mut data);
        write_varint(range(length)?, &mut data);
        write_varint(range(matches.len())?, &mut data);
        for suggestion in matches {
            write_string(&suggestion.text, &mut data);
            match &suggestion.tooltip {
                Some(tooltip) => {
                    data.push(1);
                    // JSON text before 1.20.3, anonymous network NBT since.
                    with_text(tooltip, Some(version), |text| {
                        text.write(version.nbt_components(), &mut data)
                    })?;
                }
                None => data.push(0),
            }
        }
        Ok(Packet::new(self.suggestion_response, data))
    }

    pub(crate) fn system_message(
        self,
        version: ProtocolVersion,
        message: Text,
    ) -> io::Result<Packet> {
        self.chat(version, message, false)
    }

    pub(crate) fn action_bar(self, version: ProtocolVersion, message: Text) -> io::Result<Packet> {
        self.chat(version, message, true)
    }

    fn chat(self, version: ProtocolVersion, message: Text, overlay: bool) -> io::Result<Packet> {
        // Disconnect and system chat share the version's component encoding.
        let mut packet = disconnect_text(Some(version), State::Play, message)?;
        packet.id = self.system_chat;
        // Through 1.19, position 1 is system text and 2 the action bar (a byte
        // before 1.19, then a VarInt). 1.19.1 replaces it with an overlay boolean.
        packet.data.push(if self.protocol <= 759 {
            1 + u8::from(overlay)
        } else {
            u8::from(overlay)
        });
        if (735..759).contains(&self.protocol) {
            packet.data.extend([0; 16]);
        }
        Ok(packet)
    }

    /// Set Titles Animation, Set Subtitle Text and Set Title Text, in the order
    /// the client needs to display them. Times are ticks. An absent subtitle is
    /// sent empty so that one from an earlier title cannot reappear. Before
    /// 1.17 the three are actions of one Title packet.
    pub(crate) fn title(
        self,
        version: ProtocolVersion,
        title: Option<&crate::text::Text>,
        subtitle: Option<&crate::text::Text>,
        times: [i32; 3],
    ) -> io::Result<Vec<Packet>> {
        // minecraft-data protocol.json for 1.8 through 26.1; 26.2 and 26.3
        // GameProtocols via javap.
        let ids = match self.protocol {
            // Title actions: 0 title, 1 subtitle, then times (2 before 1.11,
            // 3 once 1.11 inserted the action bar).
            47 | 107..=110 | 210 => TitleIds::Combined(0x45, 2),
            315 | 316 => TitleIds::Combined(0x45, 3),
            335 => TitleIds::Combined(0x47, 3),
            338 | 340 => TitleIds::Combined(0x48, 3),
            393 | 401 | 404 => TitleIds::Combined(0x4b, 3),
            477..=498 | 735 | 736 | 751 | 753 | 754 => TitleIds::Combined(0x4f, 3),
            573..=578 => TitleIds::Combined(0x50, 3),
            755 | 756 => TitleIds::Separate(0x57, 0x59, 0x5a),
            757..=759 => TitleIds::Separate(0x58, 0x5a, 0x5b),
            760 => TitleIds::Separate(0x5b, 0x5d, 0x5e),
            761 => TitleIds::Separate(0x59, 0x5b, 0x5c),
            762 | 763 => TitleIds::Separate(0x5d, 0x5f, 0x60),
            764 => TitleIds::Separate(0x5f, 0x61, 0x62),
            765 => TitleIds::Separate(0x61, 0x63, 0x64),
            766 | 767 => TitleIds::Separate(0x63, 0x65, 0x66),
            768 | 769 => TitleIds::Separate(0x6a, 0x6c, 0x6d),
            770..=772 => TitleIds::Separate(0x69, 0x6b, 0x6c),
            773 | 774 => TitleIds::Separate(0x6e, 0x70, 0x71),
            775 | 776 => TitleIds::Separate(0x70, 0x72, 0x73),
            777 => TitleIds::Separate(0x72, 0x74, 0x75),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Titles require Minecraft 1.8.9–26.3.",
                ));
            }
        };
        // Text as the client reads it; an absent one is empty.
        let text = |text: Option<&crate::text::Text>, data: &mut Vec<u8>| match text {
            Some(text) => with_text(text, Some(version), |text| {
                text.write(version.nbt_components(), data)
            }),
            None => Text::Literal("").write(version.nbt_components(), data),
        };
        let timing: Vec<u8> = times.iter().flat_map(|t| t.to_be_bytes()).collect();
        match ids {
            TitleIds::Separate(subtitle_id, title_id, times_id) => {
                let packet = |id, value| {
                    let mut data = Vec::new();
                    text(value, &mut data)?;
                    Ok::<_, io::Error>(Packet::new(id, data))
                };
                Ok(vec![
                    Packet::new(times_id, timing),
                    packet(subtitle_id, subtitle)?,
                    packet(title_id, title)?,
                ])
            }
            TitleIds::Combined(id, times_action) => {
                let action = |action: i32, body: &dyn Fn(&mut Vec<u8>) -> io::Result<()>| {
                    let mut data = Vec::new();
                    write_varint(action, &mut data);
                    body(&mut data)?;
                    Ok::<_, io::Error>(Packet::new(id, data))
                };
                Ok(vec![
                    action(times_action, &|data| {
                        data.extend(&timing);
                        Ok(())
                    })?,
                    action(1, &|data| text(subtitle, data))?,
                    action(0, &|data| text(title, data))?,
                ])
            }
        }
    }

    /// Append unsigned Brigadier proxy literals to the backend's command
    /// tree. Existing node indexes and redirects stay valid; only root children
    /// with the same names are replaced. A brigadier:string argument is deliberately
    /// used instead of minecraft:message, which would require a signed argument.
    /// Each command is a name and whether its arguments ask the server for
    /// suggestions; `/server <name>` always does where Rift can answer.
    #[cfg(test)]
    pub(crate) fn network_commands(self, packet: &Packet) -> io::Result<Packet> {
        self.network_commands_with(packet, &[])
    }

    pub(crate) fn network_commands_with(
        self,
        packet: &Packet,
        commands: &[(String, bool)],
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
                            .any(|(name, _)| Some(name.as_str()) == nodes[*child as usize].name)
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
        let suggestions = self.suggestion_response >= 0;
        data.push(5);
        write_varint(1, &mut data);
        write_varint(count + 1, &mut data);
        write_string("server", &mut data);
        self.string_argument("name", suggestions, &mut data);
        data.push(5);
        write_varint(0, &mut data);
        write_string("hub", &mut data);
        for (i, (name, suggest)) in commands.iter().enumerate() {
            data.push(5); // executable literal, optional greedy unsigned string
            write_varint(1, &mut data);
            write_varint(count + 4 + i as i32 * 2, &mut data);
            write_string(name, &mut data);
            self.string_argument("args", suggestions && *suggest, &mut data);
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
