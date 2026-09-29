// Control packet layouts are checked against PrismarineJS minecraft-data:
// https://github.com/PrismarineJS/minecraft-data/tree/master/data/pc
// Protocol 777 uses the repository's pinned Pumpkin fixture:
// https://github.com/Pumpkin-MC/Pumpkin/blob/204a94ed895f041a845630d5a23c094705a0704e/crates/pumpkin-data/src/generated/packet.rs
use super::{
    Direction, Packet, State, invalid, read_string, read_varint, write_string, write_varint,
};
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NextState {
    Status,
    Login,
    Transfer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handshake {
    pub protocol: i32,
    /// Includes any NUL-delimited mod metadata, for forwarding.
    pub address: String,
    pub port: u16,
    pub next_state: NextState,
}

impl Handshake {
    pub fn decode(packet: &Packet) -> io::Result<Self> {
        if packet.id != 0 {
            return Err(invalid("expected handshake"));
        }
        let mut bytes = packet.data.as_slice();
        let protocol = read_varint(&mut bytes)?;
        let address = read_string(&mut bytes, 255)?.to_owned();
        if address
            .split('\0')
            .next()
            .unwrap()
            .trim_end_matches('.')
            .is_empty()
        {
            return Err(invalid("empty handshake hostname"));
        }
        let port = u16::from_be_bytes(
            bytes
                .get(..2)
                .ok_or_else(|| invalid("missing handshake port"))?
                .try_into()
                .unwrap(),
        );
        bytes = &bytes[2..];
        let next_state = match read_varint(&mut bytes)? {
            1 => NextState::Status,
            2 => NextState::Login,
            3 => NextState::Transfer,
            _ => return Err(invalid("invalid handshake next state")),
        };
        if !bytes.is_empty() {
            return Err(invalid("trailing handshake data"));
        }
        Ok(Self {
            protocol,
            address,
            port,
            next_state,
        })
    }

    pub fn hostname(&self) -> String {
        let host = self.address.split('\0').next().unwrap();
        host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase()
    }

    pub fn packet(&self) -> Packet {
        let mut data = Vec::new();
        write_varint(self.protocol, &mut data);
        write_string(&self.address, &mut data);
        data.extend_from_slice(&self.port.to_be_bytes());
        write_varint(
            match self.next_state {
                NextState::Status => 1,
                NextState::Login => 2,
                NextState::Transfer => 3,
            },
            &mut data,
        );
        Packet::new(0, data)
    }
}

/// Only explicitly mapped versions may enter login. Status discovery also works
/// with unknown versions (including -1). No packet IDs are guessed for snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtocolVersion(i32);

impl ProtocolVersion {
    pub fn new(protocol: i32) -> io::Result<Self> {
        match protocol {
            47 | 761..=775 | 777 => Ok(Self(protocol)),
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Unsupported Minecraft version. Rift supports 1.8, 1.19.3–26.1 and protocol 777.",
            )),
        }
    }
    pub fn number(self) -> i32 {
        self.0
    }
    /// Switching is an explicit capability, independent of relay support.
    pub fn supports_switching(self) -> bool {
        self.switching().is_some()
    }
    pub(crate) fn switching(self) -> Option<super::switching::SwitchingCapabilities> {
        super::switching::for_version(self)
    }
    pub fn has_configuration(self) -> bool {
        self.0 >= 764
    }
    pub fn has_transfer(self) -> bool {
        self.0 >= 766
    }
    pub fn nbt_components(self) -> bool {
        self.0 >= 765
    }
    fn config_finish(self) -> i32 {
        if self.0 < 766 { 2 } else { 3 }
    }
    fn config_disconnect(self) -> i32 {
        if self.0 < 766 { 1 } else { 2 }
    }
    fn play_disconnect(self) -> i32 {
        match self.0 {
            47 => 0x40,
            761 => 0x17,
            762..=763 => 0x1a,
            764..=765 => 0x1b,
            766..=769 => 0x1d,
            770..=772 => 0x1c,
            _ => 0x20,
        }
    }
    pub fn start_configuration(self) -> Option<i32> {
        match self.0 {
            764 => Some(0x65),
            765 => Some(0x67),
            766..=767 => Some(0x69),
            768..=769 => Some(0x70),
            770..=772 => Some(0x6f),
            773..=774 => Some(0x74),
            775 => Some(0x76),
            777 => Some(0x78),
            _ => None,
        }
    }
    pub fn configuration_acknowledged(self) -> Option<i32> {
        match self.0 {
            764..=765 => Some(0x0b),
            766..=767 => Some(0x0c),
            768..=770 => Some(0x0e),
            771..=774 => Some(0x0f),
            775 | 777 => Some(0x10),
            _ => None,
        }
    }
    pub fn kind(self, state: State, direction: Direction, id: i32) -> PacketKind {
        use Direction::*;
        use PacketKind::*;
        match (state, direction, id) {
            (State::Handshake, Serverbound, 0) => Handshake,
            (State::Status, Serverbound, 0) => StatusRequest,
            (State::Status, Clientbound, 0) => StatusResponse,
            (State::Status, _, 1) => Ping,
            (State::Login, Serverbound, 0) => LoginStart,
            (State::Login, Serverbound, 1) => EncryptionResponse,
            (State::Login, Serverbound, 2) if self.0 >= 393 => LoginPluginResponse,
            (State::Login, Serverbound, 3) if self.has_configuration() => LoginAcknowledged,
            (State::Login, Serverbound, 4) if self.has_transfer() => CookieResponse,
            (State::Login, Clientbound, 0) => Disconnect,
            (State::Login, Clientbound, 1) => EncryptionRequest,
            (State::Login, Clientbound, 2) => LoginSuccess,
            (State::Login, Clientbound, 3) => SetCompression,
            (State::Login, Clientbound, 4) if self.0 >= 393 => LoginPluginRequest,
            (State::Login, Clientbound, 5) if self.has_transfer() => CookieRequest,
            (State::Configuration, Clientbound, id) if id == self.config_disconnect() => Disconnect,
            (State::Configuration, _, id) if id == self.config_finish() => FinishConfiguration,
            (State::Play, Clientbound, id)
                if self.switching().is_some_and(|caps| id == caps.join_game) =>
            {
                JoinGame
            }
            (State::Play, Clientbound, id) if id == self.play_disconnect() => Disconnect,
            (State::Play, Clientbound, id) if Some(id) == self.start_configuration() => {
                StartConfiguration
            }
            (State::Play, Serverbound, id) if Some(id) == self.configuration_acknowledged() => {
                ConfigurationAcknowledged
            }
            (State::Play, Clientbound, 0x46) if self.0 == 47 => SetCompression,
            _ => Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketKind {
    Handshake,
    StatusRequest,
    StatusResponse,
    Ping,
    LoginStart,
    EncryptionRequest,
    EncryptionResponse,
    LoginSuccess,
    JoinGame,
    SetCompression,
    LoginAcknowledged,
    LoginPluginRequest,
    LoginPluginResponse,
    CookieRequest,
    CookieResponse,
    FinishConfiguration,
    StartConfiguration,
    ConfigurationAcknowledged,
    Disconnect,
    Unknown,
}

pub fn disconnect(
    version: Option<ProtocolVersion>,
    state: State,
    reason: &str,
) -> io::Result<Packet> {
    let id = match state {
        State::Login => 0,
        State::Configuration => version
            .ok_or_else(|| invalid("unknown version"))?
            .config_disconnect(),
        State::Play => version
            .ok_or_else(|| invalid("unknown version"))?
            .play_disconnect(),
        _ => return Err(invalid("cannot disconnect in this protocol state")),
    };
    let mut data = Vec::new();
    if state != State::Login && version.is_some_and(ProtocolVersion::nbt_components) {
        // Anonymous NBT TAG_String is a valid literal text component. Encode Java
        // modified UTF-8, including supplementary characters as surrogate pairs.
        let mut text = Vec::new();
        for unit in reason.encode_utf16() {
            match unit {
                1..=0x7f => text.push(unit as u8),
                0..=0x7ff => text.extend([0xc0 | (unit >> 6) as u8, 0x80 | (unit & 63) as u8]),
                _ => text.extend([
                    0xe0 | (unit >> 12) as u8,
                    0x80 | ((unit >> 6) & 63) as u8,
                    0x80 | (unit & 63) as u8,
                ]),
            }
        }
        let length =
            u16::try_from(text.len()).map_err(|_| invalid("disconnect reason too long"))?;
        data.push(8);
        data.extend_from_slice(&length.to_be_bytes());
        data.extend(text);
    } else {
        write_string(&serde_json::json!({"text": reason}).to_string(), &mut data);
    }
    Ok(Packet::new(id, data))
}

pub fn status_response(protocol: i32, online: u64, maximum: usize, description: &str) -> Packet {
    let response = serde_json::json!({
        "version": {"name": "Rift", "protocol": protocol},
        "players": {"max": maximum, "online": online},
        "description": {"text": description},
        "enforcesSecureChat": false,
    });
    let mut data = Vec::new();
    write_string(&response.to_string(), &mut data);
    Packet::new(0, data)
}

/// Validate version-specific login success fields and retain the UUID/name
/// identity independently of backend-specific profile properties.
pub(crate) fn login_identity(version: ProtocolVersion, packet: &Packet) -> io::Result<Vec<u8>> {
    let mut bytes = packet.data.as_slice();
    if version.number() == 47 {
        let uuid = read_string(&mut bytes, 36)?;
        if uuid.len() != 36 {
            return Err(invalid("invalid login UUID"));
        }
    } else {
        bytes = bytes
            .get(16..)
            .ok_or_else(|| invalid("missing login UUID"))?;
    }
    if read_string(&mut bytes, 16)?.is_empty() {
        return Err(invalid("empty player name"));
    }
    let identity = packet.data[..packet.data.len() - bytes.len()].to_vec();
    if version.number() >= 761 {
        let count = read_varint(&mut bytes)?;
        // Every property has at least two string lengths and a signed flag.
        if count < 0 || count as usize > bytes.len() / 3 {
            return Err(invalid("invalid profile property count"));
        }
        for _ in 0..count {
            read_string(&mut bytes, 32767)?;
            read_string(&mut bytes, 32767)?;
            match bytes.split_first() {
                Some((&0, rest)) => bytes = rest,
                Some((&1, rest)) => {
                    bytes = rest;
                    read_string(&mut bytes, 32767)?;
                }
                _ => return Err(invalid("invalid profile signature flag")),
            }
        }
        // 26.2 adds a backend session UUID after the profile properties.
        // It is not part of the player's identity across backend connections.
        if version.number() >= 777 {
            bytes = bytes
                .get(16..)
                .ok_or_else(|| invalid("missing login session UUID"))?;
        }
        if matches!(version.number(), 766..=767) {
            match bytes.split_first() {
                Some((&(0 | 1), rest)) => bytes = rest,
                _ => return Err(invalid("invalid strict error handling flag")),
            }
        }
    }
    if !bytes.is_empty() {
        return Err(invalid("trailing login success data"));
    }
    Ok(identity)
}

/// Identity used for registry bookkeeping. A Login Start UUID is only a client
/// claim in offline mode; access grants must not trust it as authentication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerIdentity {
    pub name: String,
    pub uuid: Option<[u8; 16]>,
}

pub(crate) fn start_identity(
    version: ProtocolVersion,
    packet: &Packet,
) -> io::Result<PlayerIdentity> {
    let mut bytes = packet.data.as_slice();
    let name = read_string(&mut bytes, 16)?.to_owned();
    let uuid = if version.number() >= 764 || (version.number() >= 761 && boolean(&mut bytes)?) {
        Some(take(&mut bytes, 16)?.try_into().unwrap())
    } else {
        None
    };
    if name.is_empty() || !bytes.is_empty() {
        return Err(invalid("invalid login identity"));
    }
    Ok(PlayerIdentity { name, uuid })
}

pub(crate) fn success_identity(
    version: ProtocolVersion,
    packet: &Packet,
) -> io::Result<PlayerIdentity> {
    login_identity(version, packet)?;
    let mut bytes = packet.data.as_slice();
    let uuid = if version.number() == 47 {
        let value = read_string(&mut bytes, 36)?.replace('-', "");
        if value.len() != 32 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid("invalid login UUID"));
        }
        let mut uuid = [0; 16];
        for (i, byte) in uuid.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&value[2 * i..2 * i + 2], 16)
                .map_err(|_| invalid("invalid login UUID"))?;
        }
        Some(uuid)
    } else {
        Some(take(&mut bytes, 16)?.try_into().unwrap())
    };
    Ok(PlayerIdentity {
        name: read_string(&mut bytes, 16)?.to_owned(),
        uuid,
    })
}

pub(super) fn take<'a>(bytes: &mut &'a [u8], size: usize) -> io::Result<&'a [u8]> {
    let result = bytes.get(..size).ok_or_else(|| invalid("short packet"))?;
    *bytes = &bytes[size..];
    Ok(result)
}
pub(super) fn boolean(bytes: &mut &[u8]) -> io::Result<bool> {
    match take(bytes, 1)?[0] {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid("invalid boolean")),
    }
}

/// Encode a network command response only for an explicitly switchable version.
pub fn system_message(version: ProtocolVersion, message: &str) -> io::Result<Packet> {
    version
        .switching()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "Network commands are unavailable for this Minecraft version.",
            )
        })?
        .system_message(version, message)
}
