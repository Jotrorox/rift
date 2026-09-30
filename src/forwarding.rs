//! Velocity modern forwarding wire format. The MAC covers the complete profile
//! payload; neither client Login Start nor client plugin responses provide it.
//! Protocol reference: PaperMC/Velocity PlayerDataForwarding and LoginSessionHandler.
use crate::{
    auth::AuthenticatedProfile,
    protocol::{
        self, Packet, ProtocolVersion, read_string, read_varint, write_string, write_varint,
    },
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::{collections::HashSet, io, net::IpAddr, sync::Arc};

pub(crate) const CHANNEL: &str = "velocity:player_info";

#[derive(Clone)]
pub(crate) struct Forwarding {
    secret: Arc<[u8]>,
    address: IpAddr,
    profile: AuthenticatedProfile,
}

impl Forwarding {
    pub(crate) fn new(
        secret: Arc<[u8]>,
        address: IpAddr,
        profile: AuthenticatedProfile,
    ) -> io::Result<Self> {
        if secret.is_empty() {
            return Err(protocol::invalid(
                "Velocity forwarding secret must not be empty",
            ));
        }
        Ok(Self {
            secret,
            address,
            profile,
        })
    }

    fn response(&self, id: i32, request: &[u8]) -> io::Result<Packet> {
        // Original implementations sent an empty request (version 1). Current
        // Paper sends one byte denoting its maximum supported version. Versions
        // 2/3 carried 1.19 player keys; 1.19.3+ uses version 4 or falls back to 1.
        let requested = match request {
            [] => 1,
            [version @ 1..=127] => *version,
            _ => {
                return Err(protocol::invalid(
                    "invalid Velocity forwarding version request",
                ));
            }
        };
        let mut payload = Vec::new();
        write_varint(if requested >= 4 { 4 } else { 1 }, &mut payload);
        write_string(&self.address.to_string(), &mut payload);
        payload.extend_from_slice(&self.profile.uuid);
        write_string(&self.profile.name, &mut payload);
        write_properties(&self.profile, &mut payload);
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret)
            .map_err(|_| protocol::invalid("invalid Velocity forwarding secret"))?;
        mac.update(&payload);
        let mut data = Vec::with_capacity(payload.len() + 38);
        write_varint(id, &mut data);
        data.push(1);
        data.extend_from_slice(&mac.finalize().into_bytes());
        data.extend_from_slice(&payload);
        Ok(Packet::new(2, data))
    }
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
                    "Backend requires Velocity forwarding, but it is not enabled in Rift.",
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

    pub(crate) fn require_forwarded(&self, enabled: bool) -> io::Result<()> {
        if enabled && !self.forwarded {
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

pub(crate) fn login_start(profile: &AuthenticatedProfile, version: ProtocolVersion) -> Packet {
    let mut data = Vec::new();
    write_string(&profile.name, &mut data);
    if version.number() >= 761 {
        if version.number() < 764 {
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
    if version.number() == 47 {
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
    if version.number() >= 761 {
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
