//! Player forwarding: how a backend in offline mode learns the player's
//! Mojang UUID, skin and address from Rift.
//!
//! Velocity modern forwarding answers a login plugin request with an HMAC over
//! the complete profile payload; neither client Login Start nor client plugin
//! responses provide it. Protocol reference: PaperMC/Velocity
//! PlayerDataForwarding and LoginSessionHandler.
//!
//! BungeeCord forwarding appends the address, undashed UUID and a JSON
//! property list to the handshake hostname, NUL-separated. BungeeGuard adds a
//! `bungeeguard-token` property the backend plugin checks against its allowed
//! tokens. References: SpigotMC BungeeCord ServerConnector and
//! lucko/BungeeGuard.
use crate::{
    auth::AuthenticatedProfile,
    protocol::{
        self, Packet, ProfileKey, ProtocolVersion, read_string, read_varint, write_string,
        write_varint,
    },
};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::{collections::HashSet, io, net::IpAddr, sync::Arc};

pub(crate) const CHANNEL: &str = "velocity:player_info";
const BUNGEEGUARD_PROPERTY: &str = "bungeeguard-token";
/// Spigot and Paper read a forwarded hostname of up to `Short.MAX_VALUE` characters.
const MAX_FORWARDED_HOST: usize = 32767;

/// How the player's identity reaches backends.
#[derive(Clone)]
pub(crate) enum Method {
    Velocity { secret: Arc<[u8]> },
    BungeeCord,
    BungeeGuard { token: Arc<str> },
}

#[derive(Clone)]
pub(crate) struct Forwarding {
    method: Method,
    address: IpAddr,
    profile: AuthenticatedProfile,
    /// A 1.19–1.19.2 client's chat key, forwarded to backends that ask for it.
    key: Option<ProfileKey>,
    version: ProtocolVersion,
}

impl Forwarding {
    pub(crate) fn new(
        method: Method,
        address: IpAddr,
        profile: AuthenticatedProfile,
        key: Option<ProfileKey>,
        version: ProtocolVersion,
    ) -> io::Result<Self> {
        match &method {
            Method::Velocity { secret } if secret.is_empty() => {
                return Err(protocol::invalid(
                    "Velocity forwarding secret must not be empty",
                ));
            }
            Method::BungeeGuard { token } if token.is_empty() => {
                return Err(protocol::invalid("BungeeGuard token must not be empty"));
            }
            _ => {}
        }
        Ok(Self {
            method,
            address,
            profile,
            key,
            version,
        })
    }

    pub(crate) fn velocity(&self) -> bool {
        matches!(self.method, Method::Velocity { .. })
    }

    /// The same forwarding for a backend on another protocol than the
    /// player's. The player's 1.19 chat key is signed for the player's own
    /// protocol and message format, so a translated backend gets none.
    pub(crate) fn for_backend(&self, version: ProtocolVersion) -> Self {
        Self {
            key: None,
            version,
            ..self.clone()
        }
    }

    /// The oldest backend protocol this forwarding reaches.
    pub(crate) fn minimum_protocol(&self) -> i32 {
        if self.velocity() { 393 } else { 0 }
    }

    /// The handshake hostname a BungeeCord-forwarded backend reads, or `None`
    /// for Velocity forwarding. `address` is the client's handshake address:
    /// its hostname, then any Forge marker, which travels as Bungee's
    /// `forgeClient` and `extraData` properties.
    pub(crate) fn handshake_address(&self, address: &str) -> io::Result<Option<String>> {
        let token = match &self.method {
            Method::Velocity { .. } => return Ok(None),
            Method::BungeeCord => None,
            Method::BungeeGuard { token } => Some(token),
        };
        let (host, marker) = address.split_at(address.find('\0').unwrap_or(address.len()));
        let mut properties: Vec<serde_json::Value> = self
            .profile
            .properties
            .iter()
            .map(|property| {
                let mut value = serde_json::json!({
                    "name": property.name,
                    "value": property.value,
                });
                if let Some(signature) = &property.signature {
                    value["signature"] = signature.as_str().into();
                }
                value
            })
            // A Mojang profile never carries the token; drop any lookalike.
            .filter(|property| property["name"] != BUNGEEGUARD_PROPERTY)
            .collect();
        if !marker.is_empty() {
            properties.push(serde_json::json!({"name": "forgeClient", "value": "true"}));
            properties.push(serde_json::json!({
                "name": "extraData",
                "value": marker.replace('\0', "\u{1}"),
                "signature": "",
            }));
        }
        if let Some(token) = token {
            properties.push(serde_json::json!({
                "name": BUNGEEGUARD_PROPERTY,
                "value": token.as_ref(),
                "signature": "",
            }));
        }
        let uuid: String = self
            .profile
            .uuid
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let mut forwarded = format!("{host}\0{}\0{uuid}", self.address);
        if !properties.is_empty() {
            forwarded.push('\0');
            forwarded.push_str(&serde_json::Value::Array(properties).to_string());
        }
        if forwarded.encode_utf16().count() > MAX_FORWARDED_HOST {
            return Err(protocol::invalid(
                "The player's profile is too large for BungeeCord forwarding.",
            ));
        }
        Ok(Some(forwarded))
    }

    fn response(&self, id: i32, request: &[u8]) -> io::Result<Packet> {
        let Method::Velocity { secret } = &self.method else {
            return Err(protocol::invalid(
                "Backend requested Velocity forwarding, but Rift uses BungeeCord forwarding.",
            ));
        };
        // Original implementations sent an empty request (version 1). Current
        // Paper sends one byte denoting its maximum supported version.
        let requested = match request {
            [] => 1,
            [version @ 1..=127] => *version,
            _ => {
                return Err(protocol::invalid(
                    "invalid Velocity forwarding version request",
                ));
            }
        };
        let version = self.forwarding_version(requested);
        let mut payload = Vec::new();
        write_varint(version, &mut payload);
        write_string(&self.address.to_string(), &mut payload);
        payload.extend_from_slice(&self.profile.uuid);
        write_string(&self.profile.name, &mut payload);
        write_properties(&self.profile, &mut payload);
        if matches!(version, 2 | 3) {
            // Only chosen with a key; see forwarding_version.
            self.key.as_ref().unwrap().write(&mut payload);
            if version == 3 {
                // The key's signature covers the Mojang UUID it belongs to.
                payload.push(1);
                payload.extend_from_slice(&self.profile.uuid);
            }
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(secret)
            .map_err(|_| protocol::invalid("invalid Velocity forwarding secret"))?;
        mac.update(&payload);
        let mut data = Vec::with_capacity(payload.len() + 38);
        write_varint(id, &mut data);
        data.push(1);
        data.extend_from_slice(&mac.finalize().into_bytes());
        data.extend_from_slice(&payload);
        Ok(Packet::new(2, data))
    }

    /// Velocity's choice: 1 is the plain profile, 2 adds a 1.19 chat key,
    /// 3 a 1.19.1–1.19.2 key with its owner, and 4 tells a 1.19.3+ backend
    /// that the key arrives later in play.
    fn forwarding_version(&self, requested: u8) -> i32 {
        if requested < 2 {
            return 1;
        }
        match (self.version.number(), &self.key) {
            (761.., _) if requested >= 4 => 4,
            (759, Some(_)) => 2,
            (760, Some(_)) if requested >= 3 => 3,
            _ => 1,
        }
    }
}

/// Whether a Login Plugin Request asks for Velocity forwarding. The layout is
/// the same in every protocol that has the packet.
pub(crate) fn velocity_request(packet: &Packet) -> bool {
    let mut bytes = packet.data.as_slice();
    read_varint(&mut bytes).is_ok()
        && read_string(&mut bytes, 32767).is_ok_and(|channel| channel == CHANNEL)
}

/// Query state belongs to one backend attachment, including across retries.
/// The client may answer only queries actually relayed to it, at most once.
#[derive(Default)]
pub(crate) struct LoginPlugins {
    seen: HashSet<i32>,
    client_pending: HashSet<i32>,
    forwarded: bool,
}

impl LoginPlugins {
    pub(crate) fn request(
        &mut self,
        packet: &Packet,
        forwarding: Option<&Forwarding>,
    ) -> io::Result<Option<Packet>> {
        let mut bytes = packet.data.as_slice();
        let id = read_varint(&mut bytes)?;
        let channel = read_string(&mut bytes, 32767)?;
        // Paper generates transaction IDs with ThreadLocalRandom.nextInt():
        // every signed i32 (including negative values) is a valid opaque ID.
        if self.seen.len() >= 1024 || !self.seen.insert(id) {
            return Err(protocol::invalid(
                "invalid or duplicate login plugin request",
            ));
        }
        if channel == CHANNEL {
            if self.forwarded {
                return Err(protocol::invalid("duplicate Velocity forwarding request"));
            }
            let forwarding = forwarding.ok_or_else(|| {
                protocol::invalid(
                    "Backend requires Velocity forwarding, but forwarding is not enabled in Rift.",
                )
            })?;
            let response = forwarding.response(id, bytes)?;
            self.forwarded = true;
            Ok(Some(response))
        } else {
            self.client_pending.insert(id);
            Ok(None)
        }
    }

    pub(crate) fn client_response(&mut self, packet: &Packet) -> io::Result<()> {
        let mut bytes = packet.data.as_slice();
        let id = read_varint(&mut bytes)?;
        if !self.client_pending.remove(&id) {
            return Err(protocol::invalid("unsolicited login plugin response"));
        }
        match bytes.split_first() {
            Some((&0, [])) | Some((&1, _)) => Ok(()),
            _ => Err(protocol::invalid("invalid login plugin response")),
        }
    }

    /// Velocity forwarding must have happened by Login Success. BungeeCord
    /// forwarding has no reply; the identity check catches a backend that
    /// ignored it.
    pub(crate) fn require_forwarded(&self, forwarding: Option<&Forwarding>) -> io::Result<()> {
        if forwarding.is_some_and(Forwarding::velocity) && !self.forwarded {
            return Err(protocol::invalid(
                "Backend did not request Velocity forwarding. Enable Velocity modern forwarding on Paper.",
            ));
        }
        Ok(())
    }
}

fn write_properties(profile: &AuthenticatedProfile, data: &mut Vec<u8>) {
    write_varint(profile.properties.len() as i32, data);
    for property in &profile.properties {
        write_string(&property.name, data);
        write_string(&property.value, data);
        data.push(u8::from(property.signature.is_some()));
        if let Some(signature) = &property.signature {
            write_string(signature, data);
        }
    }
}

/// The Login Start a backend receives for an authenticated player. 1.19 and
/// 1.19.1–1.19.2 carry the client's chat key, as Velocity sends it; the claimed
/// UUID becomes the Mojang one.
pub(crate) fn login_start(
    profile: &AuthenticatedProfile,
    version: ProtocolVersion,
    key: Option<&ProfileKey>,
) -> Packet {
    let mut data = Vec::new();
    write_string(&profile.name, &mut data);
    let number = version.number();
    if matches!(number, 759..=760) {
        data.push(u8::from(key.is_some()));
        if let Some(key) = key {
            key.write(&mut data);
        }
    }
    if number >= 760 {
        if number < 764 {
            data.push(1);
        }
        data.extend_from_slice(&profile.uuid);
    }
    Packet::new(0, data)
}

/// Retain backend session metadata, but use only Mojang's verified player
/// identity/properties in the client-facing Login Success.
pub(crate) fn login_success(
    profile: &AuthenticatedProfile,
    version: ProtocolVersion,
    packet: &Packet,
) -> io::Result<Packet> {
    protocol::success_identity(version, packet)?;
    let mut data = Vec::new();
    if version.number() < 735 {
        // Before 1.16 the UUID is a dashed string.
        let hex: String = profile
            .uuid
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        write_string(
            &format!(
                "{}-{}-{}-{}-{}",
                &hex[..8],
                &hex[8..12],
                &hex[12..16],
                &hex[16..20],
                &hex[20..]
            ),
            &mut data,
        );
    } else {
        data.extend_from_slice(&profile.uuid);
    }
    write_string(&profile.name, &mut data);
    if version.number() >= 759 {
        write_properties(profile, &mut data);
        let suffix = if version.number() >= 776 {
            16
        } else if matches!(version.number(), 766..=767) {
            1
        } else {
            0
        };
        data.extend_from_slice(&packet.data[packet.data.len() - suffix..]);
    }
    Ok(Packet::new(packet.id, data))
}

#[cfg(test)]
mod tests;
